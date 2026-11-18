use super::{
    GitHttpLimits, Grant, PushReceipt, ZERO_OID,
    hook::{ACCEPTED_ENV, GRANT_ENV, HOOK_MARKER, HookConfig},
    http::{self, ChunkedWriter, Head, HttpError},
    receipt_path,
};
use crate::{Error, Result, git::DeadlineScope, service::RepositoryService, state::RetentionRef};
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
    accepted: PathBuf,
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
        let accepted = service.root.create_dir("git-http/accepted")?;
        if let Ok(entries) = std::fs::read_dir(&accepted) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let quoted = exe.to_string_lossy().replace('\'', "'\\''");
        for kind in ["pre-receive", "post-receive"] {
            let script = format!("#!/bin/sh\nexec '{quoted}' {HOOK_MARKER} {kind}\n");
            let path = hooks.join(kind);
            let temporary = hooks.join(format!(".{kind}.tmp"));
            std::fs::write(&temporary, script)?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))?;
            std::fs::rename(&temporary, &path)?;
        }
        let path = match listener.local_addr()?.as_pathname() {
            Some(path) => path.to_path_buf(),
            None => return Err(Error::new("invalid_configuration", "unnamed socket")),
        };
        let shared = Arc::new(Shared {
            service,
            limits,
            hooks,
            accepted,
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

/// Result of one buffered Git child run.
struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    timed_out: bool,
    output_limit: bool,
    input: std::io::Result<u64>,
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
            ("POST", ["git-upload-pack"]) => self.upload_pack(&head, reader, out, &grant),
            ("POST", ["git-receive-pack"]) => self.receive_pack(&head, reader, out, &grant),
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

    /// `git <service> --stateless-rpc` on the pool, advertising only the main and task refs.
    fn command(
        &self,
        grant: &Grant,
        pool: &Path,
        service: &str,
        advertise: bool,
    ) -> Result<std::process::Command> {
        let mut args: Vec<String> = Vec::new();
        let mut config = |value: String| {
            args.push("-c".into());
            args.push(value);
        };
        // Later entries win: hide everything, then un-hide the two granted refs.
        config("transfer.hideRefs=HEAD".into());
        config("transfer.hideRefs=refs/".into());
        config(format!("transfer.hideRefs=!{}", grant.main_ref));
        config(format!("transfer.hideRefs=!{}", grant.task_ref));
        if service == "receive-pack" {
            config("receive.fsckObjects=true".into());
            config(format!("receive.maxInputSize={}", grant.max_push_bytes));
            config("receive.denyDeletes=true".into());
            config("receive.autogc=false".into());
            config(format!("core.hooksPath={}", self.hooks.display()));
        }
        args.push(service.into());
        args.push("--stateless-rpc".into());
        if advertise {
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
        let (run_stdout, run_stderr, input_result, output_limit) = std::thread::scope(|scope| {
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
            (
                buffer,
                errors.join().unwrap_or_default(),
                writer
                    .join()
                    .unwrap_or_else(|_| Err(std::io::Error::other("input thread failed"))),
                limit,
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
            input: input_result,
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
        let command = self.command(grant, &pool, service, true)?;
        let run = self.run_buffered(command, &mut std::io::empty(), self.max_output())?;
        if !run.success || run.timed_out || run.output_limit {
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
        head: &Head,
        reader: &mut std::io::BufReader<UnixStream>,
        out: &mut Out,
        grant: &Grant,
    ) -> std::result::Result<(), HttpError> {
        Self::check_content_type(head, "upload-pack")?;
        let lifecycle = self
            .service
            .repositories
            .repository_lifecycle(&grant.repository);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let (pool, _) = self.prepare(grant)?;
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
            .command(grant, &pool, "upload-pack", false)?
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
    ) -> std::result::Result<(), HttpError> {
        Self::check_content_type(head, "receive-pack")?;
        let svc = &*self.service;
        let repo = &grant.repository;
        let lifecycle = svc.repositories.repository_lifecycle(repo);
        let _lease = crate::git::read_with_deadline(&lifecycle)?;
        let (pool, binding) = self.prepare(grant)?;
        let _heavy = svc.repositories.heavy_budget.acquire()?;
        if svc
            .root
            .read_json::<serde_json::Value>(receipt_path(&grant.operation))?
            .is_some()
        {
            return Err(Error::new(
                "idempotency_conflict",
                "operation already has a push receipt",
            )
            .into());
        }
        svc.commit_exists(repo, &grant.base)?;
        let limits = svc.artifact_limits()?;
        crate::artifact::reserve(&pool, limits.max_expanded_bytes, limits.disk_reserve_bytes)?;
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut body = http::body(head, &mut *reader, grant.max_push_bytes, exceeded.clone())?;
        Self::continue_if_expected(head, out)?;

        let accepted_path = self.accepted.join(uuid::Uuid::new_v4().to_string());
        let config = HookConfig {
            grant: grant.clone(),
            git: svc.repositories.runner.git.clone(),
            max_expanded_bytes: limits.max_expanded_bytes,
        };
        let mut command = self.command(grant, &pool, "receive-pack", false)?;
        command
            .env(
                GRANT_ENV,
                serde_json::to_string(&config).map_err(Error::from)?,
            )
            .env(ACCEPTED_ENV, &accepted_path);
        let run = self.run_buffered(command, &mut body, self.max_output());
        drop(body);
        let accepted = std::fs::read_to_string(&accepted_path).ok();
        let _ = std::fs::remove_file(&accepted_path);
        let run = run?;
        if exceeded.load(Ordering::SeqCst) {
            return Err(HttpError::new(
                413,
                "limit_exceeded: push exceeds the granted byte limit (AQ-Max-Push-Bytes)",
            ));
        }
        if run.timed_out
            || run.output_limit
            || (run.input.is_err() && !run.success && run.stdout.is_empty())
        {
            return Err(failure("receiving the push", &run));
        }
        if !run.success && run.stdout.is_empty() {
            return Err(failure("receiving the push", &run));
        }
        if let Some(update) = accepted.filter(|_| run.success) {
            let mutation = svc.mutation_lock(repo);
            let _mutation = crate::git::lock_with_deadline(&mutation)?;
            if let Err(error) = self.record_push(&binding, grant, &update) {
                return Err(error.into());
            }
        }
        http::respond(
            out,
            200,
            "application/x-git-receive-pack-result",
            &run.stdout,
        )?;
        Ok(())
    }

    /// Pins the new head and writes the receipt of an accepted update; undoes the
    /// ref update when either fails, so no push is acknowledged without its receipt.
    fn record_push(
        &self,
        binding: &crate::repository::Binding,
        grant: &Grant,
        update: &str,
    ) -> Result<()> {
        let svc = &*self.service;
        let repo = &grant.repository;
        let lines: Vec<&str> = update.lines().filter(|l| !l.trim().is_empty()).collect();
        let [line] = lines[..] else {
            return Err(Error::new(
                "git_failed",
                "push accepted an unexpected number of refs",
            ));
        };
        let fields: Vec<&str> = line.split(' ').collect();
        let [old, new, reference] = fields[..] else {
            return Err(Error::new(
                "git_failed",
                "push acceptance record is malformed",
            ));
        };
        if reference != grant.task_ref {
            return Err(Error::new(
                "git_failed",
                "push accepted a ref other than the grant's",
            ));
        }
        let handle = uuid::Uuid::new_v4().to_string();
        let recorded = (|| -> Result<()> {
            svc.pin(
                repo,
                binding.binding_generation,
                &handle,
                "push",
                vec![RetentionRef {
                    name: format!("refs/aq-git/pushes/{handle}"),
                    oid: new.into(),
                }],
            )?;
            svc.root.write_json(
                receipt_path(&grant.operation),
                &PushReceipt {
                    repository: repo.clone(),
                    reference: reference.into(),
                    old: (old != ZERO_OID).then(|| old.to_owned()),
                    new: new.into(),
                    base: grant.base.clone(),
                    retention_handle: handle.clone(),
                    operation_id: grant.operation.clone(),
                },
            )
        })();
        if let Err(error) = recorded {
            let undo: Vec<String> = if old == ZERO_OID {
                vec![
                    "update-ref".into(),
                    "-d".into(),
                    reference.into(),
                    new.into(),
                ]
            } else {
                vec![
                    "update-ref".into(),
                    reference.into(),
                    old.into(),
                    new.into(),
                ]
            };
            let reverted = svc.repositories.git(repo, undo).is_ok();
            return Err(error.details(serde_json::json!({"push_reverted":reverted})));
        }
        Ok(())
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
