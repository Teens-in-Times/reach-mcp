use crate::auth::{Principal, Profile};
use anyhow::{Result, bail, ensure};
use rmcp::model::Tool;
use serde_json::{Value, json};

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Tool {
    serde_json::from_value(json!({
        "name": name, "description": description,
        "inputSchema": {"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"openWorldHint":name=="Shell"}
    })).expect("static tool definition")
}

pub fn tools() -> Vec<Tool> {
    let cwd = json!({"type":"string","description":"Absolute local directory for THIS call; omitted uses the configured default."});
    let retry = json!({"type":"string","format":"uuid","description":"Optional retry ID. Omit for new work; reuse returned task_id only for the identical operation."});
    vec![
        tool(
            "Read",
            "Read one or several local UTF-8 files with line numbers and SHA-256, or list a directory. Bounded output; follow next_offset when present.",
            json!({
                "paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":20},
                "cwd":cwd,"offset":{"type":"integer","minimum":1,"description":"1-based start line; default 1."},
                "limit":{"type":"integer","minimum":1,"maximum":2000,"description":"Lines per file; default 200."}
            }),
            &["paths"],
            true,
        ),
        tool(
            "Write",
            "Atomically create/replace a UTF-8 file, or append. Optional expected_sha256 rejects stale writes. Returns an automatic task ID.",
            json!({
                "path":{"type":"string"},"content":{"type":"string"},"cwd":cwd,
                "append":{"type":"boolean"},"expected_sha256":{"type":"string"},"operation_id":retry
            }),
            &["path", "content"],
            false,
        ),
        tool(
            "Edit",
            "Exact string replacement in a UTF-8 file. Match must be unique unless replace_all=true. Optional expected_sha256 rejects stale edits. Returns an automatic task ID.",
            json!({
                "path":{"type":"string"},"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},
                "replace_all":{"type":"boolean"},"expected_sha256":{"type":"string"},"cwd":cwd,"operation_id":retry
            }),
            &["path", "old_string", "new_string"],
            false,
        ),
        tool(
            "Glob",
            "Find relative file paths by glob; respects .gitignore and skips common build/cache directories. Symlinks are not traversed. Results are bounded.",
            json!({
                "pattern":{"type":"string"},"path":{"type":"string"},"cwd":cwd,
                "limit":{"type":"integer","minimum":1,"maximum":2000}
            }),
            &["pattern"],
            true,
        ),
        tool(
            "Grep",
            "Search UTF-8 files by regex or literal. Respects ignore files. Returns bounded line matches or file names; narrow path/glob when truncated.",
            json!({
                "pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"cwd":cwd,
                "fixed_strings":{"type":"boolean"},"case_insensitive":{"type":"boolean"},
                "files_only":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":1000}
            }),
            &["pattern"],
            true,
        ),
        tool(
            "Shell",
            "Run local Windows PowerShell without a profile. Fresh shell per call; cd does not persist. Tasks keep running when MCP waiting ends. Use Tasks to inspect/cancel; do not blindly rerun an unknown command.",
            json!({
                "command":{"type":"string","minLength":1},"cwd":cwd,"operation_id":retry,"keep_stdin":{"type":"boolean"},
                "wait_seconds":{"type":"integer","minimum":0,"maximum":30,"description":"Wait for result, default 10; zero returns immediately."},
                "timeout_seconds":{"type":"integer","minimum":1,"maximum":86400,"description":"Process deadline, default 600; independent of MCP waiting."}
            }),
            &["command"],
            false,
        ),
        tool(
            "Tasks",
            "List recent tasks, recover a lost result, or wait for a task. Uses the returned task_id; never repeats the original action. A known task ID needs no cwd.",
            json!({
                "action":{"type":"string","enum":["list","get"],"description":"Default list; task_id implies get."},
                "task_id":{"type":"string","format":"uuid"},"cwd":cwd,
                "limit":{"type":"integer","minimum":1,"maximum":50},
                "wait_seconds":{"type":"integer","minimum":0,"maximum":30}
            }),
            &[],
            true,
        ),
        tool(
            "Cancel",
            "Cancel a Shell task and its process tree; returns the confirmed task state. File mutations are atomic and cannot be cancelled mid-write.",
            json!({
                "task_id":{"type":"string","format":"uuid"}
            }),
            &["task_id"],
            false,
        ),
        tool(
            "ApplyPatch",
            "Apply a contextual multi-file *** Begin Patch patch. Validate all files before writes; each replacement is atomic. Supports add/update/delete/move and preserves CRLF.",
            json!({"patch":{"type":"string","minLength":1},"cwd":cwd,"expected_sha256":{"type":"object","additionalProperties":{"type":"string"}},"operation_id":retry}),
            &["patch"],
            false,
        ),
        tool(
            "Poll",
            "Wait and read bounded incremental process output using byte offsets. Does not repeat execution.",
            json!({"task_id":{"type":"string","format":"uuid"},"wait_seconds":{"type":"integer","minimum":0,"maximum":30},"stdout_offset":{"type":"integer","minimum":0},"stderr_offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":4,"maximum":12000}}),
            &["task_id"],
            true,
        ),
        tool(
            "Input",
            "Write UTF-8 stdin to an owned Shell task created with keep_stdin=true. eof=true closes stdin. Pipe input; no terminal/PTY emulation.",
            json!({"task_id":{"type":"string","format":"uuid"},"text":{"type":"string"},"eof":{"type":"boolean"}}),
            &["task_id", "text"],
            false,
        ),
        tool(
            "Image",
            "Return a local PNG/JPEG/GIF/WebP image as native MCP image content, at most 8 MiB.",
            json!({"path":{"type":"string"},"cwd":cwd}),
            &["path"],
            true,
        ),
        tool(
            "Info",
            "Report local runtime, default cwd, effective identity, scopes and tool profile.",
            json!({}),
            &[],
            true,
        ),
    ]
}

pub fn compact() -> Vec<Tool> {
    vec![
        tool(
            "Run",
            "Execute one operation from the initialization guide. arguments is that operation's JSON object. Inspect tool and arguments for approval.",
            json!({"tool":{"type":"string"},"arguments":{"type":"object"}}),
            &["tool", "arguments"],
            false,
        ),
        tool(
            "Discover",
            "Get the exact schema for one permitted operation. Omit tool for a short catalog; do not rediscover on every call.",
            json!({"tool":{"type":"string"}}),
            &[],
            true,
        ),
    ]
}
pub fn exposed(p: &Principal) -> Vec<Tool> {
    if p.profile == Profile::Compact {
        compact()
    } else {
        tools().into_iter().filter(|t| p.allows(&t.name)).collect()
    }
}
pub fn guide(p: &Principal, cwd: &std::path::Path) -> String {
    let mut out = format!(
        "REACH {} — the user's local machine, OS={}, default cwd={}. Authenticated identity={}.\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        cwd.display(),
        p.id
    );
    out.push_str("The calling MCP host owns its approval policy; accepted calls execute directly. cwd is per-call and never changes another task. File tools are restricted to granted workspaces and exclude protected executor data. Shell has the current OS account's full execution capability; workspace roots are not an OS sandbox. Shell uses PowerShell on Windows, bash elsewhere; shell cd does not persist.\n");
    if p.profile == Profile::Compact {
        out.push_str("Only Run and Discover are registered model tools. Call Run({tool:\"Read\",arguments:{paths:[\"src/main.rs\"]}}), etc. The operation signatures below are the initial guide; Discover({tool:\"ApplyPatch\"}) supplies an exact schema when needed. Do not request the full catalog for each action.\n");
    }
    out.push_str("Signatures (? optional):\n");
    let signatures = [
        (
            "Read",
            "Read(paths:string[],cwd?,offset?,limit?) -> numbered UTF-8 text, sha256, next_offset; directories list entries. Default offset=1, limit=200.",
        ),
        (
            "Write",
            "Write(path,content,cwd?,append?,expected_sha256?,operation_id?)",
        ),
        (
            "Edit",
            "Edit(path,old_string,new_string,cwd?,replace_all?,expected_sha256?,operation_id?) -> unique exact replacement.",
        ),
        (
            "ApplyPatch",
            "ApplyPatch(patch,cwd?,expected_sha256?:{relative_path:sha256},operation_id?) -> *** Begin Patch with Add File/Update File/Delete File, @@ context hunks, optional Move to. Relative project paths. All files validated first; multi-file commits are not crash-atomic.",
        ),
        (
            "Glob",
            "Glob(pattern,path?,cwd?,limit?) -> ignored-aware file paths.",
        ),
        (
            "Grep",
            "Grep(pattern,path?,glob?,cwd?,fixed_strings?,case_insensitive?,files_only?,limit?) -> bounded matches.",
        ),
        (
            "Shell",
            "Shell(command,cwd?,wait_seconds?,timeout_seconds?,keep_stdin?,operation_id?) -> automatic task_id; wait defaults 10s (max30), timeout defaults600s. keep_stdin=false; true permits pipe Input, not a PTY.",
        ),
        (
            "Poll",
            "Poll(task_id,wait_seconds?,stdout_offset?,stderr_offset?,limit?) -> incremental stdout/stderr and next_offset (bytes). Offsets default0; limit defaults12000 per stream.",
        ),
        (
            "Input",
            "Input(task_id,text,eof?) -> send text; eof closes stdin.",
        ),
        (
            "Tasks",
            "Tasks(task_id?,action?:list|get,cwd?,limit?,wait_seconds?) -> recover recent results. Only your identity's tasks.",
        ),
        (
            "Cancel",
            "Cancel(task_id) -> stop an owned Shell process tree.",
        ),
        ("Image", "Image(path,cwd?) -> native image content."),
        ("Info", "Info() -> runtime and effective permissions."),
    ];
    for (name, signature) in signatures {
        if p.allows(name) {
            out.push_str(signature);
            out.push('\n');
        }
    }
    out.push_str("Never repeat a command because waiting ended or HTTP disconnected. Poll/Tasks using returned task_id. New writes/commands automatically get IDs; operation_id is only for an explicit identical retry, never required for new work. Read before editing; expected_sha256 rejects stale content. Bounded results provide continuation. Tasks survive connection loss; after executor restart unfinished work is interrupted and never replayed. A credential identifies a client; two TUIs sharing one credential share task visibility. No per-call conversation ID required.\n");
    out
}

/// Validates the bounded JSON-schema subset used by this catalog, on the server.
pub fn validate(name: &str, args: &Value) -> Result<()> {
    let schema = tools()
        .into_iter()
        .chain(compact())
        .find(|t| t.name == name)
        .context_tool()?;
    validate_value(
        args,
        &Value::Object((*schema.input_schema).clone()),
        "arguments",
    )
}
trait ToolOption {
    fn context_tool(self) -> Result<Tool>;
}
impl ToolOption for Option<Tool> {
    fn context_tool(self) -> Result<Tool> {
        self.ok_or_else(|| anyhow::anyhow!("Unknown operation"))
    }
}
fn validate_value(value: &Value, schema: &Value, path: &str) -> Result<()> {
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        ensure!(
            match kind {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                _ => false,
            },
            "{path} must be {kind}"
        );
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        ensure!(values.contains(value), "{path} is not an allowed value");
    }
    if let Some(number) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
            ensure!(number >= min, "{path} is below minimum");
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
            ensure!(number <= max, "{path} exceeds maximum");
        }
    }
    if let Some(text) = value.as_str() {
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
            ensure!(text.chars().count() as u64 >= min, "{path} is too short");
        }
        if schema.get("format").and_then(Value::as_str) == Some("uuid") {
            uuid::Uuid::parse_str(text).map_err(|_| anyhow::anyhow!("{path} must be a UUID"))?;
        }
    }
    if let Some(array) = value.as_array() {
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
            ensure!(array.len() as u64 >= min, "{path} has too few items");
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
            ensure!(array.len() as u64 <= max, "{path} has too many items");
        }
        if let Some(item) = schema.get("items") {
            for (i, v) in array.iter().enumerate() {
                validate_value(v, item, &format!("{path}[{i}]"))?;
            }
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                ensure!(
                    object.contains_key(key.as_str().unwrap_or_default()),
                    "{path} missing required field {key}"
                );
            }
        }
        for (key, v) in object {
            if let Some(prop) = schema.get("properties").and_then(|p| p.get(key)) {
                validate_value(v, prop, &format!("{path}.{key}"))?;
            } else {
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => bail!("Unknown field {path}.{key}"),
                    Some(s) if s.is_object() => validate_value(v, s, &format!("{path}.{key}"))?,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
