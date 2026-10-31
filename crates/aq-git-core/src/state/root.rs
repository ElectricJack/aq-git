use crate::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
/// Deterministic durability fault injection for crash-ordering verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteFault {
    BeforeRename,
    AfterRename,
}
#[derive(Clone)]
pub struct StateRoot {
    pub(crate) inner: Arc<Inner>,
}
pub(crate) struct Inner {
    pub path: PathBuf,
    pub _lock: File,
    pub directory: File,
    pub mutex: Mutex<()>,
    pub fault: Mutex<Option<(usize, WriteFault)>>,
}
fn io(e: std::io::Error) -> Error {
    Error::new(
        if e.kind() == std::io::ErrorKind::NotFound {
            "state_not_found"
        } else {
            "io_error"
        },
        e.to_string(),
    )
}
impl StateRoot {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = Self::open_for_recovery(path)?;
        if root.active_incarnation()?.is_some() {
            return Err(Error::new(
                "recovery_blocked",
                "previous owner incarnation has no cleanup evidence",
            ));
        }
        Ok(root)
    }
    pub fn open_for_recovery(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        use std::os::fd::FromRawFd;
        let directory = secure_dirs(path)?;
        let name = std::ffi::CString::new("owner.lock").unwrap();
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        let lock = unsafe { File::from_raw_fd(fd) };
        if !lock.metadata().map_err(io)?.is_file() {
            return Err(Error::new(
                "invalid_path",
                "owner lock must be a regular file",
            ));
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::new(
                "state_busy",
                "state root is owned; use --socket",
            ));
        }
        let root = Self {
            inner: Arc::new(Inner {
                path: fs::canonicalize(path).map_err(io)?,
                directory,
                _lock: lock,
                mutex: Mutex::new(()),
                fault: Mutex::new(None),
            }),
        };
        match root.read_json::<serde_json::Value>("format.json")? {
            Some(v) if v["format_version"] != 1 => {
                return Err(Error::new("state_incompatible", "unsupported state format"));
            }
            None => {
                if fs::read_dir(root.path())
                    .map_err(io)?
                    .any(|entry| entry.map(|e| e.file_name() != "owner.lock").unwrap_or(true))
                {
                    return Err(Error::new(
                        "state_corrupt",
                        "state format missing from nonempty state root",
                    ));
                }
                root.write_json("format.json",&serde_json::json!({"format_version":1,"installation_id":uuid::Uuid::new_v4().to_string(),"created_at":chrono::Utc::now().to_rfc3339()}))?
            }
            _ => {}
        }
        Ok(root)
    }
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    fn parent_fd(&self, relative: &Path, create: bool) -> Result<(File, std::ffi::CString)> {
        let components: Vec<_> = relative.components().collect();
        if components.is_empty()
            || components
                .iter()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::new(
                "invalid_path",
                "relative metadata path required",
            ).details(serde_json::json!({"origin":"state_parent_fd","absolute":relative.is_absolute(),"component_count":components.len()})));
        }
        let mut directory = self.inner.directory.try_clone().map_err(io)?;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        for component in &components[..components.len() - 1] {
            let name = std::ffi::CString::new(component.as_os_str().as_bytes())
                .map_err(|_| Error::new("invalid_path", "NUL in path"))?;
            let mut fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0
                && create
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(io(std::io::Error::last_os_error()));
                }
                directory.sync_all().map_err(io)?;
                fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            if fd < 0 {
                return Err(io(std::io::Error::last_os_error()));
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        let name = std::ffi::CString::new(components.last().unwrap().as_os_str().as_bytes())
            .map_err(|_| Error::new("invalid_path", "NUL in path"))?;
        Ok((directory, name))
    }
    /// Create metadata directories without following symlinks. The returned path is for trusted managed Git operations.
    pub fn create_dir(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        let probe = relative.join(".aq-directory-probe");
        let (directory, _) = self.parent_fd(&probe, true)?;
        directory.sync_all().map_err(io)?;
        Ok(self.path().join(relative))
    }
    pub fn read_json<T: DeserializeOwned>(&self, relative: impl AsRef<Path>) -> Result<Option<T>> {
        use std::os::fd::FromRawFd;
        let (parent, name) = match self.parent_fd(relative.as_ref(), false) {
            Ok(v) => v,
            Err(e) if e.code == "state_not_found" => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(io(e));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata().map_err(io)?.is_file() {
            return Err(Error::new(
                "state_corrupt",
                "metadata record must be a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(Error::new(
                "limit_exceeded",
                "metadata record exceeds 64 MiB",
            ));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::new("state_corrupt", e.to_string()))
    }
    pub fn inject_write_failure_once(&self, point: WriteFault) {
        *self.inner.fault.lock().unwrap() = Some((0, point));
    }
    pub fn inject_write_failure_after(&self, successful_writes: usize, point: WriteFault) {
        *self.inner.fault.lock().unwrap() = Some((successful_writes, point));
    }
    fn write_fault(&self, point: WriteFault) -> Result<()> {
        let mut fault = self.inner.fault.lock().unwrap();
        if let Some((remaining, scheduled)) = fault.as_mut()
            && *scheduled == point
        {
            if *remaining > 0 {
                *remaining -= 1;
                return Ok(());
            }
            *fault = None;
            return Err(Error::new("io_error", "injected durability failure"));
        }
        Ok(())
    }
    pub fn write_json(&self, relative: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
        let data =
            serde_json::to_vec(value).map_err(|e| Error::new("state_encode", e.to_string()))?;
        self.write_bytes(relative, &data)
    }
    pub fn write_bytes(&self, relative: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        use std::os::fd::FromRawFd;
        let (parent, name) = self.parent_fd(relative.as_ref(), true)?;
        let temp = std::ffi::CString::new(format!(".{}.tmp", uuid::Uuid::new_v4())).unwrap();
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        let result = (|| {
            let mut f = unsafe { File::from_raw_fd(fd) };
            f.write_all(data).map_err(io)?;
            f.sync_all().map_err(io)?;
            self.write_fault(WriteFault::BeforeRename)?;
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temp.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            } < 0
            {
                return Err(io(std::io::Error::last_os_error()));
            }
            self.write_fault(WriteFault::AfterRename)?;
            parent.sync_all().map_err(io)
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temp.as_ptr(), 0);
            }
        }
        result
    }
    pub fn active_incarnation(&self) -> Result<Option<String>> {
        let Some(v) = self.read_json::<serde_json::Value>("incarnation.json")? else {
            return Ok(None);
        };
        if v["version"].as_u64() != Some(1) {
            return Err(Error::new(
                "state_incompatible",
                "unsupported incarnation format",
            ));
        }
        let active = v["active"]
            .as_bool()
            .ok_or_else(|| Error::new("state_corrupt", "incarnation missing lifecycle"))?;
        let id = v["id"]
            .as_str()
            .ok_or_else(|| Error::new("state_corrupt", "incarnation missing ID"))?;
        Ok(active.then(|| id.to_owned()))
    }
    pub fn mark_active(&self, id: &str) -> Result<()> {
        super::journal::safe_id(id)?;
        if self.active_incarnation()?.is_some() {
            return Err(Error::new(
                "recovery_blocked",
                "cannot replace an active incarnation",
            ));
        }
        self.write_json("incarnation.json",&serde_json::json!({"version":1,"id":id,"active":true,"guard_pid":std::process::id(),"boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default(),"started_at":chrono::Utc::now().to_rfc3339()}))
    }
    pub fn mark_clean(&self, id: &str) -> Result<()> {
        if self.active_incarnation()?.as_deref() != Some(id) {
            return Err(Error::new(
                "incarnation_conflict",
                "incarnation does not match",
            ));
        }
        self.write_json(
            "incarnation.json",
            &serde_json::json!({"version":1,"id":id,"active":false,"cleaned_at":chrono::Utc::now().to_rfc3339()}),
        )
    }
}
fn secure_dirs(path: &Path) -> Result<File> {
    use std::os::{fd::FromRawFd, unix::ffi::OsStrExt};
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io)?.join(path)
    };
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")
        .map_err(io)?;
    for component in absolute.components() {
        let Component::Normal(part) = component else {
            if matches!(component, Component::RootDir | Component::CurDir) {
                continue;
            }
            return Err(Error::new("invalid_path", "parent traversal forbidden"));
        };
        let name = std::ffi::CString::new(part.as_bytes())
            .map_err(|_| Error::new("invalid_path", "NUL in path"))?;
        let mut fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(io(std::io::Error::last_os_error()));
            }
            directory.sync_all().map_err(io)?;
            fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if fd < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}
