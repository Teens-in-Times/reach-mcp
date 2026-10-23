//! Durable before-images precede workspace writes. Recovery is explicit and version-guarded.
//! A journal is recovery material, not evidence of crash-atomic filesystem mutation.
use crate::{
    changes::{self, Change, Receipt},
    error::{Code, Failure, Result},
    files::hash,
    policy::Policy,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Version {
    Absent,
    Present { sha256: String },
}
impl Version {
    pub fn of(bytes: Option<&[u8]>) -> Self {
        bytes.map_or(Self::Absent, |b| Self::Present { sha256: hash(b) })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    before: Version,
    proposed: Version,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Phase {
    Prepared,
    Finished { receipt: Receipt },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u8,
    task_id: Uuid,
    owner: String,
    files: Vec<Entry>,
    #[serde(flatten)]
    phase: Phase,
}
#[derive(Debug, Serialize)]
pub struct InspectedFile {
    pub path: PathBuf,
    pub before: Version,
    pub proposed: Version,
    pub current: Version,
    pub matches_before: bool,
    pub matches_proposed: bool,
}
#[derive(Debug, Serialize)]
pub struct Inspection {
    pub source_task_id: Uuid,
    pub files: Vec<InspectedFile>,
    pub recorded_receipt: Option<Receipt>,
}
pub struct Commit {
    pub receipt: Receipt,
    pub recording_error: Option<Failure>,
}

fn manifest_path(root: &Path, id: Uuid) -> PathBuf {
    root.join(id.to_string()).join("manifest.json")
}
fn persist(root: &Path, m: &Manifest) -> Result<()> {
    let mut f = tempfile::NamedTempFile::new_in(root.join(m.task_id.to_string()))?;
    f.write_all(
        &serde_json::to_vec(m).map_err(|e| {
            Failure::new(Code::Internal, e.to_string(), "Inspect recovery storage.")
        })?,
    )?;
    f.as_file().sync_all()?;
    f.persist(manifest_path(root, m.task_id))
        .map_err(|e| e.error)?;
    Ok(())
}
fn prepare(root: &Path, id: Uuid, owner: &str, changes: &[Change]) -> Result<Manifest> {
    let directory = root.join(id.to_string());
    fs::create_dir_all(root)?;
    fs::create_dir(&directory)?;
    let mut files = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        if let Some(b) = &c.before {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(format!("{i}.before")))?;
            f.write_all(b)?;
            f.sync_all()?;
        }
        files.push(Entry {
            path: c.path.clone(),
            before: Version::of(c.before.as_deref()),
            proposed: Version::of(c.after.as_deref()),
        });
    }
    let m = Manifest {
        version: 3,
        task_id: id,
        owner: owner.into(),
        files,
        phase: Phase::Prepared,
    };
    persist(root, &m)?;
    Ok(m)
}
pub fn commit(root: &Path, id: Uuid, owner: &str, changes: &[Change]) -> Result<Commit> {
    commit_with(root, id, owner, changes, |changes| {
        changes::execute(changes, false)
    })
}
fn commit_with(
    root: &Path,
    id: Uuid,
    owner: &str,
    changes: &[Change],
    apply: impl FnOnce(&[Change]) -> Result<Receipt>,
) -> Result<Commit> {
    changes::validate(changes)?;
    let mut m = prepare(root, id, owner, changes).map_err(|e|Failure::new(e.code,format!("Cannot prepare durable before-images: {}",e.message),"This operation did not modify workspace files. Fix recovery storage before submitting distinct new work."))?;
    // No workspace write can happen before every before-image and the manifest are flushed.
    let receipt = apply(changes)?;
    m.phase = Phase::Finished {
        receipt: receipt.clone(),
    };
    let recording_error = persist(root, &m).err().map(|e| {
        Failure::new(
            Code::DurabilityLost,
            format!("Files were processed but the recovery receipt could not be saved: {e}"),
            "Inspect the returned per-file receipt and Recover before repeating any action.",
        )
    });
    Ok(Commit {
        receipt,
        recording_error,
    })
}
fn load(root: &Path, id: Uuid, owner: &str) -> Result<Manifest> {
    let mut bytes = Vec::new();
    fs::File::open(manifest_path(root, id))?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Failure::new(
            Code::InvalidState,
            "Oversized recovery manifest",
            "Inspect private storage.",
        ));
    }
    let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| {
        Failure::new(
            Code::InvalidState,
            e.to_string(),
            "Inspect the recovery manifest before proceeding.",
        )
    })?;
    if m.version != 3 || m.task_id != id || m.owner != owner {
        return Err(Failure::new(
            Code::NotFound,
            "Recovery journal not found",
            "Use an owned task_id with retained recovery data.",
        ));
    }
    Ok(m)
}
pub fn inspect(root: &Path, id: Uuid, owner: &str, policy: &Policy) -> Result<Inspection> {
    let m = load(root, id, owner)?;
    let mut files = Vec::new();
    for f in m.files {
        policy.check(&f.path)?;
        let current = Version::of(changes::bytes(&f.path)?.as_deref());
        files.push(InspectedFile {
            path: f.path,
            matches_before: f.before == current,
            matches_proposed: f.proposed == current,
            before: f.before,
            proposed: f.proposed,
            current,
        });
    }
    Ok(Inspection {
        source_task_id: id,
        files,
        recorded_receipt: match m.phase {
            Phase::Prepared => None,
            Phase::Finished { receipt } => Some(receipt),
        },
    })
}
pub fn restore_plan(
    root: &Path,
    id: Uuid,
    owner: &str,
    versions: &BTreeMap<PathBuf, Version>,
    policy: &Policy,
) -> Result<Vec<Change>> {
    let m = load(root, id, owner)?;
    if versions.len() != m.files.len() {
        return Err(Failure::invalid(
            "Restore requires an explicit expected version for every journal path",
        ));
    }
    let mut changes = Vec::new();
    for (i, f) in m.files.into_iter().enumerate() {
        policy.check(&f.path)?;
        let before = changes::bytes(&f.path)?;
        if versions.get(&f.path) != Some(&Version::of(before.as_deref())) {
            return Err(Failure::conflict(format!(
                "Recovery target changed: {}",
                f.path.display()
            )));
        }
        let after = match f.before {
            Version::Absent => None,
            Version::Present { sha256 } => {
                let bytes=changes::bytes(&root.join(id.to_string()).join(format!("{i}.before")))?.ok_or_else(||Failure::new(Code::InvalidState,"Recovery before-image missing","Restore from another trusted backup; no workspace write was attempted."))?;
                changes::verify(&sha256, &bytes)?;
                Some(bytes)
            }
        };
        if before != after {
            changes.push(Change {
                path: f.path,
                before,
                after,
            });
        }
    }
    if changes.is_empty() {
        return Err(Failure::new(
            Code::InvalidState,
            "Every file already matches its before-image",
            "No restoration is required.",
        ));
    }
    changes.sort_by_key(|c| c.after.is_none());
    Ok(changes)
}
pub fn disk_usage(path: &Path) -> Result<u64> {
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = 0;
    for entry in entries {
        let entry = entry?;
        let m = entry.metadata()?;
        if !m.is_file() {
            return Err(Failure::new(
                Code::InvalidState,
                "Unexpected directory in recovery journal",
                "Inspect private recovery storage.",
            ));
        }
        bytes += m.len();
    }
    Ok(bytes)
}
pub fn remove(root: &Path, id: Uuid) -> Result<()> {
    let path = root.join(id.to_string());
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if m.file_type().is_symlink() => Err(Failure::new(
            Code::InvalidState,
            "Recovery directory is a link",
            "Inspect private storage before cleanup.",
        )),
        Ok(_) => {
            fs::remove_dir_all(path)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepared_crash_journal_restores_only_explicit_current_versions() {
        let d = tempfile::tempdir().unwrap();
        let workspace = d.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let root = d.path().join("recovery");
        let file = workspace.join("a");
        fs::write(&file, b"before\r\n").unwrap();
        let id = Uuid::new_v4();
        let changes = vec![Change {
            path: file.clone(),
            before: Some(b"before\r\n".to_vec()),
            after: Some(b"after\r\n".to_vec()),
        }];
        prepare(&root, id, "owner", &changes).unwrap();
        fs::write(&file, b"partial").unwrap();
        let policy = Policy::unrestricted_for_test(&workspace);
        let inspected = inspect(&root, id, "owner", &policy).unwrap();
        assert!(inspected.recorded_receipt.is_none());
        assert!(!inspected.files[0].matches_before);
        let mut versions = BTreeMap::new();
        versions.insert(file.clone(), Version::of(Some(b"partial")));
        let plan = restore_plan(&root, id, "owner", &versions, &policy).unwrap();
        fs::write(&file, b"external").unwrap();
        assert!(!changes::execute(&plan, false).unwrap().success);
        assert_eq!(fs::read(&file).unwrap(), b"external");
        versions.insert(file.clone(), Version::of(Some(b"external")));
        let plan = restore_plan(&root, id, "owner", &versions, &policy).unwrap();
        assert!(changes::execute(&plan, false).unwrap().success);
        assert_eq!(fs::read(&file).unwrap(), b"before\r\n");
        assert!(inspect(&root, id, "another", &policy).is_err());
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "Child fixture: parent passes an isolated directory and kills it mid-commit"]
    fn crash_during_commit_fixture() {
        let root = PathBuf::from(
            std::env::var_os("REACH_FILE_CRASH_FIXTURE").expect("parent test directory required"),
        );
        let workspace = root.join("workspace");
        let id = Uuid::parse_str(&fs::read_to_string(root.join("id")).unwrap()).unwrap();
        let plan = ["a", "b"]
            .into_iter()
            .map(|name| Change {
                path: workspace.join(name),
                before: Some(format!("before-{name}").into_bytes()),
                after: Some(format!("after-{name}").into_bytes()),
            })
            .collect::<Vec<_>>();
        commit_with(&root.join("recovery"), id, "test", &plan, |changes| {
            changes::execute_with(changes, false, |point| {
                if point == changes::Point::AfterApply(0) {
                    fs::write(root.join("ready"), b"first file committed").unwrap();
                    loop {
                        std::thread::park_timeout(std::time::Duration::from_secs(1));
                    }
                }
                Ok(())
            })
        })
        .unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn actual_executor_death_between_files_leaves_recoverable_before_images() {
        use std::{
            os::windows::process::CommandExt,
            process::Command,
            time::{Duration, Instant},
        };
        let d = tempfile::tempdir().unwrap();
        let workspace = d.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        for name in ["a", "b"] {
            fs::write(workspace.join(name), format!("before-{name}")).unwrap();
        }
        let id = Uuid::new_v4();
        fs::write(d.path().join("id"), id.to_string()).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "journal::tests::crash_during_commit_fixture",
            ])
            .env("REACH_FILE_CRASH_FIXTURE", d.path())
            .creation_flags(0x08000000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !d.path().join("ready").exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let ready = d.path().join("ready").exists();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(ready, "Child did not reach the actual mid-commit barrier");
        assert_eq!(fs::read(workspace.join("a")).unwrap(), b"after-a");
        assert_eq!(fs::read(workspace.join("b")).unwrap(), b"before-b");
        let policy = Policy::unrestricted_for_test(&workspace);
        let inspection = inspect(&d.path().join("recovery"), id, "test", &policy).unwrap();
        assert!(inspection.recorded_receipt.is_none());
        assert!(inspection.files[0].matches_proposed);
        assert!(inspection.files[1].matches_before);
        let versions = inspection
            .files
            .into_iter()
            .map(|f| (f.path, f.current))
            .collect();
        let restored =
            restore_plan(&d.path().join("recovery"), id, "test", &versions, &policy).unwrap();
        assert!(changes::execute(&restored, false).unwrap().success);
        assert_eq!(fs::read(workspace.join("a")).unwrap(), b"before-a");
        assert_eq!(fs::read(workspace.join("b")).unwrap(), b"before-b");
    }
}
