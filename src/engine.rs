use crate::{
    Config,
    auth::Principal,
    changes,
    error::{Code, Failure, Result},
    files,
    jobs::{Completion, Jobs},
    operation::*,
    policy::Policy,
    results::{self, MutationReply, TaskOutput},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;
#[derive(Clone)]
pub struct Hand {
    pub config: Arc<Config>,
    pub jobs: Arc<Jobs>,
    pub audit: Arc<crate::audit::Audit>,
    pub terminals: Arc<crate::terminal::Terminals>,
}
/// Scheduling distinguishes ordinary command execution, file mutation planning, and recovery.
/// Individual file operation variants and their planners come from the operation registry.
#[derive(Clone, Serialize)]
enum Work {
    Mutation(Mutation),
    Recover(RecoverArgs),
    Shell(ShellArgs),
}
impl Work {
    fn kind(&self) -> Kind {
        match self {
            Self::Mutation(m) => m.kind(),
            Self::Recover(_) => Kind::Recover,
            Self::Shell(_) => Kind::Shell,
        }
    }
    fn options(&mut self) -> (Option<PathBuf>, Option<Uuid>, bool, u64) {
        match self {
            Self::Mutation(m) => {
                let (cwd, id, preview) = m.options();
                (cwd, id, preview, 10)
            }
            Self::Recover(a) => (a.cwd.take(), a.operation_id.take(), a.preview, 10),
            Self::Shell(a) => (
                a.cwd.take(),
                a.operation_id.take(),
                false,
                a.wait_seconds.take().map(|n| n.get()).unwrap_or(10),
            ),
        }
    }
    fn plan(
        &self,
        cwd: &Path,
        policy: &Policy,
        jobs: &Jobs,
        owner: &str,
    ) -> Result<Vec<changes::Change>> {
        match self {
            Self::Mutation(m) => m.plan(cwd, policy),
            Self::Recover(a) => {
                ended_task(jobs, owner, a.source_task_id)?;
                let RecoveryAction::Restore { versions } = &a.recovery else {
                    return Err(Failure::invalid("Only restore is a background mutation"));
                };
                crate::journal::restore_plan(
                    &jobs.recovery_root(),
                    a.source_task_id,
                    owner,
                    versions,
                    policy,
                )
            }
            Self::Shell(_) => Err(Failure::new(
                Code::Internal,
                "Command cannot produce a file plan",
                "Inspect executor routing.",
            )),
        }
    }
}
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        Failure::new(
            Code::Internal,
            e.to_string(),
            "Inspect task state before repeating mutating work.",
        )
    })?
}
fn ended_task(jobs: &Jobs, owner: &str, id: Uuid) -> Result<()> {
    let t = jobs.get_owned(owner, id)?;
    let r = t.record.lock().expect("task");
    if !r.phase.is_terminal() {
        return Err(Failure::new(
            Code::InvalidState,
            "Recovery source task is still active",
            "Wait for its confirmed outcome or cancel a queued task first.",
        ));
    }
    if matches!(r.phase, crate::jobs::Phase::Expired { .. }) {
        return Err(Failure::new(
            Code::OutputExpired,
            "Recovery source expired",
            "Use an independent backup; this task identity is retained and will not execute again.",
        ));
    }
    Ok(())
}
pub(crate) struct Context<'a> {
    hand: &'a Hand,
    principal: &'a Principal,
    policy: Policy,
}
impl Hand {
    fn cwd(&self, requested: Option<&Path>, p: &Principal) -> Result<PathBuf> {
        let cwd = requested.unwrap_or_else(|| p.roots.first().unwrap_or(&self.config.default_cwd));
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(Failure::invalid(
                "cwd must be an existing absolute directory",
            ));
        }
        Ok(std::fs::canonicalize(cwd)?)
    }
    #[cfg(test)]
    pub async fn execute(&self, name: &str, args: Value) -> Result<Value> {
        self.execute_for(&Principal::full("test"), name, args)
            .await
            .map(|r| serde_json::to_value(r).expect("typed reply"))
    }
    pub async fn execute_for(&self, p: &Principal, name: &str, args: Value) -> Result<Reply> {
        self.execute_operation(p, Operation::decode(name, args)?)
            .await
    }
    pub async fn execute_operation(&self, p: &Principal, operation: Operation) -> Result<Reply> {
        if !p.allows(operation.name()) {
            return Err(Failure::new(
                Code::PermissionDenied,
                "Operation is not permitted",
                "Use Discover for permitted operations.",
            ));
        }
        operation
            .dispatch(Context {
                hand: self,
                principal: p,
                policy: Policy::new(&self.config, p)?,
            })
            .await
    }
    async fn submit(&self, p: &Principal, mut work: Work, policy: Policy) -> Result<MutationReply> {
        let (cwd, id, preview, wait) = work.options();
        let cwd = self.cwd(cwd.as_deref(), p)?;
        if let Work::Mutation(m) = &work
            && let Some(path) = m.target()
        {
            policy.check(&files::resolve(&cwd, path))?;
        }
        let jobs = self.jobs.clone();
        let owner = p.id.clone();
        if preview {
            return blocking(move || {
                changes::execute(&work.plan(&cwd, &policy, &jobs, &owner)?, true)
            })
            .await
            .map(MutationReply::Preview);
        }
        // Reservation and worker dispatch share a non-cancellable blocking section. A disconnected
        // HTTP caller cannot leave a durable queued task with no worker to service it.
        let t = blocking(move || {
            let (task, created) = jobs.start_owned(&owner, work.kind(), &cwd, &work, id)?;
            if created {
                tokio::spawn(run_work(
                    jobs.clone(),
                    task.clone(),
                    owner,
                    work,
                    cwd,
                    policy,
                ));
            }
            Ok(task)
        })
        .await?;
        Ok(MutationReply::Task(self.jobs.report(&t, wait).await))
    }
}
impl Context<'_> {
    fn cwd(&self, cwd: Option<&Path>) -> Result<PathBuf> {
        self.hand.cwd(cwd, self.principal)
    }
    pub(crate) async fn info(self, _: Empty) -> Result<results::Info> {
        Ok(results::Info {
            server: "reach",
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            default_cwd: self
                .principal
                .roots
                .first()
                .unwrap_or(&self.hand.config.default_cwd)
                .clone(),
            principal: self.principal.id.clone(),
            scopes: self.principal.scopes.clone(),
            profile: self.principal.profile,
            stateless_http: true,
            pty: true,
            terminal_backend: crate::terminal::backend(),
        })
    }
    pub(crate) async fn read(self, a: ReadArgs) -> Result<files::ReadBatch> {
        let cwd = self.cwd(a.cwd.as_deref())?;
        blocking(move || files::read(&cwd, &a, &self.policy)).await
    }
    pub(crate) async fn glob(self, a: GlobArgs) -> Result<files::SearchResult> {
        let cwd = self.cwd(a.cwd.as_deref())?;
        blocking(move || files::search(&cwd, files::Search::Glob(&a), &self.policy)).await
    }
    pub(crate) async fn grep(self, a: GrepArgs) -> Result<files::SearchResult> {
        let cwd = self.cwd(a.cwd.as_deref())?;
        blocking(move || files::search(&cwd, files::Search::Grep(&a), &self.policy)).await
    }
    pub(crate) async fn image(self, a: ImageArgs) -> Result<crate::image::Image> {
        let cwd = self.cwd(a.cwd.as_deref())?;
        blocking(move || crate::image::load(&cwd, &a, &self.policy)).await
    }
    pub(crate) async fn mutation(self, m: Mutation) -> Result<MutationReply> {
        self.hand
            .submit(self.principal, Work::Mutation(m), self.policy)
            .await
    }
    pub(crate) async fn shell(self, a: ShellArgs) -> Result<crate::jobs::Report> {
        if a.command.trim().is_empty() {
            return Err(Failure::invalid("command must not be blank"));
        }
        let MutationReply::Task(r) = self
            .hand
            .submit(self.principal, Work::Shell(a), self.policy)
            .await?
        else {
            unreachable!("command has no preview")
        };
        Ok(r)
    }
    pub(crate) async fn recover(self, a: RecoverArgs) -> Result<results::RecoveryReply> {
        if matches!(a.recovery, RecoveryAction::Inspect) {
            ended_task(&self.hand.jobs, &self.principal.id, a.source_task_id)?;
            let jobs = self.hand.jobs.clone();
            let owner = self.principal.id.clone();
            blocking(move || {
                crate::journal::inspect(
                    &jobs.recovery_root(),
                    a.source_task_id,
                    &owner,
                    &self.policy,
                )
            })
            .await
            .map(results::RecoveryReply::Inspection)
        } else {
            self.hand
                .submit(self.principal, Work::Recover(a), self.policy)
                .await
                .map(results::RecoveryReply::Mutation)
        }
    }
    pub(crate) async fn tasks(self, a: TasksArgs) -> Result<results::TasksReply> {
        if let Some(id) = a.task_id {
            let t = self.hand.jobs.get_owned(&self.principal.id, id)?;
            Ok(results::TasksReply::Task(
                self.hand
                    .jobs
                    .report(&t, a.wait_seconds.map(|n| n.get()).unwrap_or(0))
                    .await,
            ))
        } else {
            let cwd = a.cwd.as_ref().map(|c| self.cwd(Some(c))).transpose()?;
            Ok(results::TasksReply::List(self.hand.jobs.list(
                &self.principal.id,
                cwd.as_deref(),
                a.limit.map(|n| n.get()).unwrap_or(5) as usize,
            )))
        }
    }
    pub(crate) async fn cancel(self, a: TaskId) -> Result<crate::jobs::Report> {
        let t = self.hand.jobs.get_owned(&self.principal.id, a.task_id)?;
        self.hand.jobs.cancel(&t)?;
        Ok(self.hand.jobs.report(&t, 10).await)
    }
    pub(crate) async fn poll(self, a: PollArgs) -> Result<crate::jobs::PollReport> {
        let t = self.hand.jobs.get_owned(&self.principal.id, a.task_id)?;
        self.hand
            .jobs
            .poll(
                &t,
                a.wait_seconds.map(|n| n.get()).unwrap_or(0),
                a.stdout_offset.unwrap_or(0),
                a.stderr_offset.unwrap_or(0),
                a.limit.map(|n| n.get()).unwrap_or(12000) as usize,
                a.encoding,
            )
            .await
    }
    pub(crate) async fn input(self, a: InputArgs) -> Result<results::InputReceipt> {
        let t = self.hand.jobs.get_owned(&self.principal.id, a.task_id)?;
        self.hand.jobs.input(&t, &a.text, a.eof).await?;
        Ok(results::InputReceipt {
            task_id: a.task_id,
            accepted_bytes: a.text.len(),
            stdin_closed: a.eof,
        })
    }
    pub(crate) async fn terminal_open(
        self,
        mut a: crate::terminal::OpenArgs,
    ) -> Result<crate::terminal::Record> {
        let cwd = self.cwd(a.cwd.take().as_deref())?;
        let m = self.hand.terminals.clone();
        let owner = self.principal.id.clone();
        blocking(move || m.create(&owner, &cwd, a)).await
    }
    pub(crate) async fn terminal_read(
        self,
        a: crate::terminal::ReadArgs,
    ) -> Result<crate::terminal::ReadReceipt> {
        self.hand.terminals.read(&self.principal.id, a).await
    }
    pub(crate) async fn terminal_input(
        self,
        a: crate::terminal::InputArgs,
    ) -> Result<crate::terminal::InputReceipt> {
        self.hand.terminals.input(&self.principal.id, a).await
    }
    pub(crate) async fn terminal_resize(
        self,
        a: crate::terminal::ResizeArgs,
    ) -> Result<crate::terminal::Record> {
        let m = self.hand.terminals.clone();
        let owner = self.principal.id.clone();
        blocking(move || m.resize(&owner, a)).await
    }
    pub(crate) async fn terminal_close(
        self,
        a: crate::terminal::Id,
    ) -> Result<crate::terminal::Record> {
        let m = self.hand.terminals.clone();
        let owner = self.principal.id.clone();
        blocking(move || m.close(&owner, a.terminal_id)).await
    }
    pub(crate) async fn terminals(self, _: Empty) -> Result<results::TerminalList> {
        let m = self.hand.terminals.clone();
        let owner = self.principal.id.clone();
        Ok(results::TerminalList {
            terminals: blocking(move || Ok(m.list(&owner))).await?,
        })
    }
}
async fn run_work(
    jobs: Arc<Jobs>,
    task: Arc<crate::jobs::Task>,
    owner: String,
    work: Work,
    cwd: PathBuf,
    policy: Policy,
) {
    let _permit = tokio::select! {
        permit=jobs.slots.clone().acquire_owned()=>match permit{Ok(p)=>p,Err(_)=>{jobs.finish(&task,Completion::Interrupted{error:Failure::new(Code::Interrupted,"Executor is shutting down","Inspect Tasks after reconnecting.")});return;}},
        _=task.cancel.cancelled()=>{jobs.finish(&task,Completion::Cancelled{result:None,not_started:true});return;}
    };
    let _gate = if matches!(&work, Work::Shell(_)) {
        None
    } else {
        Some(
            tokio::select! {gate=jobs.mutation_gate.lock()=>gate,_=task.cancel.cancelled()=>{jobs.finish(&task,Completion::Cancelled{result:None,not_started:true});return;}},
        )
    };
    match jobs.running(&task) {
        Ok(true) => {}
        Ok(false) => {
            jobs.finish(
                &task,
                Completion::Cancelled {
                    result: None,
                    not_started: true,
                },
            );
            return;
        }
        Err(error) => {
            jobs.finish(
                &task,
                Completion::Failed {
                    error,
                    result: None,
                },
            );
            return;
        }
    }
    let completion = if let Work::Shell(a) = work {
        crate::process::shell(jobs.clone(), task.clone(), a)
            .await
            .map_err(Failure::from)
    } else {
        let id = task.record.lock().expect("task").task_id;
        let j = jobs.clone();
        blocking(move || {
            let changes = work.plan(&cwd, &policy, &j, &owner)?;
            let committed = crate::journal::commit(&j.recovery_root(), id, &owner, &changes)?;
            let output = TaskOutput::Files(committed.receipt.clone());
            if let Some(error) = committed.recording_error {
                Ok(Completion::Unknown {
                    error,
                    result: Some(output),
                })
            } else if committed.receipt.success {
                Ok(Completion::Completed { result: output })
            } else {
                Ok(Completion::Failed {
                    error: committed.receipt.error.unwrap_or_else(|| {
                        Failure::new(
                            Code::Io,
                            "File commit failed",
                            "Inspect every file outcome and Recover before retrying.",
                        )
                    }),
                    result: Some(output),
                })
            }
        })
        .await
    };
    jobs.finish(
        &task,
        completion.unwrap_or_else(|error| Completion::Failed {
            error,
            result: None,
        }),
    );
}
