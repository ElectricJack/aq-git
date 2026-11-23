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
mod admission;
mod execution;
mod pipes;
mod spawn_helper;
use admission::Admission;
pub use admission::MAX_CHILD_WAITERS;
use execution::RunOptions;
use spawn_helper::SPAWN_HELPER_MARKER;
pub(crate) use spawn_helper::decode_helper_failure;
pub use spawn_helper::run_spawn_helper_if_requested;
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
        self.run_internal(
            cwd,
            args,
            RunOptions {
                input,
                extra_env,
                ..RunOptions::new(timeout)
            },
        )
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
            RunOptions {
                sink: Some((file, max_bytes)),
                ..RunOptions::new(self.limits.timeout)
            },
        )
    }
    pub fn run_with_disk_budget(
        &self,
        cwd: &Path,
        args: &[String],
        budget_root: &Path,
        max_bytes: u64,
    ) -> Result<GitOutput> {
        self.run_with_disk_budget_env(cwd, args, &[], budget_root, max_bytes)
    }
    /// Like [`Self::run_with_disk_budget`] with internal environment overrides,
    /// for example a private object directory for one child.
    pub fn run_with_disk_budget_env(
        &self,
        cwd: &Path,
        args: &[String],
        extra_env: &[(String, String)],
        budget_root: &Path,
        max_bytes: u64,
    ) -> Result<GitOutput> {
        self.run_internal(
            cwd,
            args,
            RunOptions {
                extra_env,
                disk_budget: Some((budget_root, max_bytes)),
                ..RunOptions::new(self.limits.timeout)
            },
        )
    }
    pub(crate) fn run_remote(
        &self,
        (cwd, args): (&Path, &[String]),
        auth: &super::credentials::RemoteAuth,
        timeout: Duration,
    ) -> Result<GitOutput> {
        let args = auth.arguments(args);
        let output = self.run_internal(
            cwd,
            &args,
            RunOptions {
                remote_auth: Some(auth),
                ..RunOptions::new(timeout)
            },
        )?;
        auth.finish(output)
    }
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
            check_storage_entry(root, &entry.path(), &meta, active_child)?;
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

fn check_storage_entry(
    root: &Path,
    path: &Path,
    meta: &std::fs::Metadata,
    active_child: bool,
) -> Result<()> {
    if meta.file_type().is_symlink() && !active_child {
        use std::os::unix::ffi::OsStrExt;

        let relative = path
            .strip_prefix(root)
            .unwrap_or_else(|_| Path::new("<outside-managed-root>"));
        return Err(Error::new("invalid_path", "symlink in managed storage").details(serde_json::json!({"origin":"directory_bytes","entry_encoding":"unix_bytes_hex","relative_entry_hex":hex::encode(relative.as_os_str().as_bytes()),"entry_type":"symlink"})));
    }
    Ok(())
}
