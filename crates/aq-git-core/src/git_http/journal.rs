//! Durable record of a push between "the task ref is about to move" and "the retention
//! pin and the receipt are durable".
//!
//! The `reference-transaction` hook writes the journal before Git moves the ref (see
//! `hook.rs`). Whoever finds a journal while holding the repository's mutation lock
//! calls [`RepositoryService::reconcile_push`]: it looks at the ref, and either
//! completes the push (pin + receipt, then drops the journal) or, when the ref never
//! moved, drops the journal. The same operation therefore always yields the same
//! answer, whether the endpoint died, the pin failed or the client retried.
use super::{
    JOURNAL_DIR, PushJournal, PushPromotion, PushReceipt, ZERO_OID, journal_file, promotion_file,
    receipt_path,
};
use crate::{Error, Result, service::RepositoryService, state::RetentionRef};
use std::{io::Write, path::Path};

/// Writes `data` to `path` so that a crash leaves the old file or the whole new one.
pub(crate) fn write_durable(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let written = file.write_all(data).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|_| std::fs::rename(&temporary, path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::File::open(parent)?.sync_all()
}

impl RepositoryService {
    fn journal_path(&self, operation: &str) -> std::path::PathBuf {
        self.root
            .path()
            .join(JOURNAL_DIR)
            .join(journal_file(operation))
    }
    fn read_journal(&self, operation: &str) -> Result<Option<PushJournal>> {
        match std::fs::read(self.journal_path(operation)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                Error::new("recovery_blocked", "push journal is unreadable")
            })?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) fn drop_journal(&self, operation: &str) -> Result<()> {
        match std::fs::remove_file(self.journal_path(operation)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::File::open(self.root.path().join(JOURNAL_DIR))?.sync_all()?;
        Ok(())
    }
    /// The exact value of `reference` in the repository's pool, if it exists.
    pub(crate) fn ref_value(&self, repository: &str, reference: &str) -> Result<Option<String>> {
        let out = self.repositories.runner.run(
            &self.repositories.pool_path(repository),
            &[
                "for-each-ref".into(),
                "--format=%(refname) %(objectname)".into(),
                reference.into(),
            ],
        )?;
        if !out.status.success() {
            return Err(Error::new("git_failed", "could not read the task ref"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
            l.strip_prefix(reference)?
                .strip_prefix(' ')
                .map(str::to_owned)
        }))
    }
    pub(crate) fn push_receipt_of(&self, operation: &str) -> Result<Option<PushReceipt>> {
        self.root.read_json(receipt_path(operation))
    }
    /// Pins the new head and writes the receipt of a push whose ref moved.
    fn complete_push(&self, journal: &PushJournal) -> Result<PushReceipt> {
        self.pin(
            &journal.repository,
            journal.binding_generation,
            &journal.retention_handle,
            "push",
            vec![RetentionRef {
                name: format!("refs/aq-git/pushes/{}", journal.retention_handle),
                oid: journal.new.clone(),
            }],
        )?;
        let receipt = PushReceipt {
            repository: journal.repository.clone(),
            reference: journal.reference.clone(),
            old: (journal.old != ZERO_OID).then(|| journal.old.clone()),
            new: journal.new.clone(),
            base: journal.base.clone(),
            retention_handle: journal.retention_handle.clone(),
            operation_id: journal.operation_id.clone(),
        };
        self.root
            .write_json(receipt_path(&journal.operation_id), &receipt)?;
        Ok(receipt)
    }
    /// Settles the journal of `operation`, if any. The caller holds the repository's
    /// mutation lock. `Some` is the (now durable) receipt of a push whose ref moved;
    /// `None` means no journal, or the ref never moved and the journal was dropped.
    pub(crate) fn reconcile_push(&self, operation: &str) -> Result<Option<PushReceipt>> {
        let Some(journal) = self.read_journal(operation)? else {
            return Ok(None);
        };
        let current = self.ref_value(&journal.repository, &journal.reference)?;
        if current.as_deref() != Some(journal.new.as_str()) {
            self.drop_journal(operation)?;
            return Ok(None);
        }
        let receipt = self.complete_push(&journal)?;
        self.drop_journal(operation)?;
        Ok(Some(receipt))
    }
    /// Gives the ref of a journaled push its old value back (used when the pin or the
    /// receipt cannot be made durable). On failure the journal stays for a later reconcile.
    pub(crate) fn revert_push(&self, operation: &str) -> Result<bool> {
        let Some(journal) = self.read_journal(operation)? else {
            return Ok(true);
        };
        let undo: Vec<String> = if journal.old == ZERO_OID {
            vec![
                "update-ref".into(),
                "-d".into(),
                journal.reference.clone(),
                journal.new.clone(),
            ]
        } else {
            vec![
                "update-ref".into(),
                journal.reference.clone(),
                journal.old.clone(),
                journal.new.clone(),
            ]
        };
        if self.repositories.git(&journal.repository, undo).is_err() {
            return Ok(false);
        }
        self.drop_journal(operation)?;
        Ok(true)
    }
    /// Takes back the pack files of a push that failed after Git promoted them, unless
    /// some ref holds the pushed commit (then they belong to an accepted push, or to an
    /// identical earlier one). The caller holds the repository's mutation lock.
    pub(crate) fn settle_promotion(&self, operation: &str) -> Result<()> {
        let path = self
            .root
            .path()
            .join(JOURNAL_DIR)
            .join(promotion_file(operation));
        let record: PushPromotion = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
                Error::new("recovery_blocked", "push promotion record is unreadable")
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let pool = self.repositories.pool_path(&record.repository);
        let referenced = self.repositories.runner.run(
            &pool,
            &[
                "for-each-ref".into(),
                "--count=1".into(),
                "--format=%(refname)".into(),
                "--contains".into(),
                record.new.clone(),
            ],
        )?;
        let gone = self.commit_exists(&record.repository, &record.new).is_err();
        if referenced.status.success() && referenced.stdout.is_empty() && !gone {
            for name in &record.files {
                // Plain file names only: the record is ours, but never trust a path.
                if !name.contains('/') && !name.starts_with('.') {
                    let _ = std::fs::remove_file(pool.join("objects/pack").join(name));
                }
            }
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }
    /// Settles every journal left by an earlier run. Returns how many could not be settled.
    pub(crate) fn reconcile_all_pushes(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(self.root.path().join(JOURNAL_DIR)) else {
            return 0;
        };
        let mut unsettled = 0;
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect();
        // Journals first: a push that did land keeps its objects.
        names.sort_by_key(|n| !n.ends_with(".json"));
        for name in names {
            let (operation, journaled) =
                match (name.strip_suffix(".json"), name.strip_suffix(".promoted")) {
                    (Some(op), _) => (op.to_owned(), true),
                    (_, Some(op)) => (op.to_owned(), false),
                    _ => continue,
                };
            let settled = (|| -> Result<()> {
                let repository = if journaled {
                    self.read_journal(&operation)?
                        .ok_or_else(|| Error::new("recovery_blocked", "journal vanished"))?
                        .repository
                } else {
                    match std::fs::read(self.root.path().join(JOURNAL_DIR).join(&name)) {
                        Ok(bytes) => serde_json::from_slice::<PushPromotion>(&bytes)?.repository,
                        Err(_) => return Ok(()),
                    }
                };
                let lock = self.mutation_lock(&repository);
                let _mutation = crate::git::lock_with_deadline(&lock)?;
                if journaled {
                    self.reconcile_push(&operation).map(|_| ())
                } else {
                    self.settle_promotion(&operation)
                }
            })();
            if settled.is_err() {
                unsettled += 1;
            }
        }
        unsettled
    }
}
