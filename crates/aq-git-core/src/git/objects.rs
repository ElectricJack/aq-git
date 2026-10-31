//! Bounded persistent object reader with exact framing and discard on failure.
use super::GitRunner;
use crate::{Error, Result};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio},
    sync::{Mutex, atomic::Ordering},
    time::Instant,
};
#[derive(Default)]
pub struct ObjectReaders {
    readers: Mutex<BTreeMap<PathBuf, Reader>>,
    exchanges: std::sync::atomic::AtomicU64,
}
struct Reader {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    stderr: Option<ChildStderr>,
    stderr_bytes: Vec<u8>,
    helper_nonce: Option<[u8; 16]>,
    runner: GitRunner,
}
impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
        self.runner.counters.release_child();
    }
}
impl Reader {
    fn spawn(runner: &GitRunner, path: &Path) -> Result<Self> {
        let c = &runner.counters;
        let mut n = c.active.load(Ordering::Relaxed);
        loop {
            if n >= runner.limits.max_children {
                return Err(Error::new("overloaded", "Git child limit reached"));
            }
            match c
                .active
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(v) => n = v,
            }
        }
        let prepared = match runner.command(["cat-file", "--batch-command"], None) {
            Ok(command) => command,
            Err(error) => {
                c.release_child();
                return Err(error);
            }
        };
        let helper_nonce = prepared.helper_nonce;
        let mut cmd = prepared.command;
        cmd.current_dir(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                c.release_child();
                return Err(Error::new("git_spawn", "Cannot launch object reader"));
            }
        };
        c.spawns.fetch_add(1, Ordering::Relaxed);
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let stderr = child.stderr.take();
        let reader = Self {
            child,
            input,
            output,
            stderr,
            stderr_bytes: Vec::new(),
            helper_nonce,
            runner: runner.clone(),
        };
        for fd in [
            Some(reader.output.as_raw_fd()),
            reader.stderr.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .flatten()
        {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(reader)
    }
    fn drain_diagnostics(&mut self) -> Result<()> {
        let limit = 256usize.min(self.runner.limits.max_output_bytes);
        loop {
            let Some(stderr) = &mut self.stderr else {
                return Ok(());
            };
            let mut bytes = [0u8; 64];
            match stderr.read(&mut bytes) {
                Ok(0) => {
                    self.stderr = None;
                    return Ok(());
                }
                Ok(n) => {
                    if n > limit.saturating_sub(self.stderr_bytes.len()) {
                        return Err(Error::new(
                            "output_limit",
                            "Object reader diagnostics exceeded byte budget",
                        ));
                    }
                    self.stderr_bytes.extend_from_slice(&bytes[..n]);
                    self.runner
                        .counters
                        .bytes
                        .fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn setup_failure(&mut self, start: Instant) -> Result<Option<Error>> {
        self.drain_diagnostics()?;
        let Some(nonce) = self.helper_nonce else {
            return Ok(None);
        };
        let Some(_) = super::runner::decode_helper_failure(&nonce, &self.stderr_bytes) else {
            return Ok(None);
        };
        // Validate the reserved status without reaping. Drop remains the sole
        // reaper and kills the still-owned process group before releasing PID.
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, self.child.id() as i32, 0) };
        let notification = if fd >= 0 {
            Some(unsafe { std::fs::File::from_raw_fd(fd as i32) })
        } else {
            None
        };
        loop {
            let remaining = self
                .runner
                .limits
                .timeout
                .saturating_sub(start.elapsed())
                .min(super::remaining_deadline().unwrap_or(std::time::Duration::MAX));
            if remaining.is_zero() {
                return Err(Error::new(
                    if super::remaining_deadline().is_some_and(|d| d.is_zero()) {
                        "deadline_exceeded"
                    } else {
                        "git_timeout"
                    },
                    "Object reader helper exit deadline expired",
                ));
            }
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result < 0 {
                let failure = std::io::Error::last_os_error();
                if failure.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::new(
                    "git_wait",
                    "Cannot inspect object reader helper exit",
                ));
            }
            if unsafe { info.si_pid() } != 0 {
                self.drain_diagnostics()?;
                return Ok(
                    if info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 126 {
                        super::runner::decode_helper_failure(&nonce, &self.stderr_bytes)
                    } else {
                        None
                    },
                );
            }
            if let Some(notification) = &notification {
                let mut descriptor = libc::pollfd {
                    fd: notification.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let wait = remaining.min(std::time::Duration::from_millis(10));
                let timeout = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: wait.as_nanos() as libc::c_long,
                };
                unsafe {
                    libc::ppoll(&mut descriptor, 1, &timeout, std::ptr::null());
                }
            } else {
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
            }
        }
    }
    fn read(&mut self, buf: &mut [u8], start: Instant) -> Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            if super::remaining_deadline().is_some_and(|d| d.is_zero()) {
                return Err(Error::new(
                    "deadline_exceeded",
                    "Object reader request deadline expired",
                ));
            }
            if start.elapsed() >= self.runner.limits.timeout {
                return Err(Error::new("git_timeout", "Object reader deadline expired"));
            }
            match self.output.read(&mut buf[offset..]) {
                Ok(0) => {
                    return Err(self.setup_failure(start)?.unwrap_or_else(|| {
                        Error::new("git_failed", "Object reader exited during frame")
                    }));
                }
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.drain_diagnostics()?;
                    let mut descriptors = [
                        libc::pollfd {
                            fd: self.output.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: self.stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    unsafe {
                        libc::poll(
                            descriptors.as_mut_ptr(),
                            descriptors.len() as libc::nfds_t,
                            10,
                        );
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn object(&mut self, oid: &str, limit: usize) -> Result<(String, Vec<u8>)> {
        if super::remaining_deadline().is_some_and(|d| d.is_zero()) {
            return Err(Error::new(
                "deadline_exceeded",
                "Object reader request deadline expired",
            ));
        }
        let start = Instant::now();
        self.stderr_bytes.clear();
        if let Err(error) = self
            .input
            .write_all(format!("contents {oid}\n").as_bytes())
            .and_then(|_| self.input.flush())
        {
            return Err(self.setup_failure(start)?.unwrap_or_else(|| error.into()));
        }
        let mut header = Vec::new();
        loop {
            let mut byte = [0];
            self.read(&mut byte, start)?;
            if byte[0] == b'\n' {
                break;
            }
            header.push(byte[0]);
            if header.len() > 256 {
                return Err(Error::new("git_failed", "Malformed object header"));
            }
        }
        let header = std::str::from_utf8(&header)
            .map_err(|_| Error::new("git_failed", "Malformed object header"))?;
        let fields = header.split(' ').collect::<Vec<_>>();
        if fields.len() == 2 && fields[1] == "missing" {
            return Err(Error::new(
                "object_unavailable",
                "Exact object unavailable; synchronize or import it",
            ));
        }
        if fields.len() != 3 || fields[0] != oid {
            return Err(Error::new("git_failed", "Mismatched object frame"));
        }
        let len: usize = fields[2]
            .parse()
            .map_err(|_| Error::new("git_failed", "Malformed object size"))?;
        if len > limit {
            return Err(Error::new(
                "limit_exceeded",
                "Object exceeds metadata budget",
            ));
        }
        let kind = fields[1].to_owned();
        let mut data = vec![0; len];
        self.read(&mut data, start)?;
        let mut delimiter = [0];
        self.read(&mut delimiter, start)?;
        if delimiter[0] != b'\n' {
            return Err(Error::new("git_failed", "Malformed object delimiter"));
        }
        self.drain_diagnostics()?;
        self.runner
            .counters
            .bytes
            .fetch_add((len + header.len() + 2) as u64, Ordering::Relaxed);
        Ok((kind, data))
    }
}
impl ObjectReaders {
    pub fn exchanges(&self) -> u64 {
        self.exchanges.load(Ordering::Relaxed)
    }
    pub fn invalidate(&self, path: &Path) {
        self.readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path);
    }
    pub fn clear(&self) {
        self.readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear()
    }
    pub fn object(
        &self,
        runner: &GitRunner,
        path: &Path,
        oid: &str,
        limit: usize,
    ) -> Result<(String, Vec<u8>)> {
        if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::new(
                "invalid_request",
                "Object reader requires full hexadecimal ID",
            ));
        }
        let mut readers = super::lock_with_deadline(&self.readers)?;
        let persistent_limit = 2.min(runner.limits.max_children.saturating_sub(1));
        if persistent_limit == 0 {
            readers.clear();
            let mut reader = Reader::spawn(runner, path)?;
            self.exchanges.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let result = reader.object(oid, limit);
            runner.counters.duration.fetch_add(
                started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
            if result.as_ref().is_err_and(|error| {
                ["git_timeout", "deadline_exceeded"].contains(&error.code.as_str())
            }) {
                runner.counters.timeouts.fetch_add(1, Ordering::Relaxed);
            }
            return result;
        }
        if !readers.contains_key(path) {
            if readers.len() >= persistent_limit
                && let Some(k) = readers.keys().next().cloned()
            {
                readers.remove(&k);
            }
            readers.insert(path.to_owned(), Reader::spawn(runner, path)?);
        }
        self.exchanges.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let result = readers.get_mut(path).unwrap().object(oid, limit);
        runner.counters.duration.fetch_add(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if result
            .as_ref()
            .is_err_and(|error| ["git_timeout", "deadline_exceeded"].contains(&error.code.as_str()))
        {
            runner.counters.timeouts.fetch_add(1, Ordering::Relaxed);
        }
        if result.is_err() {
            readers.remove(path);
        }
        result
    }
}
