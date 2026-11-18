use super::{
    GitHttpLimits, Grant, JOURNAL_DIR,
    hook::{GRANT_ENV, HOOK_MARKER, HOOKS, HookConfig},
    http::{self, BodyLimits, ChunkedWriter, Head, HttpError},
    stage::{self, Counter, Held, PushRequest, View},
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

/// The running Git smart-HTTP endpoint. Dropping it does not stop it; call [`stop`](Self::stop).
pub struct GitHttpServer {
    stop: Arc<AtomicBool>,
    path: PathBuf,
    thread: Option<JoinHandle<()>>,
}
struct Shared {
    service: Arc<RepositoryService>,
    limits: GitHttpLimits,
    hooks: PathBuf,
    journal: PathBuf,
    staging: PathBuf,
    views: PathBuf,
    active: AtomicUsize,
    /// Bytes buffered in memory by all requests together.
    memory: Counter,
    /// Disk bytes in-flight requests may still write (staged bodies, quarantines).
    disk: Counter,
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
        let journal = service.root.create_dir(JOURNAL_DIR)?;
        let staging = service.root.create_dir("git-http/staging")?;
        let views = service.root.create_dir("git-http/views")?;
        // Nothing runs yet, so whatever these hold belongs to a request that died.
        for dir in [&staging, &views] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
                }
            }
        }
        let _ = std::fs::remove_file(hooks.join("post-receive"));
        let quoted = exe.to_string_lossy().replace('\'', "'\\''");
        for kind in HOOKS {
            let script = format!("#!/bin/sh\nexec '{quoted}' {HOOK_MARKER} {kind} \"$@\"\n");
            let path = hooks.join(kind);
            let temporary = hooks.join(format!(".{kind}.tmp"));
            std::fs::write(&temporary, script)?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))?;
            std::fs::rename(&temporary, &path)?;
        }
        // Pushes that died between "ref moved" and "receipt durable" are settled before serving.
        {
            let _scope = DeadlineScope::new(Duration::from_secs(limits.request_deadline_secs));
            service.reconcile_all_pushes();
        }
        let path = match listener.local_addr()?.as_pathname() {
            Some(path) => path.to_path_buf(),
            None => return Err(Error::new("invalid_configuration", "unnamed socket")),
        };
        let shared = Arc::new(Shared {
            service,
            limits,
            hooks,
            journal,
            staging,
            views,
            active: AtomicUsize::new(0),
            memory: Counter::default(),
            disk: Counter::default(),
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
        })
    }
    /// Stops accepting connections. In-flight requests finish or die with the process.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Ends the read side of a connection when the request's absolute deadline passes, which
/// wakes a thread blocked reading a slow body.
struct RequestDeadline {
    expired: Arc<AtomicBool>,
    cancel: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}
impl RequestDeadline {
    fn start(stream: UnixStream, timeout: Duration) -> Self {
        let expired = Arc::new(AtomicBool::new(false));
        let (cancel, wait) = mpsc::channel::<()>();
        let flag = expired.clone();
        let thread = std::thread::spawn(move || {
            if wait.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                flag.store(true, Ordering::SeqCst);
                let _ = stream.shutdown(std::net::Shutdown::Read);
            }
        });
        Self {
            expired,
            cancel: Some(cancel),
            thread: Some(thread),
        }
    }
}
impl Drop for RequestDeadline {
    fn drop(&mut self) {
        drop(self.cancel.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Result of one buffered Git child run.
struct Run<'a> {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    timed_out: bool,
    output_limit: bool,
    memory_limit: bool,
    watch_tripped: bool,
    input: std::io::Result<u64>,
    _held: Held<'a>,
}

impl Shared {
    fn handle(&self, stream: UnixStream) {
        let timeout = Some(Duration::from_secs(self.limits.io_timeout_secs));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let (Ok(mut out), Ok(waker)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        let mut reader = http::buffered(stream);
        let request = Duration::from_secs(self.limits.request_deadline_secs);
        let deadline = RequestDeadline::start(waker, request);
        let _scope = DeadlineScope::new(request);
        if let Err(error) = self.route(&mut reader, &mut out, &deadline.expired) {
            let _ = http::respond_error(&mut out, &error);
        }
        let _ = out.shutdown(std::net::Shutdown::Both);
    }
    /// An `Err` means nothing was written to the client yet.
    fn route(
        &self,
        reader: &mut std::io::BufReader<UnixStream>,
        out: &mut Out,
        expired: &Arc<AtomicBool>,
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
            ("POST", ["git-upload-pack"]) => self.upload_pack(&head, reader, out, &grant, expired),
            ("POST", ["git-receive-pack"]) => {
                self.receive_pack(&head, reader, out, &grant, expired)
            }
            (_, ["info", "refs"] | ["git-upload-pack"] | ["git-receive-pack"]) => {
                Err(HttpError::new(405, "method not allowed"))
            }
            _ => Err(HttpError::new(404, "unknown route")),
        }
    }

    /// Binding, pool and admission checks shared by every route.
    fn prepare(&self, grant: &Grant) -> Result<(PathBuf, crate::repository::Binding)> {
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
        Ok((path, binding))
    }

    /// The refs the grant may see, as a scratch repository over the pool: the main ref at
    /// the head `repo.retain_ref` last retained, and the task ref if it exists. Git serves
    /// exactly these; no `hideRefs` rule can expose a neighbour of an allowed name.
    fn view(&self, grant: &Grant, pool: &Path) -> Result<View> {
        let svc = &*self.service;
        let main = svc.retained_head(&grant.repository, &grant.main_ref)?;
        let task = svc.ref_value(&grant.repository, &grant.task_ref)?;
        View::create(&self.views, pool, &stage::grant_refs(grant, main, task))
    }

    /// `git <service> --stateless-rpc` on `repository` (a view, or the pool for a push).
    fn command(
        &self,
        repository: &Path,
        service: &str,
        advertise: bool,
        receive: Option<&Grant>,
    ) -> Result<std::process::Command> {
        let mut args: Vec<String> = Vec::new();
        let mut config = |value: String| {
            args.push("-c".into());
            args.push(value);
        };
        // The pool is an alternate of every view; its refs are not this grant's to learn.
        config("core.alternateRefsCommand=true".into());
        if let Some(grant) = receive {
            config("receive.fsckObjects=true".into());
            config(format!("receive.maxInputSize={}", grant.max_push_bytes));
            config("receive.denyDeletes=true".into());
            config("receive.autogc=false".into());
            // Always keep the received pack as a pack: its tail then shows which delta
            // bases Git copied in from the pool (see hook.rs).
            config("receive.unpackLimit=1".into());
            config("transfer.unpackLimit=1".into());
            config("receive.advertisePushOptions=false".into());
            config("receive.advertiseAtomic=false".into());
            config(format!("core.hooksPath={}", self.hooks.display()));
        }
        args.push(service.into());
        args.push("--stateless-rpc".into());
        if advertise {
            args.push("--advertise-refs".into());
        }
        args.push(repository.to_string_lossy().into_owned());
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
        let mut timeout = Duration::from_secs(self.limits.git_timeout_secs);
        if let Some(remaining) = crate::git::remaining_deadline() {
            timeout = timeout.min(remaining);
        }
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

    /// Runs a Git child to completion, feeding it `input` and buffering at most `max_out`
    /// bytes. The child counts against the runner's shared child limit; its buffered output
    /// counts against the shared memory budget. `watch` is polled while the child runs and
    /// kills it when it returns true.
    fn run_buffered(
        &self,
        mut command: std::process::Command,
        input: &mut (dyn Read + Send),
        max_out: usize,
        watch: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Result<Run<'_>> {
        let _slot = self.service.repositories.runner.admit_child()?;
        let mut held = self.memory.charge(0, self.limits.max_buffered_bytes)?;
        let mut child = command
            .spawn()
            .map_err(|e| Error::new("git_spawn", format!("could not start Git: {e}")))?;
        let pid = child.id();
        let timed_out = Arc::new(AtomicBool::new(false));
        let (done, watchdog) = self.watchdog(pid, timed_out.clone());
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let finished = AtomicBool::new(false);
        let tripped = AtomicBool::new(false);
        let kill = || unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        };
        let (run_stdout, run_stderr, input_result, output_limit, memory_limit) =
            std::thread::scope(|scope| {
                let writer = scope.spawn(move || {
                    let copied = std::io::copy(input, &mut stdin);
                    // Closing stdin tells Git the request ended. A Git child that already
                    // exited makes this fail, which the outcome below accounts for.
                    drop(stdin);
                    if copied.is_err() {
                        unsafe {
                            libc::killpg(pid as i32, libc::SIGKILL);
                        }
                    }
                    copied
                });
                let errors = scope.spawn(move || {
                    let mut buffer = Vec::new();
                    let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buffer);
                    let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                    buffer
                });
                if let Some(watch) = watch {
                    let (finished, tripped) = (&finished, &tripped);
                    scope.spawn(move || {
                        while !finished.load(Ordering::SeqCst) {
                            if watch() {
                                tripped.store(true, Ordering::SeqCst);
                                kill();
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    });
                }
                let mut buffer = Vec::new();
                let (mut limit, mut memory) = (false, false);
                let mut chunk = vec![0u8; 32 * 1024];
                loop {
                    let n = match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    if buffer.len() + n > max_out {
                        limit = true;
                    } else if held.grow(n as u64, self.limits.max_buffered_bytes).is_err() {
                        memory = true;
                    } else {
                        buffer.extend_from_slice(&chunk[..n]);
                        continue;
                    }
                    kill();
                    break;
                }
                let _ = std::io::copy(&mut stdout, &mut std::io::sink());
                finished.store(true, Ordering::SeqCst);
                (
                    buffer,
                    errors.join().unwrap_or_default(),
                    writer
                        .join()
                        .unwrap_or_else(|_| Err(std::io::Error::other("input thread failed"))),
                    limit,
                    memory,
                )
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
            memory_limit,
            watch_tripped: tripped.load(Ordering::SeqCst),
            input: input_result,
            _held: held,
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
        let (pool, _) = self.prepare(grant)?;
        let view = self.view(grant, &pool)?;
        let command = self.command(&view.path, service, true, None)?;
        let run = self.run_buffered(command, &mut std::io::empty(), self.max_output(), None)?;
        if !run.success || run.timed_out || run.output_limit || run.memory_limit {
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
    fn body_limits(&self, decoded: u64) -> BodyLimits {
        BodyLimits {
            decoded,
            encoded: decoded,
            framing: self.limits.max_header_bytes,
        }
    }
    /// Why reading a request body failed.
    fn body_error(
        &self,
        error: std::io::Error,
        exceeded: &AtomicBool,
        expired: &AtomicBool,
        limit_name: &str,
    ) -> HttpError {
        if exceeded.load(Ordering::SeqCst) {
            HttpError::new(413, format!("limit_exceeded: request exceeds {limit_name}"))
        } else if expired.load(Ordering::SeqCst) {
            HttpError::new(
                408,
                "deadline_exceeded: the request body was not received in time",
            )
        } else {
            HttpError::new(400, format!("could not read the request body: {error}"))
        }
    }
    /// Reserves `bytes` of disk against the free-space reserve, counting what every
    /// in-flight request already reserved.
    fn reserve_disk(&self, near: &Path, bytes: u64) -> Result<Held<'_>> {
        let limits = self.service.artifact_limits()?;
        let held = self.disk.charge(bytes, u64::MAX)?;
        crate::artifact::reserve(near, self.disk.used(), limits.disk_reserve_bytes)?;
        Ok(held)
    }

    fn upload_pack(
        &self,
        head: &Head,
        reader: &mut std::io::BufReader<UnixStream>,
        out: &mut Out,
        grant: &Grant,
        expired: &Arc<AtomicBool>,
    ) -> std::result::Result<(), HttpError> {
        Self::check_content_type(head, "upload-pack")?;
        Self::continue_if_expected(head, out)?;
        // The request is read before any lock or child slot is taken; its bytes are
        // charged to the shared memory budget as they arrive.
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut body = http::body(
            head,
            &mut *reader,
            self.body_limits(self.limits.max_upload_request_bytes),
            exceeded.clone(),
        )?;
        let mut held = self.memory.charge(0, self.limits.max_buffered_bytes)?;
        let mut request = Vec::new();
        let mut chunk = vec![0u8; 32 * 1024];
        loop {
            match body.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    held.grow(n as u64, self.limits.max_buffered_bytes)?;
                    request.extend_from_slice(&chunk[..n]);
                }
                Err(e) => {
                    return Err(self.body_error(e, &exceeded, expired, "the upload request limit"));
                }
            }
        }
        drop(body);
        let lifecycle = self
            .service
            .repositories
            .repository_lifecycle(&grant.repository);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let (pool, _) = self.prepare(grant)?;
        let view = self.view(grant, &pool)?;
        let _slot = self.service.repositories.runner.admit_child()?;
        let mut child: Child = self
            .command(&view.path, "upload-pack", false, None)?
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
            let mut chunk = vec![0u8; 64 * 1024];
            let mut started = false;
            let mut client_gone = false;
            loop {
                let n = match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if !started {
                    let sent = http::write_head(
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
                    );
                    if sent.is_err() {
                        client_gone = true;
                        break;
                    }
                    started = true;
                }
                if ChunkedWriter(&mut *out).chunk(&chunk[..n]).is_err() {
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
            } else {
                early_failure = Some(errors.join().unwrap_or_default());
            }
        });
        let status = child.wait();
        drop(done);
        let _ = watchdog.join();
        drop(held);
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
        head: &Head,
        reader: &mut std::io::BufReader<UnixStream>,
        out: &mut Out,
        grant: &Grant,
        expired: &Arc<AtomicBool>,
    ) -> std::result::Result<(), HttpError> {
        Self::check_content_type(head, "receive-pack")?;
        let svc = &*self.service;
        let repo = &grant.repository;
        Self::continue_if_expected(head, out)?;

        // 1. Bound the body and spool it to disk. Nothing shared (lifecycle lease, heavy
        //    permit, repository lock) is held while a client uploads, so a slow upload
        //    ties up only its own connection until the request deadline.
        let exceeded = Arc::new(AtomicBool::new(false));
        let staged = {
            let declared = head
                .header("content-length")
                .filter(|_| head.header("content-encoding").is_none())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            let _space = self.reserve_disk(&self.staging, declared.min(grant.max_push_bytes))?;
            let mut body = http::body(
                head,
                &mut *reader,
                self.body_limits(grant.max_push_bytes),
                exceeded.clone(),
            )?;
            stage::spool(&self.staging, &mut body).map_err(|e| {
                self.body_error(
                    e,
                    &exceeded,
                    expired,
                    "the granted byte limit (AQ-Max-Push-Bytes)",
                )
            })?
        };
        // 2. Refuse what Git must never see: certificates, extra capabilities, junk.
        let request: PushRequest =
            stage::parse_push(&mut staged.open()?, self.limits.max_header_bytes)?;

        // 3. Shared resources, now that the body is bounded and local.
        let lifecycle = svc.repositories.repository_lifecycle(repo);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let (pool, binding) = self.prepare(grant)?;
        let _heavy = svc.repositories.heavy_budget.acquire()?;
        // One receive per repository at a time: the ref, its journal and Git's quarantine
        // cannot change underneath the checks.
        let mutation = svc.mutation_lock(repo);
        let _mutation = crate::git::lock_with_deadline(&mutation)?;

        // 4. An operation that already happened gets the same answer again.
        let finished = match svc.reconcile_push(&grant.operation)? {
            Some(receipt) => Some(receipt),
            None => svc.push_receipt_of(&grant.operation)?,
        };
        if let Some(receipt) = finished {
            let replay = [
                receipt
                    .old
                    .clone()
                    .unwrap_or_else(|| super::ZERO_OID.to_owned()),
                receipt.new.clone(),
                receipt.reference.clone(),
            ];
            if receipt.repository == *repo && request.commands == [replay] {
                return Ok(http::respond(
                    out,
                    200,
                    "application/x-git-receive-pack-result",
                    &stage::synthetic_report(&request, &receipt.reference),
                )?);
            }
            return Err(Error::new(
                "idempotency_conflict",
                "operation already has a push receipt for a different update",
            )
            .into());
        }
        svc.commit_exists(repo, &grant.base)?;
        let limits = svc.artifact_limits()?;
        let main_head = svc.retained_head(repo, &grant.main_ref)?;
        let _quarantine_space = self.reserve_disk(&pool, limits.max_expanded_bytes)?;
        // Whatever an earlier receive left in quarantine (daemon killed mid-push) is dead.
        stage::remove_quarantines(&pool, &[]);

        let config = HookConfig {
            grant: grant.clone(),
            git: svc.repositories.runner.git.clone(),
            max_expanded_bytes: limits.max_expanded_bytes,
            main_head,
            pack_objects: request.pack_objects,
            retention_handle: uuid::Uuid::new_v4().to_string(),
            binding_generation: binding.binding_generation,
            journal_dir: self.journal.clone(),
        };
        let mut command = self.command(&pool, "receive-pack", false, Some(grant))?;
        command.env(
            GRANT_ENV,
            serde_json::to_string(&config).map_err(Error::from)?,
        );
        // 5. Run Git under a live disk budget: its quarantine may not outgrow the quota.
        let budget = limits.max_expanded_bytes;
        let watched = pool.clone();
        let over_budget = move || {
            stage::quarantines(&watched).iter().any(|dir| {
                match crate::git::directory_bytes(dir, budget) {
                    Ok(bytes) => bytes > budget,
                    Err(error) => error.code == "invalid_path",
                }
            })
        };
        let mut input = staged.open()?;
        let run = self.run_buffered(command, &mut input, self.max_output(), Some(&over_budget));
        // A killed Git leaves its quarantine behind; a finished one has none.
        stage::remove_quarantines(&pool, &[]);
        let run = run?;

        // 6. Settle the journal: the ref has moved exactly when the hook journaled it. The
        //    answer is sent only once pin and receipt are durable.
        let landed = match svc.reconcile_push(&grant.operation) {
            Ok(landed) => landed,
            Err(error) => {
                let reverted = svc.revert_push(&grant.operation).unwrap_or(false);
                let _ = svc.settle_promotion(&grant.operation);
                return Err(error
                    .details(serde_json::json!({"push_reverted":reverted}))
                    .into());
            }
        };
        // Pushed packs Git promoted for a ref update that then did not happen go again.
        let _ = svc.settle_promotion(&grant.operation);
        if landed.is_none() && run.success {
            self.refuse_unjournaled_update(grant, &request)?;
        }
        if run.watch_tripped {
            return Err(HttpError::new(
                413,
                "limit_exceeded: push exceeded the storage quota while Git unpacked it",
            ));
        }
        if run.timed_out
            || run.output_limit
            || run.memory_limit
            || (landed.is_some() && !run.success)
            || (!run.success && run.stdout.is_empty())
            || (run.input.is_err() && !run.success && run.stdout.is_empty())
        {
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

    /// Git reported success but the hook left no journal: the ref must not have moved.
    /// If it did (hooks that did not run), put it back rather than acknowledge a push
    /// with no receipt.
    fn refuse_unjournaled_update(
        &self,
        grant: &Grant,
        request: &PushRequest,
    ) -> std::result::Result<(), HttpError> {
        let svc = &*self.service;
        let [old, new, reference] = match &request.commands[..] {
            [command] => command.clone(),
            _ => return Ok(()),
        };
        if old == new || svc.ref_value(&grant.repository, &reference)?.as_deref() != Some(&new) {
            return Ok(());
        }
        let undo = if old == super::ZERO_OID {
            vec!["update-ref".into(), "-d".into(), reference, new]
        } else {
            vec!["update-ref".into(), reference, old, new]
        };
        let reverted = svc.repositories.git(&grant.repository, undo).is_ok();
        Err(Error::new(
            "recovery_blocked",
            "the task ref moved without a journal entry",
        )
        .details(serde_json::json!({"push_reverted":reverted}))
        .into())
    }
}

fn failure(action: &str, run: &Run<'_>) -> HttpError {
    if run.memory_limit {
        return HttpError::new(
            503,
            "overloaded: request buffers exceed the shared memory budget",
        );
    }
    HttpError::new(
        if run.timed_out { 504 } else { 500 },
        format!(
            "git_failed: {action} failed: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        ),
    )
}
