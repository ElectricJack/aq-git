//! Pool temporary pack hygiene.
//!
//! `git index-pack` writes `objects/pack/tmp_pack_*`, `tmp_idx_*` and `tmp_rev_*`
//! before renaming them to a real pack. A child killed at its deadline leaves
//! them behind, and Git never removes them (`gc.auto=0`). Two measures apply:
//!
//! * an import writes into a private staging object directory inside the pool
//!   ([`PoolStaging`]); its packs move into `objects/pack` only after the child
//!   succeeded, and the staging directory is removed on every other outcome;
//! * [`RepositoryService::sweep_stale_temp_packs`] removes stale temporary pack
//!   files that no running process holds open, and staging directories left by
//!   a crashed owner. Real packs, loose objects and refs are never touched.
use crate::{Error, Result, operations::sync_dir, service::RepositoryService};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

/// Temporary pack file prefixes written by `git index-pack`/`pack-objects`.
pub const TEMP_PACK_PREFIXES: &[&str] = &["tmp_pack_", "tmp_idx_", "tmp_rev_"];
/// Prefix of a per-operation staging object directory below `objects/`.
pub const STAGING_PREFIX: &str = "incoming-aq-";
/// Administrator configuration file in the state root.
pub const CONFIG_FILE: &str = "temp_pack_sweep.json";

/// Staging directories owned by a live import in this process.
static ACTIVE_STAGING: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

fn active_staging() -> std::sync::MutexGuard<'static, BTreeSet<PathBuf>> {
    ACTIVE_STAGING
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TempPackSweepConfig {
    /// Seconds between periodic sweeps of a serving process.
    pub interval_secs: u64,
    /// Minimum age (by modification time) of a temporary file before removal.
    pub max_age_secs: u64,
}
impl Default for TempPackSweepConfig {
    fn default() -> Self {
        Self {
            interval_secs: 900,
            max_age_secs: 3600,
        }
    }
}
impl TempPackSweepConfig {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
    pub fn max_age(&self) -> Duration {
        Duration::from_secs(self.max_age_secs)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TempPackSweepReport {
    pub pools_scanned: u64,
    pub files_removed: u64,
    pub bytes_removed: u64,
    /// Stale temporary files kept because a running process holds them open.
    pub files_kept_open: u64,
    pub staging_directories_removed: u64,
    /// Entries that could not be inspected or removed; the sweep continues.
    pub errors: u64,
}

/// A private object directory for one Git child writing into a pool.
///
/// The child sees the pool through `GIT_ALTERNATE_OBJECT_DIRECTORIES`, so it can
/// complete thin packs, but every file it creates lands below this directory.
/// [`PoolStaging::migrate`] links finished packs into the pool; dropping the
/// staging removes the directory, including any temporary files of a child that
/// was killed at its deadline.
pub struct PoolStaging {
    dir: PathBuf,
    objects: PathBuf,
}
impl PoolStaging {
    pub fn create(pool: &Path, operation: &str) -> Result<Self> {
        crate::operations::id(operation)?;
        let objects = pool.join("objects");
        crate::operations::checked_directory(&objects)?;
        let dir = objects.join(format!("{STAGING_PREFIX}{operation}"));
        if !active_staging().insert(dir.clone()) {
            return Err(Error::new(
                "recovery_blocked",
                "pool staging directory is owned by a running import",
            ));
        }
        let staging = Self { dir, objects };
        // A replay of the same operation discards what an earlier attempt left.
        staging.remove();
        fs::create_dir(&staging.dir)?;
        fs::create_dir(staging.dir.join("pack"))?;
        sync_dir(&staging.objects)?;
        Ok(staging)
    }
    pub fn path(&self) -> &Path {
        &self.dir
    }
    /// Environment that redirects a child's object writes into this staging.
    pub fn environment(&self) -> Vec<(String, String)> {
        vec![
            (
                "GIT_OBJECT_DIRECTORY".into(),
                self.dir.to_string_lossy().into_owned(),
            ),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES".into(),
                self.objects.to_string_lossy().into_owned(),
            ),
        ]
    }
    /// Link the finished packs into the pool, the index of each pack last.
    pub fn migrate(&self) -> Result<()> {
        let files = self.staged_packs()?;
        let target = self.objects.join("pack");
        if !target.exists() {
            fs::create_dir(&target)?;
            sync_dir(&self.objects)?;
        }
        crate::operations::checked_directory(&target)?;
        for (_, name) in files {
            // Same name means the same object set: keep the existing pack, as Git does.
            match fs::hard_link(self.dir.join("pack").join(&name), target.join(&name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        sync_dir(&target)?;
        Ok(())
    }
    fn staged_packs(&self) -> Result<Vec<(u8, String)>> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name() != "pack" || !entry.file_type()?.is_dir() {
                return Err(Error::new(
                    "git_failed",
                    "pool import wrote an unexpected staging entry",
                ));
            }
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(self.dir.join("pack"))? {
            files.push(staged_pack(entry?)?);
        }
        files.sort();
        Ok(files)
    }
    fn remove(&self) {
        if fs::symlink_metadata(&self.dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            let _ = fs::remove_dir_all(&self.dir);
            let _ = sync_dir(&self.objects);
        }
    }
}
fn staged_pack(entry: fs::DirEntry) -> Result<(u8, String)> {
    let name = entry.file_name();
    let name = name.to_str().unwrap_or_default();
    let order = name
        .strip_prefix("pack-")
        .and_then(|rest| rest.rsplit_once('.'))
        .and_then(|(_, extension)| match extension {
            "keep" => Some(0),
            "pack" => Some(1),
            "rev" | "mtimes" => Some(2),
            "bitmap" => Some(3),
            "idx" => Some(4),
            _ => None,
        });
    match order {
        Some(order) if entry.file_type()?.is_file() => Ok((order, name.to_owned())),
        _ => Err(Error::new(
            "git_failed",
            "pool import left an unexpected staging pack entry",
        )),
    }
}

impl Drop for PoolStaging {
    fn drop(&mut self) {
        self.remove();
        active_staging().remove(&self.dir);
    }
}

/// `(device, inode)` of every file some readable process holds open.
fn open_files() -> BTreeSet<(u64, u64)> {
    let mut open = BTreeSet::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return open;
    };
    for process in processes.flatten() {
        if !process
            .file_name()
            .to_str()
            .is_some_and(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        record_descriptors(process, &mut open);
    }
    open
}

fn record_descriptors(process: fs::DirEntry, open: &mut BTreeSet<(u64, u64)>) {
    let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
        return;
    };
    for descriptor in descriptors.flatten() {
        if let Ok(metadata) = fs::metadata(descriptor.path()) {
            open.insert((metadata.dev(), metadata.ino()));
        }
    }
}

fn older_than(metadata: &fs::Metadata, cutoff: SystemTime) -> bool {
    metadata.modified().is_ok_and(|modified| modified <= cutoff)
}

/// Sweep every pool below `state_root/repositories`.
pub fn sweep_pools(state_root: &Path, max_age: Duration) -> Result<TempPackSweepReport> {
    let mut report = TempPackSweepReport::default();
    let repositories = match fs::read_dir(state_root.join("repositories")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error.into()),
    };
    let cutoff = SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut open: Option<BTreeSet<(u64, u64)>> = None;
    for repository in repositories {
        let Ok(repository) = repository else {
            report.errors += 1;
            continue;
        };
        if !repository.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let objects = repository.path().join("objects.git").join("objects");
        if !fs::symlink_metadata(&objects).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        report.pools_scanned += 1;
        sweep_pool(&objects, cutoff, &mut open, &mut report);
    }
    Ok(report)
}

fn sweep_pool(
    objects: &Path,
    cutoff: SystemTime,
    open: &mut Option<BTreeSet<(u64, u64)>>,
    report: &mut TempPackSweepReport,
) {
    let pack = objects.join("pack");
    let before = report.files_removed;
    if fs::symlink_metadata(&pack).is_ok_and(|m| m.is_dir())
        && let Ok(entries) = fs::read_dir(&pack)
    {
        for entry in entries {
            let Ok(entry) = entry else {
                report.errors += 1;
                continue;
            };
            sweep_pack_entry(entry, cutoff, open, report);
        }
    }
    if report.files_removed != before && sync_dir(&pack).is_err() {
        report.errors += 1;
    }
    sweep_staging(objects, cutoff, report);
}

fn pack_metadata(path: &Path, report: &mut TempPackSweepReport) -> Option<fs::Metadata> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            report.errors += 1;
            None
        }
    }
}

fn is_temp_pack(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        TEMP_PACK_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    })
}

fn sweep_pack_entry(
    entry: fs::DirEntry,
    cutoff: SystemTime,
    open: &mut Option<BTreeSet<(u64, u64)>>,
    report: &mut TempPackSweepReport,
) {
    if !is_temp_pack(&entry.file_name()) {
        return;
    }
    let path = entry.path();
    let Some(metadata) = pack_metadata(&path, report) else {
        return;
    };
    if !metadata.is_file() || !older_than(&metadata, cutoff) {
        return;
    }
    if open
        .get_or_insert_with(open_files)
        .contains(&(metadata.dev(), metadata.ino()))
    {
        report.files_kept_open += 1;
        return;
    }
    match fs::remove_file(&path) {
        Ok(()) => {
            report.files_removed += 1;
            report.bytes_removed += metadata.len();
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => report.errors += 1,
    }
}

fn orphaned_staging_path(entry: fs::DirEntry, cutoff: SystemTime) -> Option<PathBuf> {
    let path = entry.path();
    if !entry
        .file_name()
        .to_str()
        .is_some_and(|name| name.starts_with(STAGING_PREFIX))
        || active_staging().contains(&path)
    {
        return None;
    }
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return None;
    };
    if !metadata.is_dir() || !older_than(&metadata, cutoff) {
        return None;
    }
    Some(path)
}

fn sweep_staging(objects: &Path, cutoff: SystemTime, report: &mut TempPackSweepReport) {
    // Staging directories outlive their import only if the owning process died.
    let Ok(entries) = fs::read_dir(objects) else {
        report.errors += 1;
        return;
    };
    let mut changed = false;
    for entry in entries.flatten() {
        let Some(path) = orphaned_staging_path(entry, cutoff) else {
            continue;
        };
        let bytes = crate::git::directory_bytes(&path, u64::MAX).unwrap_or(0);
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                report.staging_directories_removed += 1;
                report.bytes_removed += bytes;
                changed = true;
            }
            Err(_) => report.errors += 1,
        }
    }
    if changed && sync_dir(objects).is_err() {
        report.errors += 1;
    }
}

impl RepositoryService {
    /// Administrator sweep configuration (`temp_pack_sweep.json`, optional).
    pub fn temp_pack_sweep_config(&self) -> Result<TempPackSweepConfig> {
        let config = self
            .root
            .read_json::<TempPackSweepConfig>(CONFIG_FILE)?
            .unwrap_or_default();
        if config.interval_secs == 0 || config.max_age_secs == 0 {
            return Err(Error::new(
                "invalid_configuration",
                "temporary pack sweep interval and age must be positive",
            ));
        }
        Ok(config)
    }
    /// Remove stale temporary pack files and orphaned staging directories in
    /// every pool, using the configured minimum age.
    pub fn sweep_stale_temp_packs(&self) -> Result<TempPackSweepReport> {
        let config = self.temp_pack_sweep_config()?;
        sweep_pools(self.root.path(), config.max_age())
    }
}
