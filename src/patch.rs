//! A small, strict implementation of the familiar *** Begin Patch format.
//! All edits are validated before writes. Each replacement is atomic; a multi-file
//! patch is not a crash-atomic filesystem transaction.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

fn target(cwd: &Path, name: &str) -> Result<PathBuf> {
    let relative = Path::new(name);
    ensure!(
        !name.is_empty()
            && !relative.is_absolute()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "Patch paths must be relative, without . or .."
    );
    let path = cwd.join(relative);
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("Missing path parent")?;
    }
    ensure!(
        fs::canonicalize(ancestor)?.starts_with(fs::canonicalize(cwd)?),
        "Patch path leaves cwd through a link"
    );
    ensure!(!path.is_dir(), "Patch target is a directory");
    Ok(path)
}
fn bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => {
            ensure!(b.len() <= 16 * 1024 * 1024, "Patch file exceeds 16 MiB");
            Ok(Some(b))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content)?;
    if let Ok(m) = fs::metadata(path) {
        temp.as_file().set_permissions(m.permissions())?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn apply(cwd: &Path, args: &Value) -> Result<Value> {
    apply_with_policy(cwd, args, None)
}
pub fn apply_with_policy(
    cwd: &Path,
    args: &Value,
    policy: Option<&crate::policy::Policy>,
) -> Result<Value> {
    let patch = crate::files::text_arg(args, "patch")?;
    ensure!(patch.len() <= 1024 * 1024, "Patch exceeds 1 MiB");
    let lines: Vec<&str> = patch.lines().collect();
    ensure!(
        lines.first() == Some(&"*** Begin Patch") && lines.last() == Some(&"*** End Patch"),
        "Expected *** Begin Patch and *** End Patch"
    );
    let hashes = args.get("expected_sha256").and_then(Value::as_object);
    let mut changes: Vec<Change> = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 1;
    while i + 1 < lines.len() {
        ensure!(changes.len() < 100, "Patch exceeds 100 file operations");
        let header = lines[i];
        i += 1;
        let (kind, name) = if let Some(p) = header.strip_prefix("*** Add File: ") {
            ("add", p)
        } else if let Some(p) = header.strip_prefix("*** Update File: ") {
            ("update", p)
        } else if let Some(p) = header.strip_prefix("*** Delete File: ") {
            ("delete", p)
        } else {
            bail!("Unknown patch file header: {header}")
        };
        let path = target(cwd, name)?;
        if let Some(policy) = policy {
            policy.check(&path)?;
        }
        ensure!(seen.insert(path_key(&path)), "Duplicate patch target");
        let before = bytes(&path)?;
        if let Some(expected) = hashes.and_then(|h| h.get(name)) {
            ensure!(
                expected.as_str()
                    == Some(crate::files::hash(before.as_deref().unwrap_or_default()).as_str()),
                "Stale file: {name}"
            );
        }
        match kind {
            "add" => {
                ensure!(before.is_none(), "Add target already exists: {name}");
                let mut text = String::new();
                while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                    text.push_str(
                        lines[i]
                            .strip_prefix('+')
                            .context("Added lines must begin with +")?,
                    );
                    text.push('\n');
                    i += 1;
                }
                changes.push(Change {
                    path,
                    before,
                    after: Some(text.into_bytes()),
                });
            }
            "delete" => {
                ensure!(before.is_some(), "Delete target not found: {name}");
                changes.push(Change {
                    path,
                    before,
                    after: None,
                });
            }
            _ => {
                let original =
                    std::str::from_utf8(before.as_deref().context("Update target not found")?)
                        .context("Patch requires UTF-8")?;
                let crlf = original.contains("\r\n");
                let trailing = original.ends_with('\n');
                let mut content: Vec<String> = original.lines().map(String::from).collect();
                let mut destination = None;
                if let Some(move_to) = lines.get(i).and_then(|l| l.strip_prefix("*** Move to: ")) {
                    let to = target(cwd, move_to)?;
                    if let Some(policy) = policy {
                        policy.check(&to)?;
                    }
                    ensure!(
                        seen.insert(path_key(&to)) && bytes(&to)?.is_none(),
                        "Move target already exists or is duplicated"
                    );
                    destination = Some(to);
                    i += 1;
                }
                let mut cursor = 0;
                let mut hunks = 0;
                while i + 1 < lines.len() && lines[i].starts_with("@@") {
                    hunks += 1;
                    i += 1;
                    let mut old = Vec::new();
                    let mut new = Vec::new();
                    while i + 1 < lines.len()
                        && !lines[i].starts_with("@@")
                        && !lines[i].starts_with("*** ")
                    {
                        let line = lines[i];
                        i += 1;
                        let (prefix, text) = line
                            .split_at_checked(1)
                            .context("Patch lines require a prefix")?;
                        match prefix {
                            " " => {
                                old.push(text.to_string());
                                new.push(text.to_string());
                            }
                            "-" => old.push(text.to_string()),
                            "+" => new.push(text.to_string()),
                            _ => bail!("Hunk lines require space, + or -"),
                        }
                    }
                    let eof = lines.get(i) == Some(&"*** End of File");
                    if eof {
                        i += 1;
                    }
                    ensure!(
                        !old.is_empty(),
                        "Update hunks require context or removed lines; use Add for a new file"
                    );
                    let matches: Vec<usize> = content
                        .windows(old.len())
                        .enumerate()
                        .filter(|(pos, window)| {
                            *pos >= cursor
                                && *window == old.as_slice()
                                && (!eof || *pos + old.len() == content.len())
                        })
                        .map(|(pos, _)| pos)
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "Hunk context is missing or ambiguous in {name}; read again and include unique context"
                    );
                    let pos = matches[0];
                    let new_len = new.len();
                    content.splice(pos..pos + old.len(), new);
                    cursor = pos + new_len;
                }
                ensure!(hunks > 0, "Update requires at least one @@ hunk");
                let sep = if crlf { "\r\n" } else { "\n" };
                let mut updated = content.join(sep);
                if trailing {
                    updated.push_str(sep);
                }
                if let Some(to) = destination {
                    changes.push(Change {
                        path,
                        before,
                        after: None,
                    });
                    changes.push(Change {
                        path: to,
                        before: None,
                        after: Some(updated.into_bytes()),
                    });
                } else {
                    changes.push(Change {
                        path,
                        before,
                        after: Some(updated.into_bytes()),
                    });
                }
            }
        }
    }
    ensure!(!changes.is_empty(), "Empty patch");
    // Write destinations before deleting move sources, reducing damage from interruption.
    changes.sort_by_key(|change| change.after.is_none());
    for change in &changes {
        ensure!(
            bytes(&change.path)? == change.before,
            "File changed while validating patch"
        );
    }
    for (committed, change) in changes.iter().enumerate() {
        let result = if let Some(after) = &change.after {
            atomic(&change.path, after)
        } else {
            fs::remove_file(&change.path).map_err(Into::into)
        };
        if let Err(error) = result {
            let mut rollback_errors = Vec::new();
            for old in changes[..committed].iter().rev() {
                let result = if let Some(before) = &old.before {
                    atomic(&old.path, before)
                } else {
                    fs::remove_file(&old.path).map_err(Into::into)
                };
                if let Err(e) = result {
                    rollback_errors.push(e.to_string());
                }
            }
            bail!("Patch commit failed: {error}; rollback errors: {rollback_errors:?}");
        }
    }
    Ok(
        json!({"files":changes.iter().map(|c|json!({"path":c.path.strip_prefix(cwd).unwrap_or(&c.path),"action":if c.after.is_none(){"deleted"}else if c.before.is_none(){"created"}else{"updated"},"sha256":c.after.as_deref().map(crate::files::hash)})).collect::<Vec<_>>(),"validation_atomic":true,"crash_atomic":false}),
    )
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_every_file_before_writing_and_preserves_crlf() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), "one\r\ntwo\r\n").unwrap();
        let bad = json!({"patch":"*** Begin Patch\n*** Add File: new\n+hello\n*** Update File: a\n@@\n-missing\n+bad\n*** End Patch"});
        assert!(apply(d.path(), &bad).is_err());
        assert!(!d.path().join("new").exists());
        let good = json!({"patch":"*** Begin Patch\n*** Update File: a\n@@\n one\n-two\n+three\n*** Add File: new\n+hello\n*** End Patch"});
        apply(d.path(), &good).unwrap();
        assert_eq!(fs::read(d.path().join("a")).unwrap(), b"one\r\nthree\r\n");
        assert_eq!(fs::read(d.path().join("new")).unwrap(), b"hello\n");
        assert!(
            apply(
                d.path(),
                &json!({"patch":"*** Begin Patch\n*** Delete File: ../outside\n*** End Patch"})
            )
            .is_err()
        );
    }
    #[test]
    fn unique_context_hash_move_and_delete() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), "x\nx\n").unwrap();
        assert!(
            apply(
                d.path(),
                &json!({"patch":"*** Begin Patch\n*** Update File: a\n@@\n-x\n+y\n*** End Patch"})
            )
            .is_err()
        );
        let p =
            "*** Begin Patch\n*** Update File: a\n*** Move to: b\n@@\n x\n-x\n+y\n*** End Patch";
        assert!(
            apply(
                d.path(),
                &json!({"patch":p,"expected_sha256":{"a":"stale"}})
            )
            .is_err()
        );
        apply(d.path(), &json!({"patch":p})).unwrap();
        assert!(!d.path().join("a").exists());
        assert_eq!(fs::read_to_string(d.path().join("b")).unwrap(), "x\ny\n");
        apply(
            d.path(),
            &json!({"patch":"*** Begin Patch\n*** Delete File: b\n*** End Patch"}),
        )
        .unwrap();
        assert!(!d.path().join("b").exists());
    }
}
