//! One registry owns operation names, scopes, descriptions and argument types.
use crate::error::{Failure, Result};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

/// Schema and decoder enforce exactly the same bounds.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
pub struct Number<const MIN: u64, const MAX: u64>(u64);
impl<const MIN: u64, const MAX: u64> Number<MIN, MAX> {
    pub fn get(self) -> u64 {
        self.0
    }
}
impl<'de, const MIN: u64, const MAX: u64> Deserialize<'de> for Number<MIN, MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let n = u64::deserialize(d)?;
        if !(MIN..=MAX).contains(&n) {
            return Err(serde::de::Error::custom(format!(
                "expected integer in {MIN}..={MAX}"
            )));
        }
        Ok(Self(n))
    }
}
impl<const MIN: u64, const MAX: u64> JsonSchema for Number<MIN, MAX> {
    fn schema_name() -> Cow<'static, str> {
        format!("Integer_{MIN}_{MAX}").into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json!({"type":"integer","minimum":MIN,"maximum":MAX})
            .try_into()
            .unwrap()
    }
}
pub type Wait = Number<0, 30>;
pub type Limit = Number<4, 12000>;
macro_rules! args {
    ($name:ident { $($fields:tt)* }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)] pub struct $name { $($fields)* }
    };
}
args!(ReadArgs {
    pub paths: Vec<String>, pub cwd: Option<PathBuf>,
    pub offset: Option<Number<1, 4294967295>>, pub limit: Option<Number<1, 2000>>,
});
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Create,
    Replace,
    Append,
}
args!(WriteArgs {
    pub path: String, pub content: String,
    /// Create refuses existing paths; replace and append require expected_sha256.
    pub mode: WriteMode, pub expected_sha256: Option<String>,
    pub cwd: Option<PathBuf>, pub operation_id: Option<Uuid>,
});
args!(EditArgs {
    pub path: String, pub old_string: String, pub new_string: String, pub expected_sha256: String,
    #[serde(default)] pub replace_all: bool,
    pub cwd: Option<PathBuf>, pub operation_id: Option<Uuid>,
});
args!(GlobArgs {
    pub pattern: String, pub path: Option<String>, pub cwd: Option<PathBuf>, pub limit: Option<Number<1, 2000>>,
});
args!(GrepArgs {
    pub pattern: String, pub path: Option<String>, pub glob: Option<String>, pub cwd: Option<PathBuf>,
    #[serde(default)] pub fixed_strings: bool,
    #[serde(default)] pub case_insensitive: bool,
    #[serde(default)] pub files_only: bool,
    pub limit: Option<Number<1, 1000>>,
});
args!(ShellArgs {
    pub command: String, pub cwd: Option<PathBuf>, pub operation_id: Option<Uuid>,
    #[serde(default)] pub keep_stdin: bool,
    pub wait_seconds: Option<Wait>, pub timeout_seconds: Option<Number<1, 86400>>,
});
args!(TasksArgs {
    pub task_id: Option<Uuid>, pub cwd: Option<PathBuf>, pub limit: Option<Number<1, 50>>, pub wait_seconds: Option<Wait>,
});
args!(TaskId { pub task_id: Uuid });
args!(PatchArgs {
    pub patch: String,
    /// Required for every existing source file. New paths must not exist.
    #[serde(default)] pub expected_sha256: BTreeMap<String, String>,
    #[serde(default)] pub preview: bool,
    pub cwd: Option<PathBuf>, pub operation_id: Option<Uuid>,
});
args!(PollArgs {
    pub task_id: Uuid, pub wait_seconds: Option<Wait>, pub stdout_offset: Option<u64>, pub stderr_offset: Option<u64>, pub limit: Option<Limit>,
});
args!(InputArgs { pub task_id: Uuid, pub text: String, #[serde(default)] pub eof: bool });
args!(ImageArgs { pub path: String, pub cwd: Option<PathBuf> });
args!(Empty {});
args!(RunArgs { pub tool: String, pub arguments: Value });
args!(DiscoverArgs { pub tool: Option<String> });

macro_rules! operations {
    ($($name:ident($args:ty), $scope:literal, $read_only:literal, $description:literal;)+) => {
        #[derive(Debug, Clone, Serialize)]
        #[serde(tag = "operation", content = "arguments")]
        pub enum Operation { $($name($args)),+ }
        impl Operation {
            pub fn decode(name: &str, args: Value) -> Result<Self> {
                match name { $(stringify!($name) => serde_json::from_value(args).map(Self::$name).map_err(|e| Failure::invalid(e.to_string())),)+
                    _ => Err(Failure::invalid("Unknown operation")) }
            }
            pub fn name(&self) -> &'static str { match self { $(Self::$name(_) => stringify!($name)),+ } }
            pub fn scope(name: &str) -> Option<&'static str> { match name { $(stringify!($name) => Some($scope),)+ _ => None } }
            pub fn definitions() -> Vec<rmcp::model::Tool> {
                vec![$(crate::catalog::typed_tool::<$args>(stringify!($name), $description, $read_only, $scope == "exec")),+]
            }
        }
    }
}
operations! {
    Read(ReadArgs), "read", true, "Read bounded numbered UTF-8 text and SHA-256 versions, or list directories.";
    Write(WriteArgs), "write", false, "Create, replace or append UTF-8 content. Existing files require a current version; returns a durable task.";
    Edit(EditArgs), "write", false, "Replace exact text with a mandatory version guard. Ambiguous matches require more context or replace_all.";
    Glob(GlobArgs), "read", true, "Find bounded relative paths, respecting ignore files and workspace policy.";
    Grep(GrepArgs), "read", true, "Search UTF-8 files by regex or literal, respecting ignore files and workspace policy.";
    Shell(ShellArgs), "exec", false, "Run one fresh shell with separate stdout/stderr. Waiting never repeats execution.";
    Tasks(TasksArgs), "tasks", true, "Get or wait for an owned durable task, or list recent tasks. Interrupted work is never replayed.";
    Cancel(TaskId), "tasks", false, "Cancel an owned command process tree or queued task; wait for its confirmed state.";
    ApplyPatch(PatchArgs), "write", false, "Preview or apply a contextual patch. Per-file version guards, summaries and rollback outcomes; no multi-file atomicity.";
    Poll(PollArgs), "tasks", true, "Read bounded stdout/stderr by byte cursor without repeating execution.";
    Input(InputArgs), "exec", false, "Write bounded pipe stdin to a command created with keep_stdin. Distinct from a terminal.";
    Image(ImageArgs), "read", true, "Read a local PNG, JPEG, GIF or WebP as native MCP image content, up to 8 MiB.";
    Info(Empty), "", true, "Report runtime and effective permissions.";
    TerminalOpen(crate::terminal::OpenArgs), "exec", false, "Open an owned persistent PTY with real terminal semantics. Returns terminal_id. cwd and environment persist inside this shell; optional operation_id deduplicates lost submissions.";
    TerminalRead(crate::terminal::ReadArgs), "exec", true, "Reconnect/read an owned PTY. Default is a VT screen snapshot; since_screen suppresses unchanged screens. Stream/both returns raw base64 with byte cursors and explicit gaps.";
    TerminalInput(crate::terminal::InputArgs), "exec", false, "Send text or control keys to a PTY. Optional input_id prevents duplicate input. Queued or unknown delivery must be inspected, not blindly repeated.";
    TerminalResize(crate::terminal::ResizeArgs), "exec", false, "Resize the actual PTY and its screen model; applications receive the OS terminal resize event.";
    TerminalClose(crate::terminal::Id), "exec", false, "Close an owned PTY and its process tree. Closing is distinct from confirmed process exit; read until closed.";
    Terminals(Empty), "exec", true, "Recover owned terminal IDs and lifecycle metadata. Never reruns commands or recreates a terminal.";
}
