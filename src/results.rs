//! Typed wire results. JSON conversion happens at the transport boundary.
use crate::{
    auth::Profile,
    changes::Receipt,
    jobs::{List, Report},
    terminal,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_stored: u64,
    pub stderr_stored: u64,
    pub stdout_next_offset: u64,
    pub stderr_next_offset: u64,
    pub stdout_encoding_errors: bool,
    pub stderr_encoding_errors: bool,
    pub output_truncated: bool,
    pub logs_truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskOutput {
    Command(CommandOutput),
    Files(Receipt),
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum MutationReply {
    Preview(Receipt),
    Task(Report),
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TasksReply {
    Task(Report),
    List(List),
}
#[derive(Debug, Serialize)]
pub struct InputReceipt {
    pub task_id: uuid::Uuid,
    pub accepted_bytes: usize,
    pub stdin_closed: bool,
}
#[derive(Serialize)]
pub struct Info {
    pub server: &'static str,
    pub version: &'static str,
    pub os: &'static str,
    pub default_cwd: PathBuf,
    pub principal: String,
    pub scopes: Vec<String>,
    pub profile: Profile,
    pub stateless_http: bool,
    pub pty: bool,
    pub terminal_backend: terminal::Backend,
}
#[derive(Debug, Serialize)]
pub struct TerminalList {
    pub terminals: Vec<terminal::Record>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RecoveryReply {
    Inspection(crate::journal::Inspection),
    Mutation(MutationReply),
}
impl MutationReply {
    pub fn failed(&self) -> bool {
        match self {
            Self::Preview(r) => !r.success,
            Self::Task(r) => r.phase.is_failure(),
        }
    }
}
pub fn task_failed(r: &Report) -> bool {
    r.phase.is_failure()
}
pub fn terminal_failed(r: &terminal::Record) -> bool {
    r.state == terminal::State::Interrupted
}
pub fn recovery_failed(r: &RecoveryReply) -> bool {
    match r {
        RecoveryReply::Inspection(_) => false,
        RecoveryReply::Mutation(r) => r.failed(),
    }
}
