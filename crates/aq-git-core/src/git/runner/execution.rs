use super::pipes::{ChildNotification, OwnedChild, PipePump};
use super::*;
pub(super) struct RunOptions<'a> {
    pub(super) input: &'a [u8],
    pub(super) extra_env: &'a [(String, String)],
    pub(super) timeout: Duration,
    pub(super) sink: Option<(std::fs::File, usize)>,
    pub(super) disk_budget: Option<(&'a Path, u64)>,
    pub(super) remote_auth: Option<&'a super::super::credentials::RemoteAuth>,
}
impl RunOptions<'_> {
    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            input: &[],
            extra_env: &[],
            sink: None,
            disk_budget: None,
            remote_auth: None,
        }
    }
}
#[derive(Clone, Copy)]
pub(super) struct ExecutionBudget<'a> {
    pub(super) deadline: Instant,
    pub(super) timeout_code: &'static str,
    pub(super) disk_budget: Option<(&'a Path, u64)>,
}
impl<'a> ExecutionBudget<'a> {
    fn new(timeout: Duration, disk_budget: Option<(&'a Path, u64)>) -> Result<Self> {
        let operation_started = Instant::now();
        let command_deadline = operation_started.checked_add(timeout).ok_or_else(|| {
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
        Ok(Self {
            deadline,
            timeout_code,
            disk_budget,
        })
    }
    fn check_storage(&self, message: &str) -> Result<()> {
        if let Some((root, limit)) = self.disk_budget
            && directory_bytes(root, limit)? > limit
        {
            return Err(Error::new("storage_limit", message));
        }
        Ok(())
    }
}
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

impl GitRunner {
    pub(super) fn run_internal(
        &self,
        cwd: &Path,
        args: &[String],
        options: RunOptions<'_>,
    ) -> Result<GitOutput> {
        let budget = ExecutionBudget::new(
            options.timeout.min(self.limits.timeout),
            options.disk_budget,
        )?;
        self.validate_input(&options, args)?;
        let deadline = budget.deadline;
        let c = &self.counters;
        // A local nested scope makes coordination-lock admission consume the same deadline.
        let _scope = DeadlineScope::new(deadline.saturating_duration_since(Instant::now()));
        // Refuse pre-existing unsafe storage before any Git child can access it.
        // Only transient links created by the owned child are tolerated by the
        // live sampler; both admission and final validation remain strict.
        budget.check_storage("managed storage already exceeds quota")?;
        if Instant::now() >= deadline {
            return Err(Error::new(
                budget.timeout_code,
                "Git deadline expired during storage preflight",
            ));
        }
        c.acquire_child(self.limits.max_children, deadline)?;
        let _slot = Slot(c, Instant::now());
        if Instant::now() >= deadline {
            return Err(Error::new(
                "deadline_exceeded",
                "request deadline expired after Git admission",
            ));
        }
        let (owner, nonce) = self.spawn_child(cwd, args, (&options, deadline))?;
        self.collect_output(owner, nonce, (options, budget))
    }
    fn validate_input(&self, options: &RunOptions<'_>, args: &[String]) -> Result<()> {
        if options.input.len() > self.limits.max_output_bytes {
            return Err(Error::new("limit_exceeded", "Git stdin exceeds byte limit"));
        }
        for (key, value) in options.extra_env {
            validate_override(key, value)?;
        }
        if !self.git.is_absolute() {
            return Err(Error::new("invalid_git", "Git executable must be absolute"));
        }
        if args.iter().any(|argument| argument.contains('\0')) {
            return Err(Error::new("invalid_argument", "NUL in Git argument"));
        }
        Ok(())
    }
    fn spawn_child(
        &self,
        cwd: &Path,
        args: &[String],
        (options, deadline): (&RunOptions<'_>, Instant),
    ) -> Result<(OwnedChild, Option<[u8; 16]>)> {
        let prepared = self.command(args, options.disk_budget.map(|(_, limit)| limit))?;
        let helper_nonce = prepared.helper_nonce;
        let mut command = prepared.command;
        if let Some(auth) = options.remote_auth {
            auth.configure(&mut command);
        }
        command
            .envs(options.extra_env.iter().map(|(key, value)| (key, value)))
            .current_dir(cwd)
            .stdin(if options.input.is_empty() {
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
        self.counters.spawns.fetch_add(1, Ordering::Relaxed);
        Ok((OwnedChild::new(child), helper_nonce))
    }
    fn collect_output(
        &self,
        mut owner: OwnedChild,
        helper_nonce: Option<[u8; 16]>,
        (options, budget): (RunOptions<'_>, ExecutionBudget<'_>),
    ) -> Result<GitOutput> {
        let c = &self.counters;
        let mut pipes = PipePump::new(
            &mut owner,
            options.input,
            options.sink,
            self.limits.max_output_bytes,
        )?;
        let mut notification = ChildNotification::new(owner.pid, self.use_pidfd)?;
        match &notification {
            ChildNotification::Pidfd(_) => {
                c.pidfd_runs.fetch_add(1, Ordering::Relaxed);
            }
            ChildNotification::Waiter { .. } => {
                c.waiter_fallback_runs.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut answer = pipes.run(&mut owner, &notification, budget);
        let status = owner.finish();
        notification.join();
        if let Err(error) = pipes.sync_sink() {
            answer = Err(error);
        }
        c.bytes.fetch_add(pipes.counts.bytes, Ordering::Relaxed);
        self.record_result(answer)?;
        budget.check_storage("Git exceeded managed storage quota")?;
        let status = status?;
        if status.code() == Some(126)
            && let Some(error) =
                helper_nonce.and_then(|nonce| decode_helper_failure(&nonce, &pipes.stderr_bytes))
        {
            return Err(error);
        }
        Ok(GitOutput {
            status,
            stdout: pipes.stdout_bytes,
            stderr: pipes.stderr_bytes,
        })
    }
    fn record_result(&self, answer: Result<()>) -> Result<()> {
        let c = &self.counters;
        if let Err(error) = answer {
            if ["git_timeout", "deadline_exceeded"].contains(&error.code.as_str()) {
                c.timeouts.fetch_add(1, Ordering::Relaxed);
            }
            if error.code == "output_limit" {
                c.output_limits.fetch_add(1, Ordering::Relaxed);
            }
            return Err(error);
        }
        Ok(())
    }
}
fn validate_override(key: &str, value: &str) -> Result<()> {
    if ![
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ]
    .contains(&key)
        || value.contains('\0')
    {
        return Err(Error::new(
            "invalid_environment",
            "unsupported Git environment override",
        ));
    }
    let path = Path::new(value);
    if matches!(
        key,
        "GIT_INDEX_FILE" | "GIT_OBJECT_DIRECTORY" | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
    ) && !valid_override_path(key, path)
    {
        return Err(Error::new(
            "invalid_environment",
            "index file and object directories must be absolute internal paths",
        ));
    }
    Ok(())
}
fn valid_override_path(key: &str, path: &Path) -> bool {
    let single = key == "GIT_INDEX_FILE" || !path.as_os_str().as_encoded_bytes().contains(&b':');
    path.is_absolute()
        && single
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}
