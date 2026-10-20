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
fn execute_with(
    changes: &[Change],
    preview: bool,
    mut before_write: impl FnMut(usize) -> Result<()>,
) -> Result<Receipt> {
    if changes
        .iter()
        .map(|c| c.before.as_ref().map_or(0, Vec::len) + c.after.as_ref().map_or(0, Vec::len))
        .sum::<usize>()
        > 64 * 1024 * 1024
    {
        return Err(Failure::invalid("Change set exceeds 64 MiB"));
    }
    let mut receipt = Receipt {
        preview,
        success: true,
        crash_atomic: false,
        files: changes.iter().map(Change::receipt).collect::<Result<_>>()?,
        error: None,
    };
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
        let result = before_write(i).and_then(|()| match &c.after {
            Some(b) => locked[i].store(b),
            None => locked[i].delete(),
        });
        if let Err(e) = result {
            receipt.success = false;
            receipt.error = Some(e.clone());
            receipt.files[i].error = Some(e);
            // The failing write may already have changed bytes: roll it back too.
            for j in (0..=i).rev() {
                let result = match &changes[j].before {
                    Some(b) => locked[j].undelete().and_then(|()| locked[j].store(b)),
                    None => locked[j].delete(),
                };
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
            if i == 1 {
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
}
