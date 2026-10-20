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
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct Sequence<T, const MIN: usize, const MAX: usize>(Vec<T>);
impl<T, const MIN: usize, const MAX: usize> std::ops::Deref for Sequence<T, MIN, MAX> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}
impl<'de, T: Deserialize<'de>, const MIN: usize, const MAX: usize> Deserialize<'de>
    for Sequence<T, MIN, MAX>
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let items = Vec::<T>::deserialize(d)?;
        if !(MIN..=MAX).contains(&items.len()) {
            return Err(serde::de::Error::custom(format!(
                "expected {MIN}..={MAX} items"
            )));
        }
        Ok(Self(items))
    }
}
impl<T: JsonSchema, const MIN: usize, const MAX: usize> JsonSchema for Sequence<T, MIN, MAX> {
    fn schema_name() -> Cow<'static, str> {
        format!("Sequence_{}_{MIN}_{MAX}", T::schema_name()).into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut s = Vec::<T>::json_schema(generator);
        s.insert("minItems".into(), json!(MIN));
        s.insert("maxItems".into(), json!(MAX));
        s
    }
}
macro_rules! args {
    ($name:ident { $($fields:tt)* }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)] pub struct $name { $($fields)* }
    };
}
args!(ReadArgs {
    pub paths: Sequence<String,1,20>, pub cwd: Option<PathBuf>,
    pub offset: Option<Number<1, 4294967295>>, pub limit: Option<Number<1, 2000>>,
});
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum WriteContent {
    Create {
        content: String,
    },
    Replace {
        content: String,
        expected_sha256: String,
    },
    Append {
        content: String,
        expected_sha256: String,
    },
}
args!(WriteArgs {
    pub path: String,
    /// Create refuses existing paths; replace and append require a current SHA-256.
    pub update:WriteContent,
    #[serde(default)]pub preview:bool,
    pub cwd: Option<PathBuf>, pub operation_id: Option<Uuid>,
});
args!(EditArgs {
    pub path: String, pub old_string: String, pub new_string: String, pub expected_sha256: String,
    #[serde(default)] pub replace_all: bool,
    #[serde(default)]pub preview:bool,
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
    #[serde(default)] pub encoding:crate::jobs::OutputEncoding,
});
args!(InputArgs { pub task_id: Uuid, pub text: String, #[serde(default)] pub eof: bool });
args!(ImageArgs { pub path: String, pub cwd: Option<PathBuf> });
args!(Empty {});
args!(RunArgs { pub tool: String, pub arguments: serde_json::Map<String, Value> });
args!(DiscoverArgs { pub tool: Option<String> });
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryAction {
    #[default]
    Inspect,
    Restore {
        versions: BTreeMap<PathBuf, crate::journal::Version>,
    },
}
args!(RecoverArgs {
    pub source_task_id:Uuid,
    #[serde(default)]pub recovery:RecoveryAction,
    #[serde(default)]pub preview:bool,
    pub cwd:Option<PathBuf>,pub operation_id:Option<Uuid>,
});

macro_rules! path_hint {
    ($args:ident,$field:ident) => {
        Some($args.$field.as_str())
    };
    ($args:ident) => {
        None
    };
}
macro_rules! operations {
    (direct {$( $name:ident($args:ty)->$output:ty => $handler:ident, $scope:literal, $read_only:literal, $description:literal $(,error=$failure:path)?; )+}
     mutations {$( $mutation:ident($margs:ty) => $planner:path, $mdescription:literal $(,path=$field:ident)?; )+})=>{
        #[derive(Debug,Clone,Serialize)]
        #[serde(tag="operation",content="arguments")]pub enum Operation {$($name($args),)+$($mutation($margs)),+}
        #[derive(Debug,Clone,Copy,Serialize,Deserialize,PartialEq,Eq)]pub enum Kind {$($name,)+$($mutation),+}
        impl Kind {pub fn name(self)->&'static str {match self {$(Self::$name=>stringify!($name),)+$(Self::$mutation=>stringify!($mutation)),+}}}
        #[derive(Serialize)]#[serde(untagged)]pub enum Reply {$($name($output),)+$($mutation(crate::results::MutationReply)),+}
        #[derive(Debug,Clone,Serialize)]pub(crate) enum Mutation {$($mutation($margs)),+}
        impl Mutation {
            pub fn kind(&self)->Kind {match self {$(Self::$mutation(_)=>Kind::$mutation),+}}
            pub fn options(&mut self)->(Option<PathBuf>,Option<Uuid>,bool) {match self {$(Self::$mutation(a)=>(a.cwd.take(),a.operation_id.take(),a.preview)),+}}
            pub fn target(&self)->Option<&str> {match self {$(Self::$mutation(a)=>{let _=a;path_hint!(a $(,$field)?) }),+}}
            pub fn plan(&self,cwd:&std::path::Path,policy:&crate::policy::Policy)->Result<Vec<crate::changes::Change>> {match self {$(Self::$mutation(a)=>$planner(cwd,a,policy)),+}}
        }
        impl Operation {
            pub fn decode(name:&str,args:Value)->Result<Self> {match name {
                $(stringify!($name)=>serde_json::from_value(args).map(Self::$name).map_err(|e|Failure::invalid(e.to_string())),)+
                $(stringify!($mutation)=>serde_json::from_value(args).map(Self::$mutation).map_err(|e|Failure::invalid(e.to_string())),)+
                _=>Err(Failure::invalid("Unknown operation")),
            }}
            pub fn kind(&self)->Kind {match self {$(Self::$name(_)=>Kind::$name,)+$(Self::$mutation(_)=>Kind::$mutation),+}}
            pub fn name(&self)->&'static str {self.kind().name()}
            pub fn scope(name:&str)->Option<&'static str> {match name {$(stringify!($name)=>Some($scope),)+$(stringify!($mutation)=>Some("write"),)+_=>None}}
            pub fn definitions()->Vec<rmcp::model::Tool> {vec![
                $(crate::catalog::typed_tool::<$args>(stringify!($name),$description,$read_only,$scope=="exec"),)+
                $(crate::catalog::typed_tool::<$margs>(stringify!($mutation),$mdescription,false,false)),+
            ]}
            pub(crate) async fn dispatch(self,ctx:crate::engine::Context<'_>)->Result<Reply> {match self {
                $(Self::$name(a)=>ctx.$handler(a).await.map(Reply::$name),)+
                $(Self::$mutation(a)=>ctx.mutation(Mutation::$mutation(a)).await.map(Reply::$mutation)),+
            }}
        }
        impl Reply {
            pub fn execution_failed(&self)->bool {match self {
                $(Self::$name(r)=>{let _=r;false $(||$failure(r))?},)+
                $(Self::$mutation(r)=>r.failed()),+
            }}
            pub fn image(&self)->Option<&crate::image::Image> {if let Self::Image(i)=self{Some(i)}else{None}}
        }
    }
}
operations! {
    direct {
        Read(ReadArgs)->crate::files::ReadBatch => read, "read", true, "Read bounded numbered UTF-8 text and SHA-256 versions, or list directories.";
        Glob(GlobArgs)->crate::files::SearchResult => glob, "read", true, "Find bounded relative paths, respecting ignore files and workspace policy.";
        Grep(GrepArgs)->crate::files::SearchResult => grep, "read", true, "Search UTF-8 files by regex or literal, respecting ignore files and workspace policy.";
        Shell(ShellArgs)->crate::jobs::Report => shell, "exec", false, "Run one fresh shell with separate stdout/stderr. Waiting never repeats execution.",error=crate::results::task_failed;
        Tasks(TasksArgs)->crate::results::TasksReply => tasks, "tasks", true, "Get or wait for an owned durable task, or list recent tasks. Interrupted work is never replayed.";
        Cancel(TaskId)->crate::jobs::Report => cancel, "tasks", false, "Cancel an owned command process tree or queued task; wait for its confirmed state.";
        Poll(PollArgs)->crate::jobs::PollReport => poll, "tasks", true, "Read bounded stdout/stderr by byte cursor without repeating execution.";
        Input(InputArgs)->crate::results::InputReceipt => input, "exec", false, "Write bounded pipe stdin to a command created with keep_stdin. Distinct from a terminal.";
        Image(ImageArgs)->crate::image::Image => image, "read", true, "Read a local PNG, JPEG, GIF or WebP as native MCP image content, up to 8 MiB.";
        Info(Empty)->crate::results::Info => info, "", true, "Report runtime and effective permissions.";
        TerminalOpen(crate::terminal::OpenArgs)->crate::terminal::Record => terminal_open, "exec", false, "Open an owned persistent PTY with real terminal semantics. Returns terminal_id. cwd and environment persist inside this shell; optional operation_id deduplicates lost submissions.",error=crate::results::terminal_failed;
        TerminalRead(crate::terminal::ReadArgs)->crate::terminal::ReadReceipt => terminal_read, "exec", true, "Reconnect/read an owned PTY. Default is a VT screen snapshot; since_screen suppresses unchanged screens. Stream/both returns raw base64 with byte cursors and explicit gaps.";
        TerminalInput(crate::terminal::InputArgs)->crate::terminal::InputReceipt => terminal_input, "exec", false, "Send text or control keys to a PTY. Optional input_id prevents duplicate input. Queued or unknown delivery must be inspected, not blindly repeated.";
        TerminalResize(crate::terminal::ResizeArgs)->crate::terminal::Record => terminal_resize, "exec", false, "Resize the actual PTY and its screen model; applications receive the OS terminal resize event.";
        TerminalClose(crate::terminal::Id)->crate::terminal::Record => terminal_close, "exec", false, "Close an owned PTY and its process tree. Closing is distinct from confirmed process exit; read until closed.";
        Terminals(Empty)->crate::results::TerminalList => terminals, "exec", true, "Recover owned terminal IDs and lifecycle metadata. Never reruns commands or recreates a terminal.";
        Recover(RecoverArgs)->crate::results::RecoveryReply => recover, "write", false, "Inspect an owned ended file task's durable before-images, or explicitly restore them with current per-path versions. Restoration is a new guarded task; interrupted work is never replayed.",error=crate::results::recovery_failed;
    }
    mutations {
        Write(WriteArgs) => crate::files::plan_write, "Create, replace or append UTF-8 content. Existing files require a current version; returns a durable task.",path=path;
        Edit(EditArgs) => crate::files::plan_edit, "Replace exact text with a mandatory version guard. Ambiguous matches require more context or replace_all.",path=path;
        ApplyPatch(PatchArgs) => crate::patch::plan, "Preview or apply a contextual patch. Per-file version guards, summaries and rollback outcomes; no multi-file atomicity.";
    }
}
