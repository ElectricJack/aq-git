//! Pieces the server builds requests from: the exact-ref view Git serves, the staged
//! request body, the push request parser and the shared accounting.
use super::{Grant, http::HttpError};
use crate::{Error, Result};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// A counter of bytes held by in-flight requests, with an optional ceiling.
#[derive(Default)]
pub(crate) struct Counter {
    used: AtomicU64,
}
/// Bytes charged to a [`Counter`]; returned when dropped.
pub(crate) struct Held<'a> {
    counter: &'a Counter,
    bytes: u64,
}
impl Counter {
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }
    /// Charges `bytes`; fails with `overloaded` when the total would pass `limit`.
    pub fn charge(&self, bytes: u64, limit: u64) -> Result<Held<'_>> {
        let mut held = Held {
            counter: self,
            bytes: 0,
        };
        held.grow(bytes, limit)?;
        Ok(held)
    }
}
impl Held<'_> {
    pub fn grow(&mut self, bytes: u64, limit: u64) -> Result<()> {
        let previous = self.counter.used.fetch_add(bytes, Ordering::SeqCst);
        self.bytes += bytes;
        if previous.saturating_add(bytes) > limit {
            return Err(Error::new(
                "overloaded",
                "request buffers would exceed the shared memory budget",
            ));
        }
        Ok(())
    }
}
impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.counter.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// A scratch repository holding exactly the refs a grant may see, borrowing every object
/// from the pool. Git advertises and serves only what is in it, so nothing else in the pool
/// (other tasks' refs, ref subtrees, tags) is reachable by name or by object ID.
pub(crate) struct View {
    pub path: PathBuf,
}
impl View {
    pub fn create(parent: &Path, pool: &Path, refs: &[(String, String)]) -> Result<Self> {
        let path = parent.join(uuid::Uuid::new_v4().to_string());
        let view = Self { path };
        let build = || -> std::io::Result<()> {
            std::fs::create_dir_all(view.path.join("objects/info"))?;
            std::fs::create_dir_all(view.path.join("refs/heads"))?;
            std::fs::write(
                view.path.join("objects/info/alternates"),
                format!("{}\n", pool.join("objects").display()),
            )?;
            std::fs::write(view.path.join("HEAD"), "ref: refs/heads/aq-unborn\n")?;
            for (name, oid) in refs {
                let target = view.path.join(name);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, format!("{oid}\n"))?;
            }
            Ok(())
        };
        build().map_err(|e| Error::new("invalid_request", format!("cannot present refs: {e}")))?;
        Ok(view)
    }
}
impl Drop for View {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A request body spooled to disk. The file goes away with the value.
pub(crate) struct Staged {
    pub path: PathBuf,
    pub len: u64,
}
impl Staged {
    pub fn open(&self) -> std::io::Result<std::fs::File> {
        std::fs::File::open(&self.path)
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
/// Copies `body` to a new file in `dir`.
pub(crate) fn spool(dir: &Path, body: &mut dyn Read) -> std::io::Result<Staged> {
    let path = dir.join(uuid::Uuid::new_v4().to_string());
    let mut staged = Staged { path, len: 0 };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged.path)?;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = body.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n])?;
        staged.len += n as u64;
    }
    file.sync_data()?;
    Ok(staged)
}

/// Receive capabilities a task push may use. Everything else (push certificates,
/// push options, atomic pushes, ...) is refused before Git sees the request.
const ALLOWED_CAPABILITIES: &[&str] = &[
    "report-status",
    "report-status-v2",
    "side-band-64k",
    "quiet",
    "ofs-delta",
    "object-format=sha1",
];

/// What the head of a `git-receive-pack` request says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PushRequest {
    pub commands: Vec<[String; 3]>,
    pub capabilities: Vec<String>,
    /// Object count of the pack header, if a pack follows the commands.
    pub pack_objects: Option<u32>,
}
impl PushRequest {
    pub fn wants(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

fn bad(message: impl Into<String>) -> HttpError {
    HttpError::new(400, format!("invalid_request: {}", message.into()))
}

/// Parses the pkt-line command list and the pack header at the start of `input`.
/// `max_bytes` bounds the command list.
pub(crate) fn parse_push(
    input: &mut dyn Read,
    max_bytes: usize,
) -> std::result::Result<PushRequest, HttpError> {
    let mut commands = Vec::new();
    let mut capabilities = Vec::new();
    let mut used = 0usize;
    loop {
        let mut length = [0u8; 4];
        input
            .read_exact(&mut length)
            .map_err(|_| bad("truncated command list"))?;
        let length = std::str::from_utf8(&length)
            .ok()
            .and_then(|s| usize::from_str_radix(s, 16).ok())
            .ok_or_else(|| bad("malformed pkt-line"))?;
        if length == 0 {
            break;
        }
        if !(4..=65520).contains(&length) {
            return Err(bad("malformed pkt-line"));
        }
        used += length;
        if used > max_bytes {
            return Err(HttpError::new(
                413,
                "limit_exceeded: command list is too large",
            ));
        }
        let mut line = vec![0u8; length - 4];
        input
            .read_exact(&mut line)
            .map_err(|_| bad("truncated command list"))?;
        let (text, caps) = match line.iter().position(|b| *b == 0) {
            Some(nul) => (&line[..nul], Some(&line[nul + 1..])),
            None => (&line[..], None),
        };
        let text = std::str::from_utf8(text).map_err(|_| bad("command is not UTF-8"))?;
        let text = text.strip_suffix('\n').unwrap_or(text);
        if text.starts_with("push-cert") {
            return Err(bad("push certificates are not accepted"));
        }
        if text.starts_with("shallow ") || text.starts_with("push-option") {
            return Err(bad("shallow updates and push options are not accepted"));
        }
        if let Some(caps) = caps {
            let caps = std::str::from_utf8(caps).map_err(|_| bad("capabilities are not UTF-8"))?;
            if !capabilities.is_empty() {
                return Err(bad("capabilities given twice"));
            }
            for capability in caps.split(' ').filter(|c| !c.is_empty()) {
                if capability.starts_with("push-cert") {
                    return Err(bad("push certificates are not accepted"));
                }
                if !capability.starts_with("agent=") && !ALLOWED_CAPABILITIES.contains(&capability)
                {
                    return Err(bad(format!("unsupported receive capability {capability}")));
                }
                capabilities.push(capability.to_owned());
            }
        }
        let fields: Vec<&str> = text.split(' ').collect();
        let [old, new, reference] = fields[..] else {
            return Err(bad("malformed command"));
        };
        commands.push([old.to_owned(), new.to_owned(), reference.to_owned()]);
    }
    let mut header = [0u8; 12];
    let mut got = 0;
    while got < header.len() {
        match input.read(&mut header[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return Err(bad("truncated request")),
        }
    }
    let pack_objects = match got {
        0 => None,
        12 if &header[..4] == b"PACK" => {
            Some(u32::from_be_bytes(header[8..12].try_into().unwrap()))
        }
        _ => return Err(bad("request carries data that is not a pack")),
    };
    Ok(PushRequest {
        commands,
        capabilities,
        pack_objects,
    })
}

/// The answer Git gives for an accepted one-ref push, for replaying a finished operation.
pub(crate) fn synthetic_report(request: &PushRequest, reference: &str) -> Vec<u8> {
    fn pkt(data: &[u8]) -> Vec<u8> {
        let mut out = format!("{:04x}", data.len() + 4).into_bytes();
        out.extend_from_slice(data);
        out
    }
    if !request.wants("report-status") && !request.wants("report-status-v2") {
        return Vec::new();
    }
    let mut report = pkt(b"unpack ok\n");
    report.extend(pkt(format!("ok {reference}\n").as_bytes()));
    report.extend_from_slice(b"0000");
    if !request.wants("side-band-64k") {
        return report;
    }
    let mut band = vec![1u8];
    band.extend(report);
    let mut out = pkt(&band);
    out.extend_from_slice(b"0000");
    out
}

/// Git's push-quarantine directories under a pool's `objects` directory.
pub(crate) fn quarantines(pool: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(pool.join("objects")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("incoming-"))
        .map(|e| e.path())
        .collect()
}
/// Removes the quarantines of `pool` that are not in `keep` (a killed `receive-pack`
/// leaves its own behind).
pub(crate) fn remove_quarantines(pool: &Path, keep: &[PathBuf]) {
    for path in quarantines(pool) {
        if !keep.contains(&path) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// The refs a grant may see: the served main head and the task ref, when they exist.
pub(crate) fn grant_refs(
    grant: &Grant,
    main: Option<String>,
    task: Option<String>,
) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    if let Some(oid) = main {
        refs.push((grant.main_ref.clone(), oid));
    }
    if let Some(oid) = task {
        refs.push((grant.task_ref.clone(), oid));
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pkt(data: &str) -> Vec<u8> {
        let mut out = format!("{:04x}", data.len() + 4).into_bytes();
        out.extend_from_slice(data.as_bytes());
        out
    }
    fn request(first: &str, extra: &[&str], tail: &[u8]) -> Vec<u8> {
        let mut out = pkt(first);
        for line in extra {
            out.extend(pkt(line));
        }
        out.extend_from_slice(b"0000");
        out.extend_from_slice(tail);
        out
    }
    const ZERO: &str = "0000000000000000000000000000000000000000";
    #[test]
    fn push_requests_parse_and_refuse_certificates_and_extra_capabilities() {
        let first = format!(
            "{ZERO} {} refs/heads/aq/t\0report-status side-band-64k agent=git/2",
            "a".repeat(40)
        );
        let mut tail = b"PACK\0\0\0\x02\0\0\0\x07".to_vec();
        tail.extend_from_slice(b"rest");
        let parsed = parse_push(&mut &request(&first, &[], &tail)[..], 4096).unwrap();
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(parsed.pack_objects, Some(7));
        assert!(parsed.wants("side-band-64k"));
        assert_eq!(
            parse_push(&mut &request(&first, &[], b"")[..], 4096)
                .unwrap()
                .pack_objects,
            None
        );

        let cert = request(
            "push-cert\0report-status",
            &["certificate version 0.1\n"],
            b"",
        );
        assert_eq!(parse_push(&mut &cert[..], 4096).unwrap_err().status, 400);
        let cert_cap = format!(
            "{ZERO} {} refs/heads/aq/t\0report-status push-cert=abc",
            "a".repeat(40)
        );
        assert_eq!(
            parse_push(&mut &request(&cert_cap, &[], b"")[..], 4096)
                .unwrap_err()
                .status,
            400
        );
        for capability in [
            "push-options",
            "atomic",
            "delete-refs",
            "object-format=sha256",
        ] {
            let line = format!(
                "{ZERO} {} refs/heads/aq/t\0report-status {capability}",
                "a".repeat(40)
            );
            let error = parse_push(&mut &request(&line, &[], b"")[..], 4096).unwrap_err();
            assert_eq!(error.status, 400, "{capability}");
        }
        let junk = request(&first, &[], b"not a pack at all");
        assert_eq!(parse_push(&mut &junk[..], 4096).unwrap_err().status, 400);
        let line = format!("{ZERO} {} refs/heads/x\n", "b".repeat(40));
        let many = request(&first, &[line.as_str(); 200], b"");
        assert_eq!(parse_push(&mut &many[..], 1024).unwrap_err().status, 413);
        assert_eq!(parse_push(&mut &b"00"[..], 1024).unwrap_err().status, 400);
    }
    #[test]
    fn replayed_answers_follow_the_requested_report_format() {
        let mut request = PushRequest {
            commands: vec![],
            capabilities: vec!["report-status".into()],
            pack_objects: None,
        };
        assert_eq!(
            synthetic_report(&request, "refs/heads/x"),
            b"000eunpack ok\n0014ok refs/heads/x\n0000"
        );
        request.capabilities.push("side-band-64k".into());
        let wrapped = synthetic_report(&request, "refs/heads/x");
        assert!(wrapped.starts_with(b"002b\x01000eunpack ok\n"));
        assert!(wrapped.ends_with(b"00000000"));
        request.capabilities.clear();
        assert!(synthetic_report(&request, "refs/heads/x").is_empty());
    }
    #[test]
    fn counters_enforce_the_ceiling_and_give_bytes_back() {
        let counter = Counter::default();
        let mut first = counter.charge(60, 100).unwrap();
        assert!(counter.charge(50, 100).is_err());
        assert_eq!(counter.used(), 60, "a failed charge leaves nothing held");
        first.grow(30, 100).unwrap();
        assert_eq!(counter.used(), 90);
        assert!(first.grow(20, 100).is_err());
        drop(first);
        assert_eq!(counter.used(), 0);
    }
    #[test]
    fn leftover_quarantines_are_swept_but_kept_ones_stay() {
        let pool = tempfile::tempdir().unwrap();
        let objects = pool.path().join("objects");
        for name in ["pack", "info", "incoming-AAA", "tmp_objdir-incoming-BBB"] {
            std::fs::create_dir_all(objects.join(name)).unwrap();
        }
        std::fs::write(objects.join("incoming-AAA/tmp_pack_x"), b"half a pack").unwrap();
        let keep = vec![objects.join("incoming-AAA")];
        remove_quarantines(pool.path(), &keep);
        assert!(objects.join("incoming-AAA").exists());
        assert!(!objects.join("tmp_objdir-incoming-BBB").exists());
        assert!(objects.join("pack").exists() && objects.join("info").exists());
        remove_quarantines(pool.path(), &[]);
        assert!(quarantines(pool.path()).is_empty());
    }
}
