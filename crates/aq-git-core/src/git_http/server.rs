use super::{
    GitHttpLimits, Grant, MAIN_REF, TASK_PREFIX, TRY_PREFIX,
    hook::{GRANT_ENV, HOOK_MARKER},
    http::{self, ChunkedWriter, Head, HttpError},
};
use crate::{Error, Result, git::DeadlineScope, service::RepositoryService};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

type Out = UnixStream;

struct RequestBody<'a> {
    head: &'a Head,
    reader: &'a mut std::io::BufReader<UnixStream>,
}

enum PackRpc<'a> {
    Advertise(&'a str),
    Transfer(&'a str),
}

impl<'a> PackRpc<'a> {
    fn service(&self) -> &'a str {
        match self {
            Self::Advertise(name) | Self::Transfer(name) => name,
        }
    }
}

/// The running Git smart-HTTP endpoint. Dropping it does not stop it; call [`stop`](Self::stop).
pub struct GitHttpServer {
    stop: Arc<AtomicBool>,
    path: PathBuf,
    thread: Option<JoinHandle<()>>,
    /// The periodic `git gc --auto` thread; dropping the sender wakes and ends it.
    gc: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}
struct Shared {
    service: Arc<RepositoryService>,
    limits: GitHttpLimits,
    hooks: PathBuf,
    active: AtomicUsize,
}
impl GitHttpServer {
    /// Serves `listener` until [`stop`](Self::stop). `exe` is this program's own
    /// executable, which the installed hooks re-run (see [`super::run_hook_if_requested`]).
    pub fn start(
        service: Arc<RepositoryService>,
        listener: UnixListener,
        exe: PathBuf,
    ) -> Result<Self> {
        let limits = GitHttpLimits::load(&service)?;
        let hooks = service.root.create_dir("git-http/hooks")?;
        let quoted = exe.to_string_lossy().replace('\'', "'\\''");
        let script = format!("#!/bin/sh\nexec '{quoted}' {HOOK_MARKER} pre-receive\n");
        let temporary = hooks.join(".pre-receive.tmp");
        std::fs::write(&temporary, script)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))?;
        std::fs::rename(&temporary, hooks.join("pre-receive"))?;
        // An earlier version also installed a post-receive hook.
        let _ = std::fs::remove_file(hooks.join("post-receive"));
        let path = match listener.local_addr()?.as_pathname() {
            Some(path) => path.to_path_buf(),
            None => return Err(Error::new("invalid_configuration", "unnamed socket")),
        };
        let gc = (limits.gc_interval_secs > 0)
            .then(|| start_gc(service.clone(), limits.clone()))
            .transpose()?;
        let shared = Arc::new(Shared {
            service,
            limits,
            hooks,
            active: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("git-http-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let shared = shared.clone();
                    if shared.active.fetch_add(1, Ordering::SeqCst) >= shared.limits.max_connections
                    {
                        shared.active.fetch_sub(1, Ordering::SeqCst);
                        let mut stream = stream;
                        let _ = http::respond_error(
                            &mut stream,
                            &HttpError::new(503, "overloaded: connection limit reached"),
                        );
                        continue;
                    }
                    let spawned = std::thread::Builder::new().name("git-http".into()).spawn({
                        let shared = shared.clone();
                        move || {
                            shared.handle(stream);
                            shared.active.fetch_sub(1, Ordering::SeqCst);
                        }
                    });
                    if spawned.is_err() {
                        shared.active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })?;
        Ok(Self {
            stop,
            path,
            thread: Some(thread),
            gc,
        })
    }
    /// Stops accepting connections and the gc thread (after a pass it is running).
    /// In-flight requests finish or die with the process.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some((wake, thread)) = self.gc.take() {
            drop(wake);
            let _ = thread.join();
        }
    }
}

/// Every `gc_interval_secs`, runs `git gc --auto` in each present repository.
fn start_gc(
    service: Arc<RepositoryService>,
    limits: GitHttpLimits,
) -> Result<(mpsc::Sender<()>, JoinHandle<()>)> {
    let (wake, wait) = mpsc::channel::<()>();
    let interval = Duration::from_secs(limits.gc_interval_secs);
    let thread = std::thread::Builder::new()
        .name("git-http-gc".into())
        .spawn(move || {
            while wait.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                let _ = collect_garbage(&service, &limits);
            }
        })?;
    Ok((wake, thread))
}

/// One `git gc --auto` pass. A repository that fails is skipped until the next pass.
pub(crate) fn collect_garbage(service: &RepositoryService, limits: &GitHttpLimits) -> Result<()> {
    let entries = match std::fs::read_dir(service.root.path().join("repositories")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let Ok(repo) = entry.file_name().into_string() else {
            continue;
        };
        if aq_git_protocol::validate_id(&repo).is_err() {
            continue;
        }
        let _ = gc_repository(service, limits, &repo);
    }
    Ok(())
}
fn gc_repository(service: &RepositoryService, limits: &GitHttpLimits, repo: &str) -> Result<()> {
    let timeout = Duration::from_secs(limits.git_timeout_secs);
    let _deadline = DeadlineScope::new(timeout);
    let svc = service;
    // Like `maintenance`: drain the repository so no reader holds a replaced pack.
    let lifecycle = svc.repositories.repository_lifecycle(repo);
    let _drain = crate::git::write_with_deadline(&lifecycle)?;
    if svc.artifact_pool_record(repo)?["lifecycle"] != "present" {
        return Ok(());
    }
    let _heavy = svc.repositories.heavy_budget.acquire()?;
    let pool = svc.repositories.pool_path(repo);
    crate::operations::checked_directory(&pool)?;
    svc.repositories.invalidate(repo);
    let args: Vec<String> = [
        "-c".to_string(),
        format!("gc.auto={}", limits.gc_auto_objects),
        "-c".into(),
        "gc.autoDetach=false".into(),
        "gc".into(),
        "--auto".into(),
        "--quiet".into(),
    ]
    .into();
    let output = svc
        .repositories
        .runner
        .run_with(&pool, &args, &[], &[], timeout)?;
    if !output.status.success() {
        return Err(Error::new("git_failed", "git gc --auto failed"));
    }
    Ok(())
}

/// Result of one buffered Git child run.
struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    timed_out: bool,
    output_limit: bool,
}

impl Shared {
    fn handle(&self, stream: UnixStream) {
        let timeout = Some(Duration::from_secs(self.limits.io_timeout_secs));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        let mut reader = http::buffered(stream);
        let _deadline = DeadlineScope::new(Duration::from_secs(self.limits.git_timeout_secs));
        if let Err(error) = self.route(&mut reader, &mut out) {
            let _ = http::respond_error(&mut out, &error);
        }
        let _ = out.shutdown(std::net::Shutdown::Both);
    }
    /// An `Err` means nothing was written to the client yet.
    fn route(
        &self,
        reader: &mut std::io::BufReader<UnixStream>,
        out: &mut Out,
    ) -> std::result::Result<(), HttpError> {
        let head = http::read_head(reader, self.limits.max_header_bytes)?;
        let grant = Grant::from_headers(&head.headers)?;
        let segments: Vec<&str> = head.path.trim_start_matches('/').split('/').collect();
        if segments.first() != Some(&grant.repository.as_str()) {
            return Err(HttpError::new(
                400,
                "invalid_grant: repository in the URL differs from AQ-Repository",
            ));
        }
        let body = RequestBody {
            head: &head,
            reader,
        };
        match (head.method.as_str(), &segments[1..]) {
            ("GET", ["info", "refs"]) => {
                let service = head
                    .query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("service="))
                    .unwrap_or("");
                match service {
                    "git-upload-pack" | "git-receive-pack" => {
                        self.advertise(&grant, &service["git-".len()..], out)
                    }
                    _ => Err(HttpError::new(
                        400,
                        "service must be git-upload-pack or git-receive-pack",
                    )),
                }
            }
            ("POST", ["git-upload-pack"]) => self.upload_pack(body, out, &grant),
            ("POST", ["git-receive-pack"]) => self.receive_pack(body, out, &grant),
            (_, ["info", "refs"] | ["git-upload-pack"] | ["git-receive-pack"]) => {
                Err(HttpError::new(405, "method not allowed"))
            }
            _ => Err(HttpError::new(404, "unknown route")),
        }
    }

    /// Binding, pool and admission checks shared by every route.
    fn prepare(&self, grant: &Grant) -> Result<PathBuf> {
        let svc = &*self.service;
        let binding = svc.repositories.binding(&grant.repository)?;
        if binding.object_format != "sha1" {
            return Err(Error::new(
                "unsupported_feature",
                "only SHA-1 repositories are served",
            ));
        }
        let pool = svc.artifact_pool_record(&grant.repository)?;
        if pool["lifecycle"] != "present" {
            return Err(Error::new(
                "recovery_blocked",
                "the repository pool is not present",
            ));
        }
        let path = svc.repositories.pool_path(&grant.repository);
        crate::operations::checked_directory(&path)?;
        Ok(path)
    }

    /// `git <service> --stateless-rpc` on the repository, advertising only
    /// `refs/heads/main`, the `refs/heads/aq/*` task branches and the
    /// `refs/heads/aq-try/*` try refs.
    fn command(
        &self,
        grant: &Grant,
        pool: &Path,
        rpc: PackRpc<'_>,
    ) -> Result<std::process::Command> {
        let service = rpc.service();
        let mut args: Vec<String> = Vec::new();
        let mut config = |value: String| {
            args.push("-c".into());
            args.push(value);
        };
        // Later entries win: hide everything, then un-hide main, the task branches
        // and the try refs (a builder fetches those by advertisement). Git refuses a
        // push to a hidden ref, so a push outside them fails here already.
        config("transfer.hideRefs=HEAD".into());
        config("transfer.hideRefs=refs/".into());
        config(format!("transfer.hideRefs=!{MAIN_REF}"));
        config(format!(
            "transfer.hideRefs=!{}",
            TASK_PREFIX.trim_end_matches('/')
        ));
        config(format!(
            "transfer.hideRefs=!{}",
            TRY_PREFIX.trim_end_matches('/')
        ));
        if service == "receive-pack" {
            config("receive.fsckObjects=true".into());
            config(format!("receive.maxInputSize={}", grant.max_push_bytes));
            config("receive.denyDeletes=true".into());
            // A returned task may rebuild its branch; the worker pushes with a lease.
            config("receive.denyNonFastForwards=false".into());
            config("receive.autogc=false".into());
            config(format!("core.hooksPath={}", self.hooks.display()));
        }
        args.push(service.into());
        args.push("--stateless-rpc".into());
        if matches!(rpc, PackRpc::Advertise(_)) {
            args.push("--advertise-refs".into());
        }
        args.push(pool.to_string_lossy().into_owned());
        let mut prepared = self.service.repositories.runner.command(args, None)?;
        prepared
            .command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(prepared.command)
    }

    fn watchdog(&self, pid: u32, timed_out: Arc<AtomicBool>) -> (mpsc::Sender<()>, JoinHandle<()>) {
        let (done, wait) = mpsc::channel::<()>();
        let timeout = Duration::from_secs(self.limits.git_timeout_secs);
        let handle = std::thread::spawn(move || {
            if wait.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                timed_out.store(true, Ordering::SeqCst);
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
            }
        });
        (done, handle)
    }

    /// Runs a Git child to completion, feeding it `input` and buffering at most `max_out` bytes.
    fn run_buffered(
        &self,
        mut command: std::process::Command,
        input: &mut (dyn Read + Send),
        max_out: usize,
    ) -> Result<Run> {
        let mut child = command
            .spawn()
            .map_err(|e| Error::new("git_spawn", format!("could not start Git: {e}")))?;
        let pid = child.id();
        let timed_out = Arc::new(AtomicBool::new(false));
        let (done, watchdog) = self.watchdog(pid, timed_out.clone());
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (run_stdout, run_stderr, output_limit) = std::thread::scope(|scope| {
            scope.spawn(move || {
                let copied = std::io::copy(input, &mut stdin);
                // Closing stdin tells Git the request ended. A Git child that already
                // exited makes this fail, which the outcome below accounts for.
                drop(stdin);
                if copied.is_err() {
                    unsafe {
                        libc::killpg(pid as i32, libc::SIGKILL);
                    }
                }
            });
            let errors = scope.spawn(move || {
                let mut buffer = Vec::new();
                let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buffer);
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                buffer
            });
            let mut buffer = Vec::new();
            let _ = (&mut stdout)
                .take(max_out as u64 + 1)
                .read_to_end(&mut buffer);
            let limit = buffer.len() > max_out;
            if limit {
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
            }
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            (buffer, errors.join().unwrap_or_default(), limit)
        });
        let status = child.wait();
        drop(done);
        let _ = watchdog.join();
        Ok(Run {
            stdout: run_stdout,
            stderr: run_stderr,
            success: status.is_ok_and(|s| s.success()),
            timed_out: timed_out.load(Ordering::SeqCst),
            output_limit,
        })
    }

    fn max_output(&self) -> usize {
        self.service.repositories.runner.limits().max_output_bytes
    }

    fn advertise(
        &self,
        grant: &Grant,
        service: &str,
        out: &mut Out,
    ) -> std::result::Result<(), HttpError> {
        let lifecycle = self
            .service
            .repositories
            .repository_lifecycle(&grant.repository);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let pool = self.prepare(grant)?;
        let command = self.command(grant, &pool, PackRpc::Advertise(service))?;
        let run = self.run_buffered(command, &mut std::io::empty(), self.max_output())?;
        if !run.success || run.incomplete() {
            return Err(failure("advertising refs", &run));
        }
        let announcement = format!("# service=git-{service}\n");
        let mut body = format!("{:04x}{announcement}0000", announcement.len() + 4).into_bytes();
        body.extend_from_slice(&run.stdout);
        http::respond(
            out,
            200,
            &format!("application/x-git-{service}-advertisement"),
            &body,
        )?;
        Ok(())
    }

    fn check_content_type(head: &Head, service: &str) -> std::result::Result<(), HttpError> {
        let expected = format!("application/x-git-{service}-request");
        match head.header("content-type") {
            Some(value) if value.split(';').next().unwrap_or("").trim() == expected => Ok(()),
            _ => Err(HttpError::new(
                415,
                format!("Content-Type must be {expected}"),
            )),
        }
    }
    fn continue_if_expected(head: &Head, out: &mut Out) -> std::io::Result<()> {
        if head
            .header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
        {
            out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        Ok(())
    }

    fn upload_pack(
        &self,
        body: RequestBody<'_>,
        out: &mut Out,
        grant: &Grant,
    ) -> std::result::Result<(), HttpError> {
        let RequestBody { head, reader } = body;
        Self::check_content_type(head, "upload-pack")?;
        let lifecycle = self
            .service
            .repositories
            .repository_lifecycle(&grant.repository);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let pool = self.prepare(grant)?;
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut body = http::body(
            head,
            &mut *reader,
            self.limits.max_upload_request_bytes,
            exceeded,
        )?;
        Self::continue_if_expected(head, out)?;
        let mut request = Vec::new();
        body.read_to_end(&mut request)
            .map_err(|e| HttpError::new(400, format!("could not read the request body: {e}")))?;
        let mut child: Child = self
            .command(grant, &pool, PackRpc::Transfer("upload-pack"))?
            .spawn()
            .map_err(|e| HttpError::new(500, format!("git_spawn: {e}")))?;
        let pid = child.id();
        let timed_out = Arc::new(AtomicBool::new(false));
        let (done, watchdog) = self.watchdog(pid, timed_out.clone());
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let mut early_failure = None;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _ = stdin.write_all(&request);
            });
            let errors = scope.spawn(move || {
                let mut buffer = Vec::new();
                let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buffer);
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                buffer
            });
            if !stream_upload(&mut stdout, out, pid) {
                early_failure = Some(errors.join().unwrap_or_default());
            }
        });
        let status = child.wait();
        drop(done);
        let _ = watchdog.join();
        if let Some(stderr) = early_failure {
            // Nothing was sent: report a failed Git child, or an empty successful reply.
            if !status.is_ok_and(|s| s.success()) || timed_out.load(Ordering::SeqCst) {
                return Err(HttpError::new(
                    500,
                    format!(
                        "git_failed: upload-pack failed: {}",
                        String::from_utf8_lossy(&stderr).trim()
                    ),
                ));
            }
            http::respond(out, 200, "application/x-git-upload-pack-result", b"")?;
        }
        Ok(())
    }

    fn receive_pack(
        &self,
        body: RequestBody<'_>,
        out: &mut Out,
        grant: &Grant,
    ) -> std::result::Result<(), HttpError> {
        let RequestBody { head, reader } = body;
        Self::check_content_type(head, "receive-pack")?;
        let svc = &*self.service;
        let lifecycle = svc.repositories.repository_lifecycle(&grant.repository);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let pool = self.prepare(grant)?;
        let _heavy = svc.repositories.heavy_budget.acquire()?;
        let limits = svc.artifact_limits()?;
        crate::artifact::reserve(&pool, limits.max_expanded_bytes, limits.disk_reserve_bytes)?;
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut body = http::body(head, &mut *reader, grant.max_push_bytes, exceeded.clone())?;
        Self::continue_if_expected(head, out)?;

        let mut command = self.command(grant, &pool, PackRpc::Transfer("receive-pack"))?;
        command.env(
            GRANT_ENV,
            serde_json::to_string(grant).map_err(Error::from)?,
        );
        let run = self.run_buffered(command, &mut body, self.max_output());
        drop(body);
        let run = run?;
        if exceeded.load(Ordering::SeqCst) {
            return Err(HttpError::new(
                413,
                "limit_exceeded: push exceeds the granted byte limit (AQ-Max-Push-Bytes)",
            ));
        }
        // Git reports refused updates in its result; only a run without one is an error.
        if run.incomplete() || run.failed_without_reply() {
            return Err(failure("receiving the push", &run));
        }
        http::respond(
            out,
            200,
            "application/x-git-receive-pack-result",
            &run.stdout,
        )?;
        Ok(())
    }
}

fn write_upload_head(out: &mut Out) -> std::io::Result<()> {
    http::write_head(
        out,
        200,
        &[
            (
                "Content-Type",
                "application/x-git-upload-pack-result".into(),
            ),
            ("Cache-Control", "no-cache".into()),
            ("Transfer-Encoding", "chunked".into()),
        ],
    )
}

fn write_upload_chunk(out: &mut Out, chunk: &[u8], started: &mut bool) -> std::io::Result<()> {
    if !*started {
        write_upload_head(out)?;
        *started = true;
    }
    ChunkedWriter(out).chunk(chunk)
}

fn stream_upload(stdout: &mut impl Read, out: &mut Out, pid: u32) -> bool {
    let mut chunk = vec![0u8; 64 * 1024];
    let mut started = false;
    let mut client_gone = false;
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if write_upload_chunk(out, &chunk[..n], &mut started).is_err() {
            client_gone = true;
            break;
        }
    }
    if client_gone {
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    }
    if started {
        let _ = ChunkedWriter(&mut *out).finish();
    }
    started
}

impl Run {
    fn incomplete(&self) -> bool {
        self.timed_out || self.output_limit
    }
    fn failed_without_reply(&self) -> bool {
        !self.success && self.stdout.is_empty()
    }
}

fn failure(action: &str, run: &Run) -> HttpError {
    HttpError::new(
        if run.timed_out { 504 } else { 500 },
        format!(
            "git_failed: {action} failed: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        ),
    )
}
