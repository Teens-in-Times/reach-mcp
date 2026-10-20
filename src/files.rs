use anyhow::{Context, Result, bail, ensure};
use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_FILE: u64 = 16 * 1024 * 1024;

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn text_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("{name} must be a string"))
}

pub fn count(args: &Value, name: &str, default: u64, max: u64) -> Result<usize> {
    match args.get(name) {
        None => Ok(default as usize),
        Some(v) => {
            let n = v
                .as_u64()
                .with_context(|| format!("{name} must be a positive integer"))?;
            ensure!(n > 0 && n <= max, "{name} must be between 1 and {max}");
            Ok(n as usize)
        }
    }
}

pub fn flag(args: &Value, name: &str) -> Result<bool> {
    match args.get(name) {
        None => Ok(false),
        Some(v) => v
            .as_bool()
            .with_context(|| format!("{name} must be boolean")),
    }
}

pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::metadata(path)?.len() <= MAX_FILE,
        "File exceeds 16 MiB; narrow the query or use Shell"
    );
    let bytes = fs::read(path)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "File grew beyond 16 MiB");
    Ok(bytes)
}

pub fn read(cwd: &Path, args: &Value) -> Result<Value> {
    read_with_policy(cwd, args, None)
}
pub fn read_with_policy(
    cwd: &Path,
    args: &Value,
    policy: Option<&crate::policy::Policy>,
) -> Result<Value> {
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .context("paths must be a string array")?;
    ensure!(
        !paths.is_empty() && paths.len() <= 20,
        "Read accepts 1 to 20 paths"
    );
    let offset = count(args, "offset", 1, u32::MAX as u64)?;
    let limit = count(args, "limit", 200, 2000)?;
    let mut results = Vec::new();
    let mut budget = 32_000usize;
    for path in paths {
        let name = path.as_str().context("Every path must be a string")?;
        let path = resolve(cwd, name);
        let result = (|| -> Result<Value> {
            if let Some(policy) = policy {
                policy.check(&path)?;
            }
            ensure!(
                budget >= 512,
                "Batch output budget reached; request this path separately"
            );
            if path.is_dir() {
                let mut entries = fs::read_dir(&path)?
                    .take(2001)
                    .collect::<std::io::Result<Vec<_>>>()?;
                entries.sort_by_key(|e| e.file_name());
                let mut rows = Vec::new();
                for entry in entries.iter().take(200) {
                    if policy.is_some_and(|p| p.check(&entry.path()).is_err()) {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.len() + 64 > budget {
                        break;
                    }
                    budget -= name.len() + 64;
                    rows.push(json!({"name":name,"directory":entry.file_type()?.is_dir()}));
                }
                return Ok(
                    json!({"path":path,"entries":rows,"truncated":rows.len()<entries.len()}),
                );
            }
            let bytes = read_bytes(&path)?;
            let text = std::str::from_utf8(&bytes).context("Not a UTF-8 text file")?;
            ensure!(!text.contains('\0'), "Binary file contains NUL bytes");
            let total = text.lines().count();
            ensure!(
                offset <= total.max(1),
                "offset exceeds file length ({total} lines)"
            );
            let mut lines = Vec::new();
            let mut next = offset;
            let mut line_truncated = false;
            for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                let shown: String = line.chars().take(2000).collect();
                let row = format!("{}: {}", i + 1, shown);
                if row.len() + 32 > budget {
                    break;
                }
                line_truncated |= shown.len() < line.len();
                budget -= row.len() + 32;
                lines.push(row);
                next = i + 2;
            }
            Ok(
                json!({"path":path,"sha256":hash(&bytes),"total_lines":total,
                "text":lines.join("\n"),"next_offset":if next<=total {Some(next)} else {None},
                "long_lines_truncated":line_truncated}),
            )
        })();
        results.push(match result {
            Ok(v) => v,
            Err(e) => json!({"path":path,"error":format!("{e:#}")}),
        });
    }
    Ok(json!({"files":results}))
}

pub fn write(
    cwd: &Path,
    args: &crate::operation::WriteArgs,
) -> crate::error::Result<crate::changes::Receipt> {
    use crate::{
        changes::{self, Change},
        error::Failure,
        operation::WriteMode,
    };
    let path = resolve(cwd, &args.path);
    if args.content.len() > changes::MAX_FILE {
        return Err(Failure::invalid("Content exceeds 16 MiB"));
    }
    changes::text(args.content.as_bytes())?;
    let before = changes::bytes(&path)?;
    let after = match args.mode {
        WriteMode::Create => {
            if args.expected_sha256.is_some() {
                return Err(Failure::invalid("create does not accept expected_sha256"));
            }
            if before.is_some() {
                return Err(Failure::conflict("create refuses an existing file"));
            }
            args.content.as_bytes().to_vec()
        }
        WriteMode::Replace | WriteMode::Append => {
            let old = before
                .as_ref()
                .ok_or_else(|| Failure::conflict("replace/append requires an existing file"))?;
            changes::text(old)?;
            changes::verify(
                args.expected_sha256.as_deref().ok_or_else(|| {
                    Failure::invalid("replace/append requires expected_sha256 from Read")
                })?,
                old,
            )?;
            if matches!(args.mode, WriteMode::Append) {
                let mut b = old.clone();
                b.extend_from_slice(args.content.as_bytes());
                b
            } else {
                args.content.as_bytes().to_vec()
            }
        }
    };
    if after.len() > changes::MAX_FILE {
        return Err(Failure::invalid("Result exceeds 16 MiB"));
    }
    changes::execute(
        &[Change {
            path,
            before,
            after: Some(after),
        }],
        false,
    )
}
pub fn edit(
    cwd: &Path,
    args: &crate::operation::EditArgs,
) -> crate::error::Result<crate::changes::Receipt> {
    use crate::{
        changes::{self, Change},
        error::{Code, Failure},
    };
    let path = resolve(cwd, &args.path);
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
    changes::execute(
        &[Change {
            path,
            before: Some(before),
            after: Some(after),
        }],
        false,
    )
}

pub fn search(tool: &str, cwd: &Path, args: &Value) -> Result<Value> {
    search_with_policy(tool, cwd, args, None)
}
pub fn search_with_policy(
    tool: &str,
    cwd: &Path,
    args: &Value,
    policy: Option<&crate::policy::Policy>,
) -> Result<Value> {
    let root = resolve(cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
    let pattern = text_arg(args, "pattern")?;
    ensure!(pattern.len() <= 16_384, "Pattern exceeds 16 KiB");
    let max = count(args, "limit", 200, if tool == "Glob" { 2000 } else { 1000 })?;
    let glob = if tool == "Glob" {
        Some(Glob::new(pattern)?.compile_matcher())
    } else {
        args.get("glob")
            .and_then(Value::as_str)
            .map(|s| Glob::new(s).map(|g| g.compile_matcher()))
            .transpose()?
    };
    let regex = if tool == "Grep" {
        let pattern = if flag(args, "fixed_strings")? {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(flag(args, "case_insensitive")?)
                .size_limit(2 * 1024 * 1024)
                .build()?,
        )
    } else {
        None
    };
    let files_only = flag(args, "files_only")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    if let Some(policy) = policy {
        policy.check(&root)?;
    }
    let root = fs::canonicalize(&root)?;
    let policy = policy.cloned();
    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .git_global(false)
        .filter_entry(move |entry| {
            if policy
                .as_ref()
                .is_some_and(|p| !p.allows_walk(entry.path()))
            {
                return false;
            }
            entry.depth() == 0
                || !entry.file_type().is_some_and(|t| t.is_dir())
                || !matches!(
                    entry.file_name().to_str(),
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
                )
        })
        .build();
    let mut rows = Vec::new();
    let mut skipped = 0;
    let mut budget = 24_000usize;
    let mut truncated = false;
    'walk: for (visited, entry) in walker.enumerate() {
        if visited >= 200_000 || Instant::now() > deadline {
            truncated = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let relative = if relative.as_os_str().is_empty() {
            entry.path().file_name().map(Path::new).unwrap_or(relative)
        } else {
            relative
        };
        if glob.as_ref().is_some_and(|g| !g.is_match(relative)) {
            continue;
        }
        if tool == "Glob" {
            let row = json!({"path":relative});
            let size = row.to_string().len();
            if rows.len() >= max || size > budget {
                truncated = true;
                break;
            }
            budget -= size;
            rows.push(row);
            continue;
        }
        if entry
            .metadata()
            .map(|m| m.len() > 4 * 1024 * 1024)
            .unwrap_or(true)
        {
            skipped += 1;
            continue;
        }
        let bytes = match fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if bytes.contains(&0) {
            skipped += 1;
            continue;
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        for (i, line) in text.lines().enumerate() {
            if i % 1024 == 0 && Instant::now() > deadline {
                truncated = true;
                break 'walk;
            }
            if !regex.as_ref().expect("Grep regex").is_match(line) {
                continue;
            }
            let row = if files_only {
                json!({"path":relative})
            } else {
                json!({"path":relative,"line":i+1,"text":line.chars().take(1500).collect::<String>(),"line_truncated":line.chars().count()>1500})
            };
            let size = row.to_string().len();
            if rows.len() >= max || size > budget {
                truncated = true;
                break 'walk;
            }
            budget -= size;
            rows.push(row);
            if files_only {
                break;
            }
        }
    }
    if !root.exists() {
        bail!("Search path does not exist");
    }
    Ok(json!({"root":root,"matches":rows,"truncated":truncated,"skipped":skipped}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batch_read_continuation_and_ignored_search() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "two").unwrap();
        let read = read(dir.path(), &json!({"paths":["a.txt","missing"],"limit":2})).unwrap();
        assert_eq!(read["files"][0]["next_offset"], 3);
        assert!(read["files"][1]["error"].is_string());
        let found = search("Grep", dir.path(), &json!({"pattern":"two","glob":"*.txt"})).unwrap();
        assert_eq!(found["matches"].as_array().unwrap().len(), 1);
    }
}
