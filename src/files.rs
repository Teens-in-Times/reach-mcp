use crate::{
    changes,
    error::{Code, Failure, Result},
    operation::{GlobArgs, GrepArgs, ReadArgs},
    policy::Policy,
};
use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_owned()
    } else {
        cwd.join(p)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Utf8,
    Utf8Bom,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineEndings {
    pub lf: usize,
    pub crlf: usize,
    pub lone_cr: usize,
}
impl LineEndings {
    pub fn of(s: &str) -> Self {
        let crlf = s.as_bytes().windows(2).filter(|w| *w == b"\r\n").count();
        Self {
            lf: s.bytes().filter(|b| *b == b'\n').count() - crlf,
            crlf,
            lone_cr: s.bytes().filter(|b| *b == b'\r').count() - crlf,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextFile {
    pub path: PathBuf,
    pub sha256: String,
    pub encoding: Encoding,
    pub line_endings: LineEndings,
    pub total_lines: usize,
    pub text: String,
    pub next_offset: Option<usize>,
    pub long_lines_truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub directory: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Directory {
    pub path: PathBuf,
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathFailure {
    pub path: PathBuf,
    pub error: Failure,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadEntry {
    Text(TextFile),
    Directory(Directory),
    Error(PathFailure),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadBatch {
    pub files: Vec<ReadEntry>,
}

pub fn read(cwd: &Path, a: &ReadArgs, policy: &Policy) -> Result<ReadBatch> {
    let offset = a.offset.map(|n| n.get() as usize).unwrap_or(1);
    let limit = a.limit.map(|n| n.get() as usize).unwrap_or(200);
    let mut budget = 32000usize;
    let mut files = Vec::new();
    for name in a.paths.iter() {
        let path = resolve(cwd, name);
        let entry = (|| -> Result<ReadEntry> {
            policy.check(&path)?;
            if budget < 512 {
                return Err(Failure::new(
                    Code::CapacityExceeded,
                    "Batch output budget reached",
                    "Read this path separately.",
                ));
            }
            if path.is_dir() {
                let mut all = fs::read_dir(&path)?
                    .take(2001)
                    .collect::<std::io::Result<Vec<_>>>()?;
                all.sort_by_key(|e| e.file_name());
                let mut entries = Vec::new();
                for e in all.iter().take(200) {
                    if policy.check(&e.path()).is_err() {
                        continue;
                    }
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.len() + 64 > budget {
                        break;
                    }
                    budget -= name.len() + 64;
                    entries.push(DirectoryEntry {
                        name,
                        directory: e.file_type()?.is_dir(),
                    });
                }
                let truncated = entries.len() < all.len();
                return Ok(ReadEntry::Directory(Directory {
                    path: path.clone(),
                    entries,
                    truncated,
                }));
            }
            let bytes = changes::bytes(&path)?.ok_or_else(|| {
                Failure::new(
                    Code::NotFound,
                    "File not found",
                    "Check the path or list its parent directory.",
                )
            })?;
            let original = changes::text(&bytes)?;
            let (text, encoding) = if let Some(s) = original.strip_prefix('\u{feff}') {
                (s, Encoding::Utf8Bom)
            } else {
                (original, Encoding::Utf8)
            };
            let total_lines = text.lines().count();
            if offset > total_lines.max(1) {
                return Err(Failure::invalid(format!(
                    "offset exceeds {total_lines} lines"
                )));
            }
            let mut lines = Vec::new();
            let mut next = offset;
            let mut long_lines_truncated = false;
            for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                let shown: String = line.chars().take(2000).collect();
                let row = format!("{}: {}", i + 1, shown);
                if row.len() + 32 > budget {
                    break;
                }
                long_lines_truncated |= shown.len() < line.len();
                budget -= row.len() + 32;
                lines.push(row);
                next = i + 2;
            }
            Ok(ReadEntry::Text(TextFile {
                path: path.clone(),
                sha256: hash(&bytes),
                encoding,
                line_endings: LineEndings::of(text),
                total_lines,
                text: lines.join("\n"),
                next_offset: (next <= total_lines).then_some(next),
                long_lines_truncated,
            }))
        })();
        files.push(entry.unwrap_or_else(|error| ReadEntry::Error(PathFailure { path, error })));
    }
    Ok(ReadBatch { files })
}

pub fn plan_write(
    cwd: &Path,
    args: &crate::operation::WriteArgs,
    policy: &Policy,
) -> Result<Vec<changes::Change>> {
    use crate::operation::WriteContent;
    let path = resolve(cwd, &args.path);
    policy.check(&path)?;
    let (content, expected, append) = match &args.update {
        WriteContent::Create { content } => (content, None, false),
        WriteContent::Replace {
            content,
            expected_sha256,
        } => (content, Some(expected_sha256.as_str()), false),
        WriteContent::Append {
            content,
            expected_sha256,
        } => (content, Some(expected_sha256.as_str()), true),
    };
    if content.len() > changes::MAX_FILE {
        return Err(Failure::invalid("Content exceeds 16 MiB"));
    }
    changes::text(content.as_bytes())?;
    let before = changes::bytes(&path)?;
    if let Some(expected) = expected {
        let old = before
            .as_ref()
            .ok_or_else(|| Failure::conflict("replace/append requires an existing file"))?;
        changes::text(old)?;
        changes::verify(expected, old)?;
    } else if before.is_some() {
        return Err(Failure::conflict("create refuses an existing file"));
    }
    let after = if append {
        let mut b = before.clone().expect("existing append target");
        b.extend_from_slice(content.as_bytes());
        b
    } else {
        content.as_bytes().to_vec()
    };
    if after.len() > changes::MAX_FILE {
        return Err(Failure::invalid("Result exceeds 16 MiB"));
    }
    Ok(vec![changes::Change {
        path,
        before,
        after: Some(after),
    }])
}
pub fn plan_edit(
    cwd: &Path,
    args: &crate::operation::EditArgs,
    policy: &crate::policy::Policy,
) -> crate::error::Result<Vec<crate::changes::Change>> {
    use crate::{
        changes::{self, Change},
        error::{Code, Failure},
    };
    let path = resolve(cwd, &args.path);
    policy.check(&path)?;
    let before = changes::bytes(&path)?.ok_or_else(|| {
        Failure::new(
            Code::NotFound,
            "Edit target not found",
            "Read the target first.",
        )
    })?;
    changes::verify(&args.expected_sha256, &before)?;
    let text = changes::text(&before)?;
    if args.old_string.is_empty() {
        return Err(Failure::invalid("old_string must not be empty"));
    }
    let matches = text.matches(&args.old_string).count();
    if matches == 0 {
        return Err(Failure::conflict("old_string was not found"));
    }
    if matches > 1 && !args.replace_all {
        return Err(Failure::new(
            Code::AmbiguousMatch,
            format!("old_string occurs {matches} times"),
            "Include unique context, or explicitly choose replace_all.",
        ));
    }
    let after = if args.replace_all {
        text.replace(&args.old_string, &args.new_string)
    } else {
        text.replacen(&args.old_string, &args.new_string, 1)
    }
    .into_bytes();
    changes::text(&after)?;
    if after.len() > changes::MAX_FILE {
        return Err(Failure::invalid("Result exceeds 16 MiB"));
    }
    Ok(vec![Change {
        path,
        before: Some(before),
        after: Some(after),
    }])
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_truncated: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub root: PathBuf,
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    pub skipped: usize,
}
pub enum Search<'a> {
    Glob(&'a GlobArgs),
    Grep(&'a GrepArgs),
}
pub fn search(cwd: &Path, query: Search<'_>, policy: &Policy) -> Result<SearchResult> {
    let (pattern, path, filter, max, grep, files_only) = match query {
        Search::Glob(a) => (
            &a.pattern,
            a.path.as_deref(),
            Some(a.pattern.as_str()),
            a.limit.map(|n| n.get() as usize).unwrap_or(200),
            None,
            false,
        ),
        Search::Grep(a) => (
            &a.pattern,
            a.path.as_deref(),
            a.glob.as_deref(),
            a.limit.map(|n| n.get() as usize).unwrap_or(200),
            Some(a),
            a.files_only,
        ),
    };
    if pattern.len() > 16384 {
        return Err(Failure::invalid("Pattern exceeds 16 KiB"));
    }
    let root = resolve(cwd, path.unwrap_or("."));
    policy.check(&root)?;
    let root = fs::canonicalize(root)?;
    let glob = filter
        .map(|s| Glob::new(s).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| Failure::invalid(e.to_string()))?;
    let regex = grep
        .map(|a| {
            let pattern = if a.fixed_strings {
                regex::escape(pattern)
            } else {
                pattern.clone()
            };
            RegexBuilder::new(&pattern)
                .case_insensitive(a.case_insensitive)
                .size_limit(2 * 1024 * 1024)
                .build()
                .map_err(|e| Failure::invalid(e.to_string()))
        })
        .transpose()?;
    let walk_policy = policy.clone();
    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .git_global(false)
        .filter_entry(move |e| {
            walk_policy.allows_walk(e.path())
                && (e.depth() == 0
                    || !e.file_type().is_some_and(|t| t.is_dir())
                    || !matches!(
                        e.file_name().to_str(),
                        Some(
                            ".git"
                                | "node_modules"
                                | "target"
                                | "dist"
                                | "build"
                                | ".venv"
                                | "__pycache__"
                                | ".next"
                        )
                    ))
        })
        .build();
    let mut matches = Vec::new();
    let mut skipped = 0;
    let mut budget = 24000usize;
    let mut truncated = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    'walk: for (visited, entry) in walker.enumerate() {
        if visited >= 200000 || Instant::now() > deadline {
            truncated = true;
            break;
        }
        let e = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let relative = e.path().strip_prefix(&root).unwrap_or(e.path());
        let relative = if relative.as_os_str().is_empty() {
            e.path().file_name().map(Path::new).unwrap_or(relative)
        } else {
            relative
        };
        if glob.as_ref().is_some_and(|g| !g.is_match(relative)) {
            continue;
        }
        if grep.is_none() {
            let size = relative.as_os_str().len() + 64;
            if matches.len() >= max || size > budget {
                truncated = true;
                break;
            }
            budget -= size;
            matches.push(SearchMatch {
                path: relative.to_owned(),
                line: None,
                text: None,
                line_truncated: None,
            });
            continue;
        }
        if e.metadata().map_or(true, |m| m.len() > 4 * 1024 * 1024) {
            skipped += 1;
            continue;
        }
        let bytes = match changes::bytes(e.path()) {
            Ok(Some(b)) => b,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let text = match changes::text(&bytes) {
            Ok(t) => t,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        for (i, line) in text
            .strip_prefix('\u{feff}')
            .unwrap_or(text)
            .lines()
            .enumerate()
        {
            if i % 1024 == 0 && Instant::now() > deadline {
                truncated = true;
                break 'walk;
            }
            if !regex.as_ref().expect("Grep pattern").is_match(line) {
                continue;
            }
            let shown = (!files_only).then(|| line.chars().take(1500).collect::<String>());
            let size = relative.as_os_str().len() + shown.as_ref().map_or(0, String::len) + 128;
            if matches.len() >= max || size > budget {
                truncated = true;
                break 'walk;
            }
            budget -= size;
            matches.push(SearchMatch {
                path: relative.to_owned(),
                line: (!files_only).then_some(i + 1),
                text: shown,
                line_truncated: (!files_only).then(|| line.chars().count() > 1500),
            });
            if files_only {
                break;
            }
        }
    }
    Ok(SearchResult {
        root,
        matches,
        truncated,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_reports_encoding_partial_failures_and_search_respects_ignores() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "\u{feff}one\r\ntwo\nthree").unwrap();
        fs::write(d.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(d.path().join("ignored.txt"), "two").unwrap();
        let policy = Policy::unrestricted_for_test(d.path());
        let read_args =
            serde_json::from_value(serde_json::json!({"paths":["a.txt","missing"],"limit":2}))
                .unwrap();
        let r = read(d.path(), &read_args, &policy).unwrap();
        let ReadEntry::Text(first) = &r.files[0] else {
            panic!("expected text")
        };
        assert!(matches!(first.encoding, Encoding::Utf8Bom));
        assert_eq!(first.line_endings.crlf, 1);
        assert_eq!(first.line_endings.lf, 1);
        assert_eq!(first.next_offset, Some(3));
        assert_eq!(first.text, "1: one\n2: two");
        assert!(matches!(&r.files[1],ReadEntry::Error(e)if e.error.code==Code::NotFound));
        let a =
            serde_json::from_value(serde_json::json!({"pattern":"two","glob":"*.txt"})).unwrap();
        let found = search(d.path(), Search::Grep(&a), &policy).unwrap();
        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].path, PathBuf::from("a.txt"));
    }
    #[test]
    fn create_append_replace_and_exact_edit_have_distinct_versioned_semantics() {
        use crate::operation::WriteContent;
        let d = tempfile::tempdir().unwrap();
        let policy = Policy::unrestricted_for_test(d.path());
        let mut a: crate::operation::WriteArgs = serde_json::from_value(
            serde_json::json!({"path":"a","update":{"mode":"create","content":"one\r\n"}}),
        )
        .unwrap();
        assert!(
            changes::execute(&plan_write(d.path(), &a, &policy).unwrap(), false)
                .unwrap()
                .success
        );
        assert!(plan_write(d.path(), &a, &policy).is_err());
        a.update = WriteContent::Append {
            content: "two\r\n".into(),
            expected_sha256: hash(b"stale"),
        };
        assert_eq!(
            plan_write(d.path(), &a, &policy).err().unwrap().code,
            Code::Conflict
        );
        a.update = WriteContent::Append {
            content: "two\r\n".into(),
            expected_sha256: hash(b"one\r\n"),
        };
        changes::execute(&plan_write(d.path(), &a, &policy).unwrap(), false).unwrap();
        assert_eq!(fs::read(d.path().join("a")).unwrap(), b"one\r\ntwo\r\n");
        let edit_args=serde_json::from_value(serde_json::json!({"path":"a","old_string":"one","new_string":"ONE","expected_sha256":hash(b"one\r\ntwo\r\n")})).unwrap();
        changes::execute(&plan_edit(d.path(), &edit_args, &policy).unwrap(), false).unwrap();
        assert_eq!(fs::read(d.path().join("a")).unwrap(), b"ONE\r\ntwo\r\n");
        fs::write(d.path().join("utf16"), [0xff, 0xfe, 65, 0]).unwrap();
        a.path = "utf16".into();
        a.update = WriteContent::Replace {
            content: "bad".into(),
            expected_sha256: hash(&[0xff, 0xfe, 65, 0]),
        };
        assert_eq!(
            plan_write(d.path(), &a, &policy).err().unwrap().code,
            Code::UnsupportedEncoding
        );
        assert_eq!(
            fs::read(d.path().join("utf16")).unwrap(),
            [0xff, 0xfe, 65, 0]
        );
    }
}
