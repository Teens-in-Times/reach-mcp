//! Strict contextual patch planning. Applying plans and reporting rollback belongs to changes.
use crate::{
    changes::{self, Change},
    error::{Code, Failure, Result},
    operation::PatchArgs,
    policy::Policy,
};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
};
#[derive(Clone)]
struct Line {
    body: String,
    ending: &'static str,
}
fn lines(s: &str) -> Vec<Line> {
    s.split_inclusive('\n')
        .map(|s| {
            if let Some(body) = s.strip_suffix("\r\n") {
                Line {
                    body: body.into(),
                    ending: "\r\n",
                }
            } else if let Some(body) = s.strip_suffix('\n') {
                Line {
                    body: body.into(),
                    ending: "\n",
                }
            } else {
                Line {
                    body: s.into(),
                    ending: "",
                }
            }
        })
        .collect()
}
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Update,
    Delete,
}
fn target(cwd: &Path, name: &str, policy: &Policy) -> Result<PathBuf> {
    let relative = Path::new(name);
    if name.is_empty()
        || relative.is_absolute()
        || !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Failure::invalid(
            "Patch paths must be relative, without . or ..",
        ));
    }
    let path = cwd.join(relative);
    policy.check(&path)?;
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| Failure::invalid("Missing path parent"))?;
    }
    if !fs::canonicalize(ancestor)?.starts_with(fs::canonicalize(cwd)?) {
        return Err(Failure::new(
            Code::PermissionDenied,
            "Patch target leaves cwd through a link",
            "Choose a path inside cwd.",
        ));
    }
    if path.is_dir() {
        return Err(Failure::invalid("Patch target is a directory"));
    }
    Ok(path)
}
fn key(p: &Path) -> String {
    let p = p.to_string_lossy();
    if cfg!(windows) {
        p.to_lowercase()
    } else {
        p.into_owned()
    }
}
pub fn plan(cwd: &Path, args: &PatchArgs, policy: &Policy) -> Result<Vec<Change>> {
    if args.patch.len() > 1024 * 1024 {
        return Err(Failure::invalid("Patch exceeds 1 MiB"));
    }
    let patch: Vec<_> = args.patch.lines().collect();
    if patch.first() != Some(&"*** Begin Patch") || patch.last() != Some(&"*** End Patch") {
        return Err(Failure::invalid(
            "Expected *** Begin Patch and *** End Patch",
        ));
    }
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 1;
    while i + 1 < patch.len() {
        let header = patch[i];
        i += 1;
        let (kind, name) = if let Some(p) = header.strip_prefix("*** Add File: ") {
            (Kind::Add, p)
        } else if let Some(p) = header.strip_prefix("*** Update File: ") {
            (Kind::Update, p)
        } else if let Some(p) = header.strip_prefix("*** Delete File: ") {
            (Kind::Delete, p)
        } else {
            return Err(Failure::invalid(format!(
                "Unknown patch file header: {header}"
            )));
        };
        let path = target(cwd, name, policy)?;
        if !seen.insert(key(&path)) {
            return Err(Failure::invalid("Duplicate patch target"));
        }
        let before = changes::bytes(&path)?;
        if kind == Kind::Add {
            if before.is_some() {
                return Err(Failure::conflict(format!(
                    "Add target already exists: {name}"
                )));
            }
            if args.expected_sha256.contains_key(name) {
                return Err(Failure::invalid("New file must not have expected_sha256"));
            }
        } else {
            let b = before.as_deref().ok_or_else(|| {
                Failure::new(
                    Code::NotFound,
                    format!("Patch source not found: {name}"),
                    "Read the source before preparing a patch.",
                )
            })?;
            changes::verify(
                args.expected_sha256.get(name).ok_or_else(|| {
                    Failure::invalid(format!("Missing expected_sha256 for {name}"))
                })?,
                b,
            )?;
        }
        match kind {
            Kind::Add => {
                let mut text = String::new();
                while i + 1 < patch.len() && !patch[i].starts_with("*** ") {
                    text.push_str(
                        patch[i]
                            .strip_prefix('+')
                            .ok_or_else(|| Failure::invalid("Added lines must begin with +"))?,
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
            Kind::Delete => changes.push(Change {
                path,
                before,
                after: None,
            }),
            Kind::Update => {
                let original = changes::text(before.as_deref().expect("validated source"))?;
                let (original, bom) = if let Some(s) = original.strip_prefix('\u{feff}') {
                    (s, true)
                } else {
                    (original, false)
                };
                let trailing = original.ends_with('\n');
                let mut content = lines(original);
                let default_ending = content
                    .iter()
                    .find(|l| !l.ending.is_empty())
                    .map(|l| l.ending)
                    .unwrap_or("\n");
                let destination = if let Some(name) =
                    patch.get(i).and_then(|l| l.strip_prefix("*** Move to: "))
                {
                    i += 1;
                    let p = target(cwd, name, policy)?;
                    if !seen.insert(key(&p)) || changes::bytes(&p)?.is_some() {
                        return Err(Failure::conflict("Move target exists or is duplicated"));
                    }
                    Some(p)
                } else {
                    None
                };
                let mut cursor = 0;
                let mut hunks = 0;
                while i + 1 < patch.len() && patch[i].starts_with("@@") {
                    hunks += 1;
                    i += 1;
                    let mut old: Vec<&str> = Vec::new();
                    let mut new: Vec<(&str, Option<usize>)> = Vec::new();
                    let mut removed = VecDeque::new();
                    while i + 1 < patch.len()
                        && !patch[i].starts_with("@@")
                        && !patch[i].starts_with("*** ")
                    {
                        let line = patch[i];
                        i += 1;
                        let (prefix, text) = line
                            .split_at_checked(1)
                            .ok_or_else(|| Failure::invalid("Patch lines require a prefix"))?;
                        match prefix {
                            " " => {
                                removed.clear();
                                new.push((text, Some(old.len())));
                                old.push(text);
                            }
                            "-" => {
                                removed.push_back(old.len());
                                old.push(text);
                            }
                            "+" => new.push((
                                text,
                                removed.pop_front().or_else(|| old.len().checked_sub(1)),
                            )),
                            _ => return Err(Failure::invalid("Hunk lines require space, + or -")),
                        }
                    }
                    let eof = patch.get(i) == Some(&"*** End of File");
                    if eof {
                        i += 1;
                    }
                    if old.is_empty() {
                        return Err(Failure::invalid(
                            "Update requires nonempty context; use Write to replace an empty file",
                        ));
                    }
                    let found = content
                        .windows(old.len())
                        .enumerate()
                        .filter(|(pos, w)| {
                            *pos >= cursor
                                && w.iter().map(|l| l.body.as_str()).eq(old.iter().copied())
                                && (!eof || pos + old.len() == content.len())
                        })
                        .map(|(p, _)| p)
                        .collect::<Vec<_>>();
                    if found.is_empty() {
                        return Err(Failure::conflict(format!(
                            "Patch context missing in {name}"
                        )));
                    }
                    if found.len() != 1 {
                        return Err(Failure::new(
                            Code::AmbiguousMatch,
                            format!("Patch context matches {} times in {name}", found.len()),
                            "Read again and provide unique context or an explicit End of File marker.",
                        ));
                    }
                    let pos = found[0];
                    let ending = content[pos..pos + old.len()]
                        .iter()
                        .find(|l| !l.ending.is_empty())
                        .map(|l| l.ending)
                        .unwrap_or(default_ending);
                    let updated = new
                        .iter()
                        .map(|(body, source)| Line {
                            body: (*body).into(),
                            ending: source
                                .map(|n| content[pos + n].ending)
                                .filter(|s| !s.is_empty())
                                .unwrap_or(ending),
                        })
                        .collect::<Vec<_>>();
                    cursor = pos + updated.len();
                    content.splice(pos..pos + old.len(), updated);
                }
                if hunks == 0 && destination.is_none() {
                    return Err(Failure::invalid(
                        "Update requires an @@ hunk or a move destination",
                    ));
                }
                if !trailing && let Some(last) = content.last_mut() {
                    last.ending = "";
                }
                let mut updated = if bom {
                    "\u{feff}".to_owned()
                } else {
                    String::new()
                };
                for line in content {
                    updated.push_str(&line.body);
                    updated.push_str(line.ending);
                }
                let after = Some(updated.into_bytes());
                if let Some(to) = destination {
                    changes.push(Change {
                        path,
                        before,
                        after: None,
                    });
                    changes.push(Change {
                        path: to,
                        before: None,
                        after,
                    });
                } else {
                    changes.push(Change {
                        path,
                        before,
                        after,
                    });
                }
            }
        }
        if changes.len() > 100
            || changes
                .iter()
                .map(|c| {
                    c.before.as_ref().map_or(0, Vec::len) + c.after.as_ref().map_or(0, Vec::len)
                })
                .sum::<usize>()
                > 64 * 1024 * 1024
        {
            return Err(Failure::invalid("Patch exceeds 100 changes or 64 MiB"));
        }
    }
    if changes.is_empty() {
        return Err(Failure::invalid("Empty patch"));
    }
    changes.sort_by_key(|c| c.after.is_none());
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn apply(cwd: &Path, v: &Value) -> anyhow::Result<crate::changes::Receipt> {
        let mut v = v.clone();
        if v.get("expected_sha256").is_none() {
            let hashes = fs::read_dir(cwd)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        json!(crate::files::hash(&fs::read(e.path()).unwrap())),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            v["expected_sha256"] = Value::Object(hashes);
        }
        let args = serde_json::from_value(v)?;
        let policy = crate::policy::Policy::unrestricted_for_test(cwd);
        Ok(crate::changes::execute(
            &super::plan(cwd, &args, &policy)?,
            args.preview,
        )?)
    }
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
    #[test]
    fn mixed_line_endings_bom_and_missing_final_newline_survive_contextual_edits() {
        let d = tempfile::tempdir().unwrap();
        let original = "\u{feff}one\r\ntwo\nthree\r\nlast";
        fs::write(d.path().join("a"), original).unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: a\n@@\n one\n-two\n+TWO\n three\n*** End Patch";
        let preview = apply(d.path(), &json!({"patch":patch,"preview":true})).unwrap();
        assert!(preview.preview);
        assert_eq!(fs::read_to_string(d.path().join("a")).unwrap(), original);
        apply(d.path(), &json!({"patch":patch})).unwrap();
        assert_eq!(
            fs::read_to_string(d.path().join("a")).unwrap(),
            "\u{feff}one\r\nTWO\nthree\r\nlast"
        );
    }
}
