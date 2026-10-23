//! File plans and commits share one version check and one truthful receipt model.
//! Windows holds non-sharing handles through commit and rollback. Existing files
//! are updated through those handles; this is NOT crash-atomic replacement.
use crate::{
    error::{Code, Failure, Result},
    files::hash,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_FILE: usize = 16 * 1024 * 1024;
pub struct Change {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}
pub fn validate(changes: &[Change]) -> Result<()> {
    if changes.is_empty()
        || changes.len() > 100
        || changes
            .iter()
            .map(|c| c.path.as_os_str().len())
            .sum::<usize>()
            > 32000
        || changes
            .iter()
            .map(|c| c.before.as_ref().map_or(0, Vec::len) + c.after.as_ref().map_or(0, Vec::len))
            .sum::<usize>()
            > 64 * 1024 * 1024
    {
        return Err(Failure::invalid(
            "Change set exceeds 100 files, 32 KiB of paths, or 64 MiB of contents",
        ));
    }
    for c in changes {
        if !c.path.is_absolute() {
            return Err(Failure::invalid(
                "Change targets must be absolute local paths",
            ));
        }
        if fs::symlink_metadata(&c.path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Failure::new(
                Code::PermissionDenied,
                "Mutation target is a symbolic link",
                "Use its explicit workspace target path; link deletion/replacement requires an explicit filesystem command.",
            ));
        }
        if c.before.as_ref().is_some_and(|b| b.len() > MAX_FILE)
            || c.after.as_ref().is_some_and(|b| b.len() > MAX_FILE)
        {
            return Err(Failure::invalid("File exceeds 16 MiB"));
        }
        text(c.before.as_deref().unwrap_or_default())?;
        text(c.after.as_deref().unwrap_or_default())?;
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Planned,
    Applied,
    NotAttempted,
    Failed,
    RolledBack,
    RollbackFailed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReceipt {
    pub path: PathBuf,
    pub before_sha256: Option<String>,
    pub proposed_sha256: Option<String>,
    pub outcome: Outcome,
    pub error: Option<Failure>,
    pub diff: String,
    pub diff_truncated: bool,
    pub added_lines: usize,
    pub removed_lines: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub preview: bool,
    pub success: bool,
    pub crash_atomic: bool,
    pub files: Vec<FileReceipt>,
    pub error: Option<Failure>,
}

pub fn text(bytes: &[u8]) -> Result<&str> {
    let s = std::str::from_utf8(bytes).map_err(|_|Failure::new(Code::UnsupportedEncoding,"Expected UTF-8 text (UTF-8 BOM is preserved); UTF-16 and binary data are not rewritten", "Convert the file deliberately with an appropriate encoding tool."))?;
    if s.contains('\0') {
        return Err(Failure::new(
            Code::UnsupportedEncoding,
            "Text contains NUL bytes",
            "Use an explicit binary/encoding tool.",
        ));
    }
    Ok(s)
}
pub fn bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match File::open(path) {
        Ok(mut f) => {
            let mut b = Vec::new();
            std::io::Read::by_ref(&mut f)
                .take(MAX_FILE as u64 + 1)
                .read_to_end(&mut b)?;
            if b.len() > MAX_FILE {
                return Err(Failure::invalid("File exceeds 16 MiB"));
            }
            Ok(Some(b))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn verify(expected: &str, bytes: &[u8]) -> Result<()> {
    if expected != hash(bytes) {
        return Err(Failure::conflict("File version changed since it was read"));
    }
    Ok(())
}

impl Change {
    fn receipt(&self) -> Result<FileReceipt> {
        let before = text(self.before.as_deref().unwrap_or_default())?;
        let after = text(self.after.as_deref().unwrap_or_default())?;
        let diff = similar::TextDiff::configure()
            .timeout(Duration::from_millis(100))
            .diff_lines(before, after);
        let added_lines = diff
            .iter_all_changes()
            .filter(|c| c.tag() == similar::ChangeTag::Insert)
            .count();
        let removed_lines = diff
            .iter_all_changes()
            .filter(|c| c.tag() == similar::ChangeTag::Delete)
            .count();
        // Budget the formatter itself, rather than allocating an arbitrarily large diff.
        struct Bounded {
            value: String,
            truncated: bool,
        }
        impl std::fmt::Write for Bounded {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                let mut end = s.len().min(12000usize.saturating_sub(self.value.len()));
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                self.value.push_str(&s[..end]);
                if end < s.len() {
                    self.truncated = true;
                    return Err(std::fmt::Error);
                }
                Ok(())
            }
        }
        let mut shown = Bounded {
            value: String::new(),
            truncated: false,
        };
        let _ = std::fmt::write(
            &mut shown,
            format_args!("{}", diff.unified_diff().context_radius(2)),
        );
        Ok(FileReceipt {
            path: self.path.clone(),
            before_sha256: self.before.as_deref().map(hash),
            proposed_sha256: self.after.as_deref().map(hash),
            outcome: Outcome::Planned,
            error: None,
            diff: shown.value,
            diff_truncated: shown.truncated,
            added_lines,
            removed_lines,
        })
    }
}

struct Locked {
    file: File,
    path: PathBuf,
    created: bool,
}
impl Locked {
    fn open(change: &Change) -> Result<Self> {
        if change.before.is_none() {
            fs::create_dir_all(
                change
                    .path
                    .parent()
                    .ok_or_else(|| Failure::invalid("Missing parent"))?,
            )?;
        }
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        if change.before.is_none() {
            opts.create_new(true);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // DELETE access lets deletion be marked through this same exclusive handle.
            opts.share_mode(0)
                .access_mode(0x80000000 | 0x40000000 | 0x00010000);
        }
        let mut file = opts.open(&change.path).map_err(|e| {
            if matches!(e.raw_os_error(), Some(32 | 33))
                || e.kind() == std::io::ErrorKind::AlreadyExists
            {
                Failure::conflict(format!(
                    "File is locked or appeared during validation: {}",
                    change.path.display()
                ))
            } else {
                e.into()
            }
        })?;
        #[cfg(not(windows))]
        fs2::FileExt::try_lock_exclusive(&file)?;
        let mut current = Vec::new();
        (&mut file)
            .take(MAX_FILE as u64 + 1)
            .read_to_end(&mut current)?;
        if change.before.as_ref().is_some_and(|b| *b != current) {
            return Err(Failure::conflict(format!(
                "File changed during preparation: {}",
                change.path.display()
            )));
        }
        Ok(Self {
            file,
            path: change.path.clone(),
            created: change.before.is_none(),
        })
    }
    fn store(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(bytes)?;
        self.file.set_len(bytes.len() as u64)?;
        self.file.sync_all()?;
        Ok(())
    }
    fn delete(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
            };
            let info = FILE_DISPOSITION_INFO { DeleteFile: true };
            if unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle() as _,
                    FileDispositionInfo,
                    &info as *const _ as _,
                    std::mem::size_of_val(&info) as _,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))]
        fs::remove_file(&self.path)?;
        Ok(())
    }
    fn undelete(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
            };
            let info = FILE_DISPOSITION_INFO { DeleteFile: false };
            if unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle() as _,
                    FileDispositionInfo,
                    &info as *const _ as _,
                    std::mem::size_of_val(&info) as _,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))]
        {
            if !self.path.exists() {
                self.file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&self.path)?;
            }
        }
        Ok(())
    }
}
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = &self.path;
    }
}

pub fn execute(changes: &[Change], preview: bool) -> Result<Receipt> {
    execute_with(changes, preview, |_| Ok(()))
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    Apply(usize),
    AfterApply(usize),
    Rollback(usize),
}
pub(crate) fn execute_with(
    changes: &[Change],
    preview: bool,
    mut before_write: impl FnMut(Point) -> Result<()>,
) -> Result<Receipt> {
    validate(changes)?;
    let mut receipt = Receipt {
        preview,
        success: true,
        crash_atomic: false,
        files: changes.iter().map(Change::receipt).collect::<Result<_>>()?,
        error: None,
    };
    let mut diff_budget = 32000usize;
    for file in &mut receipt.files {
        if file.diff.len() > diff_budget {
            let mut end = diff_budget;
            while !file.diff.is_char_boundary(end) {
                end -= 1;
            }
            file.diff.truncate(end);
            file.diff_truncated = true;
        }
        diff_budget = diff_budget.saturating_sub(file.diff.len());
    }
    if preview {
        return Ok(receipt);
    }
    for r in &mut receipt.files {
        r.outcome = Outcome::NotAttempted;
    }
    let mut locked: Vec<Locked> = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        match Locked::open(c) {
            Ok(f) => locked.push(f),
            Err(e) => {
                receipt.success = false;
                receipt.error = Some(e.clone());
                receipt.files[i].error = Some(e);
                receipt.files[i].outcome = Outcome::Failed;
                for (j, f) in locked.iter_mut().enumerate().filter(|(_, f)| f.created) {
                    if let Err(e) = f.delete() {
                        receipt.files[j].outcome = Outcome::RollbackFailed;
                        receipt.files[j].error = Some(e);
                    }
                }
                return Ok(receipt);
            }
        }
    }
    for (i, c) in changes.iter().enumerate() {
        let result = before_write(Point::Apply(i))
            .and_then(|()| match &c.after {
                Some(b) => locked[i].store(b),
                None => locked[i].delete(),
            })
            .and_then(|()| before_write(Point::AfterApply(i)));
        if let Err(e) = result {
            receipt.success = false;
            receipt.error = Some(e.clone());
            receipt.files[i].error = Some(e);
            // The failing write may already have changed bytes: roll it back too.
            for j in (0..=i).rev() {
                let result =
                    before_write(Point::Rollback(j)).and_then(|()| match &changes[j].before {
                        Some(b) => locked[j].undelete().and_then(|()| locked[j].store(b)),
                        None => locked[j].delete(),
                    });
                receipt.files[j].outcome = match result {
                    Ok(()) => Outcome::RolledBack,
                    Err(e) => {
                        receipt.files[j].error = Some(e);
                        Outcome::RollbackFailed
                    }
                };
            }
            // Reservations for not-yet-attempted new files also need removal.
            for (j, file) in locked.iter_mut().enumerate().skip(i + 1) {
                if file.created
                    && let Err(e) = file.delete()
                {
                    receipt.files[j].outcome = Outcome::RollbackFailed;
                    receipt.files[j].error = Some(e);
                }
            }
            return Ok(receipt);
        }
        receipt.files[i].outcome = Outcome::Applied;
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflict_and_injected_failure_preserve_external_and_restore_each_file() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::write(&a, b"old").unwrap();
        let changes = vec![
            Change {
                path: a.clone(),
                before: Some(b"old".to_vec()),
                after: Some(b"new".to_vec()),
            },
            Change {
                path: b.clone(),
                before: None,
                after: Some(b"b".to_vec()),
            },
        ];
        assert!(execute(&changes, true).unwrap().preview);
        assert!(!b.exists());
        fs::write(&a, b"external").unwrap();
        assert!(!execute(&changes, false).unwrap().success);
        assert_eq!(fs::read(&a).unwrap(), b"external");
        assert!(!b.exists());
        fs::write(&a, b"old").unwrap();
        let report = execute_with(&changes, false, |i| {
            if i == Point::Apply(1) {
                Err(Failure::new(Code::Io, "injected disk failure", "inspect"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert!(!report.success);
        assert_eq!(report.files[0].outcome, Outcome::RolledBack);
        assert_eq!(report.files[1].outcome, Outcome::RolledBack);
        assert_eq!(fs::read(&a).unwrap(), b"old");
        assert!(!b.exists());
    }
    #[test]
    fn failed_rollback_is_reported_per_file_and_does_not_claim_restoration() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::write(&a, b"old-a").unwrap();
        fs::write(&b, b"old-b").unwrap();
        let changes = vec![
            Change {
                path: a.clone(),
                before: Some(b"old-a".to_vec()),
                after: Some(b"new-a".to_vec()),
            },
            Change {
                path: b.clone(),
                before: Some(b"old-b".to_vec()),
                after: Some(b"new-b".to_vec()),
            },
        ];
        let r = execute_with(&changes, false, |point| {
            if matches!(point, Point::Apply(1) | Point::Rollback(0)) {
                Err(Failure::new(
                    Code::Io,
                    "injected storage failure",
                    "inspect recovery data",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert!(!r.success);
        assert_eq!(r.files[0].outcome, Outcome::RollbackFailed);
        assert_eq!(r.files[1].outcome, Outcome::RolledBack);
        assert_eq!(fs::read(&a).unwrap(), b"new-a");
        assert_eq!(fs::read(&b).unwrap(), b"old-b");
    }
    #[cfg(windows)]
    #[test]
    fn windows_commit_excludes_external_writes_and_rename_until_handles_close() {
        use std::sync::mpsc;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a");
        fs::write(&path, b"old").unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let target = path.clone();
        let worker = std::thread::spawn(move || {
            execute_with(
                &[Change {
                    path: target,
                    before: Some(b"old".to_vec()),
                    after: Some(b"new".to_vec()),
                }],
                false,
                |point| {
                    if point == Point::Apply(0) {
                        ready_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                    Ok(())
                },
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let external = fs::OpenOptions::new().write(true).open(&path);
        let renamed = fs::rename(&path, d.path().join("renamed"));
        release_tx.send(()).unwrap();
        let r = worker.join().unwrap().unwrap();
        assert!(external.is_err());
        assert!(renamed.is_err());
        assert!(r.success);
        assert_eq!(fs::read(path).unwrap(), b"new");
    }
    #[test]
    fn multi_file_diff_output_has_a_global_budget() {
        let d = tempfile::tempdir().unwrap();
        let changes = (0..8)
            .map(|i| Change {
                path: d.path().join(i.to_string()),
                before: None,
                after: Some(("line\n".repeat(5000)).into_bytes()),
            })
            .collect::<Vec<_>>();
        let r = execute(&changes, true).unwrap();
        assert!(r.files.iter().map(|f| f.diff.len()).sum::<usize>() <= 32000);
        assert!(r.files.iter().any(|f| f.diff_truncated));
        assert!(fs::read_dir(d.path()).unwrap().next().is_none());
    }
}
