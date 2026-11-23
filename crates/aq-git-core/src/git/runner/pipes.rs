use super::execution::ExecutionBudget;
use super::*;
pub(super) struct OwnedChild {
    child: Option<std::process::Child>,
    pub(super) pid: i32,
    group_stopped: bool,
}
impl OwnedChild {
    pub(super) fn new(child: std::process::Child) -> Self {
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
    pub(super) fn finish(&mut self) -> Result<ExitStatus> {
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

pub(super) enum ChildNotification {
    Pidfd(std::fs::File),
    Waiter {
        socket: std::os::unix::net::UnixStream,
        thread: Option<std::thread::JoinHandle<()>>,
    },
}
impl ChildNotification {
    pub(super) fn new(pid: i32, use_pidfd: bool) -> Result<Self> {
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
    pub(super) fn join(&mut self) {
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
pub(super) struct PipePump<'a> {
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    input: &'a [u8],
    offset: usize,
    pub(super) stdout_bytes: Vec<u8>,
    pub(super) stderr_bytes: Vec<u8>,
    pub(super) counts: OutputCounts,
    sink: Option<Sink>,
}
fn nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
impl<'a> PipePump<'a> {
    pub(super) fn new(
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
            counts: OutputCounts {
                buffered: 0,
                limit,
                bytes: 0,
            },
            sink: sink.map(|(file, limit)| Sink {
                file,
                limit,
                written: 0,
            }),
        })
    }
    pub(super) fn sync_sink(&mut self) -> Result<()> {
        if let Some(sink) = &self.sink {
            sink.file.sync_all()?;
        }
        Ok(())
    }
    pub(super) fn run(
        &mut self,
        owner: &mut OwnedChild,
        notification: &ChildNotification,
        budget: ExecutionBudget<'_>,
    ) -> Result<()> {
        let mut next_disk_check = Instant::now();
        loop {
            if self.pump_once(owner, notification, (budget, &mut next_disk_check))? {
                return Ok(());
            }
        }
    }
    fn pump_once(
        &mut self,
        owner: &mut OwnedChild,
        notification: &ChildNotification,
        (budget, next_disk_check): (ExecutionBudget<'_>, &mut Instant),
    ) -> Result<bool> {
        if Instant::now() >= budget.deadline {
            return Err(Error::new(
                budget.timeout_code,
                "Git command exceeded its execution deadline",
            ));
        }
        *next_disk_check = next_storage_sample(budget.disk_budget, *next_disk_check)?;
        let progressed = self.read_outputs()?;
        if owner.exited()? {
            return self.finish_pipes(owner, progressed);
        }
        if self.write_input()? {
            return Ok(false);
        }
        let wake = if budget.disk_budget.is_some() {
            budget.deadline.min(*next_disk_check)
        } else {
            budget.deadline
        };
        self.poll(notification, wake)?;
        Ok(false)
    }
    fn finish_pipes(&mut self, owner: &mut OwnedChild, progressed: bool) -> Result<bool> {
        owner.stop_group();
        self.stdin.take();
        // Drain buffered output without waiting on an escaped helper's open pipe.
        let tail = self.read_outputs()?;
        Ok(!tail && !progressed)
    }
    fn read_outputs(&mut self) -> Result<bool> {
        let out = self.counts.read_available(
            &mut self.stdout,
            &mut self.stdout_bytes,
            self.sink.as_mut(),
        )?;
        let err = self
            .counts
            .read_available(&mut self.stderr, &mut self.stderr_bytes, None)?;
        Ok(out || err)
    }
    fn write_input(&mut self) -> Result<bool> {
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
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                    self.stdin.take();
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }
    fn poll(&self, notification: &ChildNotification, wake: Instant) -> Result<()> {
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
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
        if poll.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
            return Err(Error::new(
                "io_error",
                "invalid Git pipe notification descriptor",
            ));
        }
        Ok(())
    }
}
fn next_storage_sample(budget: Option<(&Path, u64)>, next: Instant) -> Result<Instant> {
    if let Some((root, limit)) = budget
        && Instant::now() >= next
    {
        if sampled_directory_bytes(root, limit, true)? > limit {
            return Err(Error::new(
                "storage_limit",
                "Git exceeded managed storage quota",
            ));
        }
        return Ok(Instant::now() + Duration::from_millis(2));
    }
    Ok(next)
}
pub(super) struct OutputCounts {
    buffered: usize,
    limit: usize,
    pub(super) bytes: u64,
}
impl OutputCounts {
    fn read_available<R: Read>(
        &mut self,
        pipe: &mut Option<R>,
        output: &mut Vec<u8>,
        mut sink: Option<&mut Sink>,
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
                    self.append(&buffer[..n], output, sink.as_deref_mut())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(progressed)
    }
    fn append(
        &mut self,
        buffer: &[u8],
        output: &mut Vec<u8>,
        mut sink: Option<&mut Sink>,
    ) -> Result<()> {
        let n = buffer.len();
        self.bytes = self.bytes.saturating_add(n as u64);
        let available = if let Some(sink) = sink.as_ref() {
            sink.limit.saturating_sub(sink.written)
        } else {
            self.limit.saturating_sub(self.buffered)
        };
        let keep = n.min(available);
        if let Some(sink) = sink.as_mut() {
            sink.file.write_all(&buffer[..keep])?;
            sink.written += keep;
        } else {
            output.extend_from_slice(&buffer[..keep]);
            self.buffered += keep;
        }
        if keep < n {
            return Err(Error::new(
                "output_limit",
                "Git command exceeded output budget",
            ));
        }
        Ok(())
    }
}
