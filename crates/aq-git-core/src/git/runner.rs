use super::environment;
use crate::{Error, Result};
use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
thread_local! {static REQUEST_DEADLINE:std::cell::Cell<Option<Instant>>=const{std::cell::Cell::new(None)};}
/// Applies a caller deadline to all synchronous Git work on the current request thread.
/// Scopes nest without extending an outer deadline and restore it on drop.
pub struct DeadlineScope {
    previous: Option<Instant>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl DeadlineScope {
    pub fn new(timeout: Duration) -> Self {
        let now = Instant::now();
        let deadline = now.checked_add(timeout);
        let previous = REQUEST_DEADLINE.with(|cell| {
            let old = cell.get();
            cell.set(match (old, deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            });
            old
        });
        Self {
            previous,
            _thread: std::marker::PhantomData,
        }
    }
}
impl Drop for DeadlineScope {
    fn drop(&mut self) {
        REQUEST_DEADLINE.with(|cell| cell.set(self.previous));
    }
}
pub fn remaining_deadline() -> Option<Duration> {
    REQUEST_DEADLINE.with(|cell| {
        cell.get()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    })
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitLimits {
    pub max_children: usize,
    pub max_address_space_bytes: u64,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}
impl Default for GitLimits {
    fn default() -> Self {
        Self {
            max_children: 16,
            max_address_space_bytes: 1024 * 1024 * 1024,
            timeout: Duration::from_secs(300),
            max_output_bytes: 16 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug)]
pub struct GitOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct GitMetrics {
    pub spawns: u64,
    pub active: usize,
    pub duration_micros: u64,
    pub output_bytes: u64,
    pub timeouts: u64,
    pub output_limits: u64,
    pub waiting: usize,
    pub peak_waiters: usize,
    pub queued: u64,
    pub queue_wait_micros: u64,
    pub queue_timeouts: u64,
    pub pidfd_runs: u64,
    pub waiter_fallback_runs: u64,
}
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) spawns: AtomicU64,
    pub(crate) active: AtomicUsize,
    pub(crate) duration: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) output_limits: AtomicU64,
    admission: std::sync::Mutex<Admission>,
    available: std::sync::Condvar,
    waiting: AtomicUsize,
    peak_waiters: AtomicUsize,
    queued: AtomicU64,
    queue_wait_micros: AtomicU64,
    queue_timeouts: AtomicU64,
    pidfd_runs: AtomicU64,
    waiter_fallback_runs: AtomicU64,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}
impl ExecutableIdentity {
    fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(std::io::Error::other(
                "configured Git is not a regular executable",
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        })
    }
}
pub(crate) struct PreparedCommand {
    pub command: Command,
    pub helper_nonce: Option<[u8; 16]>,
}
#[derive(Clone)]
struct SpawnHelper {
    path: PathBuf,
    identity: ExecutableIdentity,
}
#[derive(Clone)]
pub struct GitRunner {
    pub(crate) git: PathBuf,
    pub(crate) limits: GitLimits,
    pub(crate) counters: Arc<Counters>,
    initial_identity: Option<ExecutableIdentity>,
    use_pidfd: bool,
    spawn_helper: Option<SpawnHelper>,
}
impl GitRunner {
    pub fn new(git: PathBuf) -> Self {
        Self::with_limits(git, GitLimits::default())
    }
    pub fn with_limits(git: PathBuf, limits: GitLimits) -> Self {
        let initial_identity = ExecutableIdentity::read(&git).ok();
        Self {
            git,
            limits,
            initial_identity,
            use_pidfd: true,
            spawn_helper: None,
            counters: Arc::new(Counters::default()),
        }
    }
    pub(crate) fn ensure_executable_unchanged(&self) -> Result<()> {
        let Some(initial) = self.initial_identity.as_ref() else {
            return Err(Error::new(
                "git_spawn",
                "configured Git was unavailable when this runner started",
            )
            .details(serde_json::json!({"restart_required":true})));
        };
        if ExecutableIdentity::read(&self.git).ok().as_ref() != Some(initial) {
            return Err(Error::new(
                "unsupported_feature",
                "configured Git executable changed; drain and restart the owner to verify the replacement",
            ).details(serde_json::json!({"restart_required":true})));
        }
        Ok(())
    }
    #[doc(hidden)]
    pub fn without_pidfd_for_testing(mut self) -> Self {
        self.use_pidfd = false;
        self
    }
    pub fn limits(&self) -> &GitLimits {
        &self.limits
    }
    /// Opt into the same-executable, pre-runtime resource-limit helper. Library
    /// embedding defaults to the existing pre_exec fallback. The supplied path
    /// must be trusted and wire run_spawn_helper_if_requested before its runtime;
    /// this is an internal setup contract, not a general program wrapper.
    pub fn with_spawn_helper(mut self, path: PathBuf) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::new(
                "invalid_configuration",
                "spawn helper path must be absolute",
            ));
        }
        let identity = ExecutableIdentity::read(&path)
            .map_err(|_| Error::new("git_spawn", "spawn helper is unavailable"))?;
        self.spawn_helper = Some(SpawnHelper { path, identity });
        Ok(self)
    }
    /// One factory controls environment, configuration, process groups and both
    /// resource limits for ordinary commands and persistent object readers.
    pub(crate) fn command<I, S>(&self, args: I, file_limit: Option<u64>) -> Result<PreparedCommand>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.ensure_executable_unchanged()?;
        let memory = self.limits.max_address_space_bytes;
        if memory == 0 {
            return Err(Error::new(
                "invalid_configuration",
                "Git address space limit must be positive",
            ));
        }
        let helper_nonce = self
            .spawn_helper
            .as_ref()
            .map(|_| *uuid::Uuid::new_v4().as_bytes());
        let mut command = if let Some(helper) = &self.spawn_helper {
            if ExecutableIdentity::read(&helper.path).ok().as_ref() != Some(&helper.identity) {
                return Err(Error::new(
                    "unsupported_feature",
                    "spawn helper executable changed; drain and restart the owner",
                )
                .details(serde_json::json!({"restart_required":true})));
            }
            let mut command = Command::new(&helper.path);
            command
                .arg(SPAWN_HELPER_MARKER)
                .arg(memory.to_string())
                .arg(file_limit.map_or_else(|| "none".into(), |limit| limit.to_string()))
                .arg(serde_json::to_string(
                    self.initial_identity.as_ref().unwrap(),
                )?)
                .arg(uuid::Uuid::from_bytes(helper_nonce.unwrap()).to_string())
                .arg(&self.git);
            command
        } else {
            let mut command = Command::new(&self.git);
            unsafe {
                command.pre_exec(move || {
                    let bound = libc::rlimit {
                        rlim_cur: memory,
                        rlim_max: memory,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &bound) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(file_limit) = file_limit {
                        let bound = libc::rlimit {
                            rlim_cur: file_limit,
                            rlim_max: file_limit,
                        };
                        if libc::setrlimit(libc::RLIMIT_FSIZE, &bound) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            command
        };
        environment::configure(&mut command);
        command.args(args).process_group(0);
        Ok(PreparedCommand {
            command,
            helper_nonce,
        })
    }
    pub fn metrics(&self) -> GitMetrics {
        let c = &self.counters;
        GitMetrics {
            spawns: c.spawns.load(Ordering::Relaxed),
            active: c.active.load(Ordering::Relaxed),
            duration_micros: c.duration.load(Ordering::Relaxed),
            output_bytes: c.bytes.load(Ordering::Relaxed),
            timeouts: c.timeouts.load(Ordering::Relaxed),
            output_limits: c.output_limits.load(Ordering::Relaxed),
            waiting: c.waiting.load(Ordering::Relaxed),
            peak_waiters: c.peak_waiters.load(Ordering::Relaxed),
            queued: c.queued.load(Ordering::Relaxed),
            queue_wait_micros: c.queue_wait_micros.load(Ordering::Relaxed),
            queue_timeouts: c.queue_timeouts.load(Ordering::Relaxed),
            pidfd_runs: c.pidfd_runs.load(Ordering::Relaxed),
            waiter_fallback_runs: c.waiter_fallback_runs.load(Ordering::Relaxed),
        }
    }
    pub fn run(&self, cwd: &Path, args: &[String]) -> Result<GitOutput> {
        self.run_with(cwd, args, &[], &[], self.limits.timeout)
    }
    pub fn run_with(
        &self,
        cwd: &Path,
        args: &[String],
        input: &[u8],
        extra_env: &[(String, String)],
        timeout: Duration,
    ) -> Result<GitOutput> {
        self.run_internal(cwd, args, input, extra_env, timeout, None, None, None)
    }
    pub fn run_to_file(
        &self,
        cwd: &Path,
        args: &[String],
        file: std::fs::File,
        max_bytes: usize,
    ) -> Result<GitOutput> {
        self.run_internal(
            cwd,
            args,
            &[],
            &[],
            self.limits.timeout,
            Some((file, max_bytes)),
            None,
            None,
        )
    }
    pub fn run_with_disk_budget(
        &self,
        cwd: &Path,
        args: &[String],
        budget_root: &Path,
        max_bytes: u64,
    ) -> Result<GitOutput> {
        self.run_internal(
            cwd,
            args,
            &[],
            &[],
            self.limits.timeout,
            None,
            Some((budget_root, max_bytes)),
            None,
        )
    }
    pub(crate) fn run_remote(
        &self,
        cwd: &Path,
        args: &[String],
        auth: &super::credentials::RemoteAuth,
        timeout: Duration,
    ) -> Result<GitOutput> {
        let args = auth.arguments(args);
        let output = self.run_internal(cwd, &args, &[], &[], timeout, None, None, Some(auth))?;
        auth.finish(output)
    }
    #[allow(clippy::too_many_arguments)]
    fn run_internal(
        &self,
        cwd: &Path,
        args: &[String],
        input: &[u8],
        extra_env: &[(String, String)],
        timeout: Duration,
        sink: Option<(std::fs::File, usize)>,
        disk_budget: Option<(&Path, u64)>,
        remote_auth: Option<&super::credentials::RemoteAuth>,
    ) -> Result<GitOutput> {
        let operation_started = Instant::now();
        let command_deadline = operation_started
            .checked_add(timeout.min(self.limits.timeout))
            .ok_or_else(|| {
                Error::new("invalid_configuration", "Git timeout is not representable")
            })?;
        let request_deadline = REQUEST_DEADLINE.with(|cell| cell.get());
        let deadline =
            request_deadline.map_or(command_deadline, |request| request.min(command_deadline));
        let timeout_code = if request_deadline.is_some_and(|request| request <= command_deadline) {
            "deadline_exceeded"
        } else {
            "git_timeout"
        };
        if deadline <= Instant::now() {
            return Err(Error::new(
                "deadline_exceeded",
                "request deadline expired before Git admission",
            ));
        }
        if input.len() > self.limits.max_output_bytes {
            return Err(Error::new("limit_exceeded", "Git stdin exceeds byte limit"));
        }
        for (key, value) in extra_env {
            if ![
                "GIT_AUTHOR_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_AUTHOR_DATE",
                "GIT_COMMITTER_NAME",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_DATE",
                "GIT_INDEX_FILE",
            ]
            .contains(&key.as_str())
                || value.contains('\0')
            {
                return Err(Error::new(
                    "invalid_environment",
                    "unsupported Git environment override",
                ));
            }
            if key == "GIT_INDEX_FILE"
                && (!Path::new(value).is_absolute()
                    || Path::new(value)
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir)))
            {
                return Err(Error::new(
                    "invalid_environment",
                    "index file must be an absolute internal path",
                ));
            }
        }
        if !self.git.is_absolute() {
            return Err(Error::new("invalid_git", "Git executable must be absolute"));
        }
        if args.iter().any(|argument| argument.contains('\0')) {
            return Err(Error::new("invalid_argument", "NUL in Git argument"));
        }
        let c = &self.counters;
        // A local nested scope makes coordination-lock admission consume the same deadline.
        let _scope = DeadlineScope::new(deadline.saturating_duration_since(Instant::now()));
        // Refuse pre-existing unsafe storage before any Git child can access it.
        // Only transient links created by the owned child are tolerated by the
        // live sampler; both admission and final validation remain strict.
        if let Some((root, limit)) = disk_budget
            && directory_bytes(root, limit)? > limit
        {
            return Err(Error::new(
                "storage_limit",
                "managed storage already exceeds quota",
            ));
        }
        if Instant::now() >= deadline {
            return Err(Error::new(
                timeout_code,
                "Git deadline expired during storage preflight",
            ));
        }
        c.acquire_child(self.limits.max_children, deadline)?;
        struct Slot<'a>(&'a Counters, Instant);
        impl Drop for Slot<'_> {
            fn drop(&mut self) {
                self.0.release_child();
                self.0.duration.fetch_add(
                    self.1.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
            }
        }
        let _slot = Slot(c, Instant::now());
        if Instant::now() >= deadline {
            return Err(Error::new(
                "deadline_exceeded",
                "request deadline expired after Git admission",
            ));
        }
        let prepared = self.command(args, disk_budget.map(|(_, limit)| limit))?;
        let helper_nonce = prepared.helper_nonce;
        let mut command = prepared.command;
        if let Some(auth) = remote_auth {
            auth.configure(&mut command);
        }
        command
            .envs(extra_env.iter().map(|(key, value)| (key, value)))
            .current_dir(cwd)
            .stdin(if input.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if Instant::now() >= deadline {
            return Err(Error::new(
                "deadline_exceeded",
                "request deadline expired before Git spawn",
            ));
        }
        let child = command
            .spawn()
            .map_err(|_| Error::new("git_spawn", "could not spawn configured Git executable"))?;
        c.spawns.fetch_add(1, Ordering::Relaxed);
        let mut owner = OwnedChild::new(child);
        let mut pipes = PipePump::new(&mut owner, input, sink, self.limits.max_output_bytes)?;
        let mut notification = ChildNotification::new(owner.pid, self.use_pidfd)?;
        match &notification {
            ChildNotification::Pidfd(_) => {
                c.pidfd_runs.fetch_add(1, Ordering::Relaxed);
            }
            ChildNotification::Waiter { .. } => {
                c.waiter_fallback_runs.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut answer = pipes.run(
            &mut owner,
            &notification,
            deadline,
            timeout_code,
            disk_budget,
        );
        let status = owner.finish();
        notification.join();
        if let Err(error) = pipes.sync_sink() {
            answer = Err(error);
        }
        c.bytes.fetch_add(pipes.bytes, Ordering::Relaxed);
        if let Err(error) = answer {
            if ["git_timeout", "deadline_exceeded"].contains(&error.code.as_str()) {
                c.timeouts.fetch_add(1, Ordering::Relaxed);
            }
            if error.code == "output_limit" {
                c.output_limits.fetch_add(1, Ordering::Relaxed);
            }
            return Err(error);
        }
        if let Some((root, limit)) = disk_budget
            && directory_bytes(root, limit)? > limit
        {
            return Err(Error::new(
                "storage_limit",
                "Git exceeded managed storage quota",
            ));
        }
        let status = status?;
        if status.code() == Some(126)
            && let Some(nonce) = helper_nonce
            && let Some(error) = decode_helper_failure(&nonce, &pipes.stderr_bytes)
        {
            return Err(error);
        }
        Ok(GitOutput {
            status,
            stdout: pipes.stdout_bytes,
            stderr: pipes.stderr_bytes,
        })
    }
}

pub const MAX_CHILD_WAITERS: usize = 64;
#[derive(Default)]
struct Admission {
    next_ticket: u64,
    queue: std::collections::VecDeque<u64>,
}
impl Counters {
    fn acquire_child(&self, limit: usize, deadline: Instant) -> Result<()> {
        if limit == 0 {
            return Err(Error::new(
                "invalid_configuration",
                "Git child limit must be positive",
            ));
        }
        let mut admission = lock_with_deadline(&self.admission)?;
        if Instant::now() >= deadline {
            return Err(Error::new(
                "deadline_exceeded",
                "Git child admission deadline expired",
            ));
        }
        if admission.queue.is_empty() && self.try_acquire_child(limit) {
            return Ok(());
        }
        if admission.queue.len() >= MAX_CHILD_WAITERS {
            return Err(Error::new("overloaded", "Git child wait queue is full"));
        }
        let mut ticket = admission.next_ticket;
        while admission.queue.contains(&ticket) {
            ticket = ticket.wrapping_add(1);
        }
        admission.next_ticket = ticket.wrapping_add(1);
        admission.queue.push_back(ticket);
        self.queued.fetch_add(1, Ordering::Relaxed);
        let waiting = self.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_waiters.fetch_max(waiting, Ordering::Relaxed);
        let queued_at = Instant::now();
        loop {
            if Instant::now() >= deadline {
                admission.queue.retain(|queued| *queued != ticket);
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                self.queue_wait_micros.fetch_add(
                    queued_at.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                self.queue_timeouts.fetch_add(1, Ordering::Relaxed);
                self.timeouts.fetch_add(1, Ordering::Relaxed);
                self.available.notify_all();
                return Err(Error::new(
                    "deadline_exceeded",
                    "Git child admission deadline expired",
                ));
            }
            if admission.queue.front() == Some(&ticket) && self.try_acquire_child(limit) {
                admission.queue.pop_front();
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                self.queue_wait_micros.fetch_add(
                    queued_at.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                self.available.notify_all();
                return Ok(());
            }
            let duration = deadline.saturating_duration_since(Instant::now());
            admission = match self.available.wait_timeout(admission, duration) {
                Ok((guard, _)) => guard,
                Err(poison) => {
                    let (mut guard, _) = poison.into_inner();
                    guard.queue.retain(|queued| *queued != ticket);
                    self.waiting.fetch_sub(1, Ordering::Relaxed);
                    self.available.notify_all();
                    return Err(Error::new(
                        "recovery_blocked",
                        "Git child admission lock is poisoned",
                    ));
                }
            };
        }
    }
    pub(crate) fn try_acquire_child(&self, limit: usize) -> bool {
        let mut active = self.active.load(Ordering::Relaxed);
        loop {
            if active >= limit {
                return false;
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(value) => active = value,
            }
        }
    }
    pub(crate) fn release_child(&self) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.active.fetch_sub(1, Ordering::Release);
        self.available.notify_all();
    }
}

struct OwnedChild {
    child: Option<std::process::Child>,
    pid: i32,
    group_stopped: bool,
}
impl OwnedChild {
    fn new(child: std::process::Child) -> Self {
        let pid = child.id() as i32;
        Self {
            child: Some(child),
            pid,
            group_stopped: false,
        }
    }
    fn exited(&self) -> Result<bool> {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(Error::new("git_wait", "could not inspect owned Git child"));
            }
        }
    }
    fn stop_group(&mut self) {
        if !self.group_stopped {
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
            self.group_stopped = true;
        }
    }
    fn finish(&mut self) -> Result<ExitStatus> {
        self.stop_group();
        let mut child = self
            .child
            .take()
            .ok_or_else(|| Error::new("git_wait", "Git child already reaped"))?;
        child
            .wait()
            .map_err(|_| Error::new("git_wait", "could not reap Git child"))
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.finish();
        }
    }
}

enum ChildNotification {
    Pidfd(std::fs::File),
    Waiter {
        socket: std::os::unix::net::UnixStream,
        thread: Option<std::thread::JoinHandle<()>>,
    },
}
impl ChildNotification {
    fn new(pid: i32, use_pidfd: bool) -> Result<Self> {
        use std::os::fd::FromRawFd;
        if use_pidfd {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd >= 0 {
                return Ok(Self::Pidfd(unsafe {
                    std::fs::File::from_raw_fd(fd as i32)
                }));
            }
        }
        let (read, mut write) = std::os::unix::net::UnixStream::pair()?;
        let thread = std::thread::Builder::new()
            .name("aq-git-child-wait".into())
            .spawn(move || {
                loop {
                    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    let result = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid as u32,
                            &mut info,
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if result == 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
                let _ = write.write_all(&[1]);
            })?;
        Ok(Self::Waiter {
            socket: read,
            thread: Some(thread),
        })
    }
    fn fd(&self) -> i32 {
        match self {
            Self::Pidfd(file) => file.as_raw_fd(),
            Self::Waiter { socket, .. } => socket.as_raw_fd(),
        }
    }
    fn join(&mut self) {
        if let Self::Waiter { thread, .. } = self
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}
struct Sink {
    file: std::fs::File,
    limit: usize,
    written: usize,
}
struct PipePump<'a> {
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    input: &'a [u8],
    offset: usize,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
    buffered: usize,
    limit: usize,
    sink: Option<Sink>,
    bytes: u64,
}
fn nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
impl<'a> PipePump<'a> {
    fn new(
        owner: &mut OwnedChild,
        input: &'a [u8],
        sink: Option<(std::fs::File, usize)>,
        limit: usize,
    ) -> Result<Self> {
        let child = owner.child.as_mut().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        for fd in [
            stdin.as_ref().map(AsRawFd::as_raw_fd),
            stdout.as_ref().map(AsRawFd::as_raw_fd),
            stderr.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .flatten()
        {
            nonblocking(fd)?;
        }
        Ok(Self {
            stdin,
            stdout,
            stderr,
            input,
            offset: 0,
            stdout_bytes: Vec::new(),
            stderr_bytes: Vec::new(),
            buffered: 0,
            limit,
            sink: sink.map(|(file, limit)| Sink {
                file,
                limit,
                written: 0,
            }),
            bytes: 0,
        })
    }
    fn sync_sink(&mut self) -> Result<()> {
        if let Some(sink) = &self.sink {
            sink.file.sync_all()?;
        }
        Ok(())
    }
    fn run(
        &mut self,
        owner: &mut OwnedChild,
        notification: &ChildNotification,
        deadline: Instant,
        timeout_code: &str,
        disk_budget: Option<(&Path, u64)>,
    ) -> Result<()> {
        let mut next_disk_check = Instant::now();
        loop {
            if Instant::now() >= deadline {
                return Err(Error::new(
                    timeout_code,
                    "Git command exceeded its execution deadline",
                ));
            }
            if let Some((root, limit)) = disk_budget
                && Instant::now() >= next_disk_check
            {
                if sampled_directory_bytes(root, limit, true)? > limit {
                    return Err(Error::new(
                        "storage_limit",
                        "Git exceeded managed storage quota",
                    ));
                }
                next_disk_check = Instant::now() + Duration::from_millis(2);
            }
            let out = read_available(
                &mut self.stdout,
                &mut self.stdout_bytes,
                self.sink.as_mut(),
                &mut self.buffered,
                self.limit,
                &mut self.bytes,
            )?;
            let err = read_available(
                &mut self.stderr,
                &mut self.stderr_bytes,
                None,
                &mut self.buffered,
                self.limit,
                &mut self.bytes,
            )?;
            if owner.exited()? {
                owner.stop_group();
                self.stdin.take();
                // Drain already-buffered output, but do not wait on an escaped helper's open pipe.
                let tail_out = read_available(
                    &mut self.stdout,
                    &mut self.stdout_bytes,
                    self.sink.as_mut(),
                    &mut self.buffered,
                    self.limit,
                    &mut self.bytes,
                )?;
                let tail_err = read_available(
                    &mut self.stderr,
                    &mut self.stderr_bytes,
                    None,
                    &mut self.buffered,
                    self.limit,
                    &mut self.bytes,
                )?;
                if !out && !err && !tail_out && !tail_err {
                    return Ok(());
                }
                continue;
            }
            if let Some(stdin) = &mut self.stdin {
                let end = self.input.len().min(self.offset.saturating_add(65536));
                match stdin.write(&self.input[self.offset..end]) {
                    Ok(0) => {
                        self.stdin.take();
                    }
                    Ok(n) => {
                        self.offset += n;
                        if self.offset == self.input.len() {
                            self.stdin.take();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                        self.stdin.take();
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let mut poll = [
                libc::pollfd {
                    fd: notification.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLOUT,
                    revents: 0,
                },
            ];
            let wake = if disk_budget.is_some() {
                deadline.min(next_disk_check)
            } else {
                deadline
            };
            let remaining = wake.saturating_duration_since(Instant::now());
            let timeout = libc::timespec {
                tv_sec: remaining.as_secs().min(i64::MAX as u64) as libc::time_t,
                tv_nsec: remaining.subsec_nanos() as libc::c_long,
            };
            let result = unsafe {
                libc::ppoll(
                    poll.as_mut_ptr(),
                    poll.len() as libc::nfds_t,
                    &timeout,
                    std::ptr::null(),
                )
            };
            if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if poll.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err(Error::new(
                    "io_error",
                    "invalid Git pipe notification descriptor",
                ));
            }
        }
    }
}
fn read_available<R: Read>(
    pipe: &mut Option<R>,
    output: &mut Vec<u8>,
    mut sink: Option<&mut Sink>,
    buffered: &mut usize,
    limit: usize,
    bytes: &mut u64,
) -> Result<bool> {
    let mut progressed = false;
    let mut consumed = 0;
    while consumed < 65536 {
        let Some(reader) = pipe.as_mut() else { break };
        let mut buffer = [0u8; 8192];
        match reader.read(&mut buffer) {
            Ok(0) => {
                pipe.take();
                break;
            }
            Ok(n) => {
                progressed = true;
                consumed += n;
                *bytes = bytes.saturating_add(n as u64);
                let available = if let Some(sink) = sink.as_ref() {
                    sink.limit.saturating_sub(sink.written)
                } else {
                    limit.saturating_sub(*buffered)
                };
                let keep = n.min(available);
                if let Some(sink) = sink.as_mut() {
                    sink.file.write_all(&buffer[..keep])?;
                    sink.written += keep;
                } else {
                    output.extend_from_slice(&buffer[..keep]);
                    *buffered += keep;
                }
                if keep < n {
                    return Err(Error::new(
                        "output_limit",
                        "Git command exceeded output budget",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(progressed)
}

/// Counts allocated managed bytes without following symlinks. Stops as soon as the budget is exceeded.
pub fn directory_bytes(path: &Path, limit: u64) -> Result<u64> {
    sampled_directory_bytes(path, limit, false)
}

// Git init briefly creates a filesystem-capability probe symlink. During an owned
// child, charge the link itself without traversing it; the mandatory post-exit
// scan remains strict, so no residual symlink can enter accepted managed storage.
fn sampled_directory_bytes(path: &Path, limit: u64, active_child: bool) -> Result<u64> {
    let root = path;
    let mut total = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = match entry.path().symlink_metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if meta.file_type().is_symlink() && !active_child {
                use std::os::unix::ffi::OsStrExt;
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or_else(|_| Path::new("<outside-managed-root>"));
                return Err(Error::new("invalid_path", "symlink in managed storage").details(serde_json::json!({"origin":"directory_bytes","entry_encoding":"unix_bytes_hex","relative_entry_hex":hex::encode(relative.as_os_str().as_bytes()),"entry_type":"symlink"})));
            }
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
                if total > limit {
                    return Ok(total);
                }
            }
        }
    }
    Ok(total)
}

fn wait_for_lock<G>(mut acquire: impl FnMut() -> std::sync::TryLockResult<G>) -> Result<G> {
    loop {
        if remaining_deadline().is_some_and(|remaining| remaining.is_zero()) {
            return Err(Error::new(
                "deadline_exceeded",
                "request deadline expired waiting for coordination",
            ));
        }
        match acquire() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(Error::new(
                    "recovery_blocked",
                    "coordination lock is poisoned",
                ));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(1))
            }
        }
    }
}
pub fn lock_with_deadline<T>(lock: &std::sync::Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    wait_for_lock(|| lock.try_lock())
}
pub fn read_with_deadline<T>(
    lock: &std::sync::RwLock<T>,
) -> Result<std::sync::RwLockReadGuard<'_, T>> {
    wait_for_lock(|| lock.try_read())
}
pub fn write_with_deadline<T>(
    lock: &std::sync::RwLock<T>,
) -> Result<std::sync::RwLockWriteGuard<'_, T>> {
    wait_for_lock(|| lock.try_write())
}

const SPAWN_HELPER_MARKER: &str = "__aq_git_spawn_v1";
const SPAWN_HELPER_FAILURE: &[u8] = b"\0aq-git-spawn-v1\0";
pub(crate) fn decode_helper_failure(nonce: &[u8; 16], bytes: &[u8]) -> Option<Error> {
    if bytes.len() != SPAWN_HELPER_FAILURE.len() + 17 {
        return None;
    }
    let rest = bytes.strip_prefix(SPAWN_HELPER_FAILURE)?;
    if &rest[..16] != nonce || !b"1234".contains(&rest[16]) {
        return None;
    }
    let error = Error::new(
        if rest[16] == b'2' {
            "unsupported_feature"
        } else {
            "git_spawn"
        },
        "internal spawn helper refused Git execution",
    );
    Some(if rest[16] == b'2' {
        error.details(serde_json::json!({"restart_required":true}))
    } else {
        error
    })
}
fn helper_fail(nonce: Option<&[u8; 16]>, code: u8) -> i32 {
    // A single bounded machine frame, bound to this spawn and not forwarded to
    // Git. Static stack storage only, including after resource limits apply.
    let mut frame = [0u8; SPAWN_HELPER_FAILURE.len() + 17];
    frame[..SPAWN_HELPER_FAILURE.len()].copy_from_slice(SPAWN_HELPER_FAILURE);
    if let Some(nonce) = nonce {
        frame[SPAWN_HELPER_FAILURE.len()..SPAWN_HELPER_FAILURE.len() + 16].copy_from_slice(nonce);
    }
    frame[SPAWN_HELPER_FAILURE.len() + 16] = code;
    unsafe {
        libc::write(libc::STDERR_FILENO, frame.as_ptr().cast(), frame.len());
    }
    126
}
/// Hidden same-binary entry, called before CLI parsing, runtime or root ownership.
/// Returns None for every ordinary invocation. Successful execution never returns.
pub fn run_spawn_helper_if_requested() -> Option<i32> {
    use std::os::unix::ffi::OsStrExt;
    let mut arguments = std::env::args_os();
    let executable = arguments.next();
    let marker = arguments.next();
    if marker.as_deref() != Some(std::ffi::OsStr::new(SPAWN_HELPER_MARKER)) {
        return None;
    }
    let mut args = vec![executable.unwrap_or_default(), marker.unwrap()];
    args.extend(arguments);
    if args.len() < 7 {
        return Some(helper_fail(None, b'1'));
    }
    let token = match args[5].to_str().and_then(|s| uuid::Uuid::parse_str(s).ok()) {
        Some(token) => token,
        None => return Some(helper_fail(None, b'1')),
    };
    let nonce = Some(token.as_bytes());
    let memory = match args[2]
        .to_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
    {
        Some(n) => n,
        None => return Some(helper_fail(nonce, b'1')),
    };
    let file_limit = if args[3] == "none" {
        None
    } else {
        match args[3].to_str().and_then(|s| s.parse::<u64>().ok()) {
            Some(n) => Some(n),
            None => return Some(helper_fail(nonce, b'1')),
        }
    };
    let expected: ExecutableIdentity =
        match args[4].to_str().and_then(|s| serde_json::from_str(s).ok()) {
            Some(identity) => identity,
            None => return Some(helper_fail(nonce, b'1')),
        };
    let git = Path::new(&args[6]);
    if !git.is_absolute() || ExecutableIdentity::read(git).ok().as_ref() != Some(&expected) {
        return Some(helper_fail(nonce, b'2'));
    }
    let program = match std::ffi::CString::new(args[6].as_bytes()) {
        Ok(s) => s,
        Err(_) => return Some(helper_fail(nonce, b'1')),
    };
    let mut argv = Vec::with_capacity(args.len() - 6);
    argv.push(program.clone());
    for arg in &args[7..] {
        match std::ffi::CString::new(arg.as_bytes()) {
            Ok(arg) => argv.push(arg),
            Err(_) => return Some(helper_fail(nonce, b'1')),
        }
    }
    let mut environment = Vec::new();
    for (key, value) in std::env::vars_os() {
        let mut entry = key.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        match std::ffi::CString::new(entry) {
            Ok(entry) => environment.push(entry),
            Err(_) => return Some(helper_fail(nonce, b'1')),
        }
    }
    let mut argv_ptr = argv.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    argv_ptr.push(std::ptr::null());
    let mut env_ptr = environment.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
    env_ptr.push(std::ptr::null());
    // Everything that can allocate is prepared. Limits apply BEFORE actual Git
    // exec, without a parent-side prlimit race or an unsafe custom vfork callback.
    unsafe {
        let bound = libc::rlimit {
            rlim_cur: memory,
            rlim_max: memory,
        };
        if libc::setrlimit(libc::RLIMIT_AS, &bound) != 0 {
            let status = helper_fail(nonce, b'3');
            libc::_exit(status);
        }
        if let Some(size) = file_limit {
            let bound = libc::rlimit {
                rlim_cur: size,
                rlim_max: size,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &bound) != 0 {
                let status = helper_fail(nonce, b'3');
                libc::_exit(status);
            }
        }
        if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
            let status = helper_fail(nonce, b'3');
            libc::_exit(status);
        }
        libc::execve(program.as_ptr(), argv_ptr.as_ptr(), env_ptr.as_ptr());
        let status = helper_fail(nonce, b'4');
        libc::_exit(status);
    }
}
