use crate::{
    Config,
    auth::Principal,
    error::{Code, Failure, Result},
    files,
    jobs::{Jobs, State},
    operation::*,
    policy::Policy,
};
use serde::Serialize;
use serde_json::{Value, json};
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

#[derive(Serialize)]
enum Work {
    Write(WriteArgs),
    Edit(EditArgs),
    Patch(PatchArgs),
    Shell(ShellArgs),
}
impl Work {
    fn name(&self) -> &'static str {
        match self {
            Self::Write(_) => "Write",
            Self::Edit(_) => "Edit",
            Self::Patch(_) => "ApplyPatch",
            Self::Shell(_) => "Shell",
        }
    }
}
// Temporary adapter for the bounded read/search/image implementation. All dispatch is typed.
fn arguments(args: impl Serialize) -> Value {
    let mut v = serde_json::to_value(args).expect("serializable arguments");
    if let Some(m) = v.as_object_mut() {
        m.retain(|_, v| !v.is_null());
    }
    v
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
        self.execute_for(&Principal::full("test"), name, args).await
    }
    pub async fn execute_for(&self, p: &Principal, name: &str, args: Value) -> Result<Value> {
        if !p.allows(name) {
            return Err(Failure::new(
                Code::PermissionDenied,
                "Operation is not permitted",
                "Use Discover for permitted operations.",
            ));
        }
        self.execute_operation(p, Operation::decode(name, args)?)
            .await
    }
    pub async fn execute_operation(&self, p: &Principal, operation: Operation) -> Result<Value> {
        if !p.allows(operation.name()) {
            return Err(Failure::new(
                Code::PermissionDenied,
                "Operation is not permitted",
                "Use Discover for permitted operations.",
            ));
        }
        let policy = Policy::new(&self.config, p).map_err(Failure::from)?;
        match operation {
            Operation::Info(_) => Ok(
                json!({"server":"reach","version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"default_cwd":p.roots.first().unwrap_or(&self.config.default_cwd),"principal":p.id,"scopes":p.scopes,"profile":p.profile,"stateless_http":true,"pty":true}),
            ),
            Operation::TerminalOpen(mut a) => {
                let cwd = self.cwd(a.cwd.take().as_deref(), p)?;
                let terminals = self.terminals.clone();
                let owner = p.id.clone();
                tokio::task::spawn_blocking(move || terminals.create(&owner, &cwd, a))
                    .await
                    .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                    .map(|r| json!(r))
            }
            Operation::TerminalRead(a) => self.terminals.read(&p.id, a).await.map(|r| json!(r)),
            Operation::TerminalInput(a) => self.terminals.input(&p.id, a).await.map(|r| json!(r)),
            Operation::TerminalResize(a) => {
                let terminals = self.terminals.clone();
                let owner = p.id.clone();
                tokio::task::spawn_blocking(move || terminals.resize(&owner, a))
                    .await
                    .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                    .map(|r| json!(r))
            }
            Operation::TerminalClose(a) => {
                let terminals = self.terminals.clone();
                let owner = p.id.clone();
                tokio::task::spawn_blocking(move || terminals.close(&owner, a.terminal_id))
                    .await
                    .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                    .map(|r| json!(r))
            }
            Operation::Terminals(_) => Ok(json!({"terminals":self.terminals.list(&p.id)})),
            Operation::Read(a) => {
                if a.paths.is_empty() || a.paths.len() > 20 {
                    return Err(Failure::invalid("Read accepts 1..20 paths"));
                }
                let cwd = self.cwd(a.cwd.as_deref(), p)?;
                tokio::task::spawn_blocking(move || {
                    files::read_with_policy(&cwd, &arguments(a), Some(&policy))
                })
                .await
                .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                .map_err(Failure::from)
            }
            Operation::Glob(a) => {
                let cwd = self.cwd(a.cwd.as_deref(), p)?;
                tokio::task::spawn_blocking(move || {
                    files::search_with_policy("Glob", &cwd, &arguments(a), Some(&policy))
                })
                .await
                .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                .map_err(Failure::from)
            }
            Operation::Grep(a) => {
                let cwd = self.cwd(a.cwd.as_deref(), p)?;
                tokio::task::spawn_blocking(move || {
                    files::search_with_policy("Grep", &cwd, &arguments(a), Some(&policy))
                })
                .await
                .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                .map_err(Failure::from)
            }
            Operation::Image(a) => {
                let cwd = self.cwd(a.cwd.as_deref(), p)?;
                check(&policy, &files::resolve(&cwd, &a.path))?;
                tokio::task::spawn_blocking(move || crate::image::load(&cwd, &arguments(a)))
                    .await
                    .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                    .map_err(Failure::from)
            }
            Operation::Write(mut a) => {
                match a.mode {
                    WriteMode::Create if a.expected_sha256.is_some() => {
                        return Err(Failure::invalid("create does not accept expected_sha256"));
                    }
                    WriteMode::Replace | WriteMode::Append if a.expected_sha256.is_none() => {
                        return Err(Failure::invalid("replace/append requires expected_sha256"));
                    }
                    _ => {}
                }
                let cwd = self.cwd(a.cwd.take().as_deref(), p)?;
                check(&policy, &files::resolve(&cwd, &a.path))?;
                let id = a.operation_id.take();
                self.submit(p, Work::Write(a), cwd, policy, id, 10).await
            }
            Operation::Edit(mut a) => {
                if a.old_string.is_empty() {
                    return Err(Failure::invalid("old_string must not be empty"));
                }
                let cwd = self.cwd(a.cwd.take().as_deref(), p)?;
                check(&policy, &files::resolve(&cwd, &a.path))?;
                let id = a.operation_id.take();
                self.submit(p, Work::Edit(a), cwd, policy, id, 10).await
            }
            Operation::ApplyPatch(mut a) => {
                let cwd = self.cwd(a.cwd.take().as_deref(), p)?;
                if a.preview {
                    return tokio::task::spawn_blocking(move || {
                        crate::patch::apply_with_policy(&cwd, &a, Some(&policy))
                    })
                    .await
                    .map_err(|e| Failure::from(anyhow::Error::from(e)))?
                    .map(|r| json!(r))
                    .map_err(Failure::from);
                }
                let id = a.operation_id.take();
                self.submit(p, Work::Patch(a), cwd, policy, id, 10).await
            }
            Operation::Shell(mut a) => {
                if a.command.trim().is_empty() {
                    return Err(Failure::invalid("command must not be empty"));
                }
                let cwd = self.cwd(a.cwd.take().as_deref(), p)?;
                let id = a.operation_id.take();
                let wait = a.wait_seconds.take().map(|n| n.get()).unwrap_or(10);
                self.submit(p, Work::Shell(a), cwd, policy, id, wait).await
            }
            Operation::Tasks(a) => {
                if let Some(id) = a.task_id {
                    let task = self.jobs.get_owned(&p.id, id).map_err(Failure::from)?;
                    Ok(self
                        .jobs
                        .report(&task, a.wait_seconds.map(|n| n.get()).unwrap_or(0))
                        .await)
                } else {
                    Ok(self.jobs.list(
                        &p.id,
                        &self.cwd(a.cwd.as_deref(), p)?,
                        a.limit.map(|n| n.get()).unwrap_or(5) as usize,
                    ))
                }
            }
            Operation::Cancel(a) => {
                let task = self
                    .jobs
                    .get_owned(&p.id, a.task_id)
                    .map_err(Failure::from)?;
                self.jobs.cancel(&task).map_err(Failure::from)?;
                Ok(self.jobs.report(&task, 10).await)
            }
            Operation::Poll(a) => {
                let task = self
                    .jobs
                    .get_owned(&p.id, a.task_id)
                    .map_err(Failure::from)?;
                self.jobs
                    .poll(
                        &task,
                        a.wait_seconds.map(|n| n.get()).unwrap_or(0),
                        a.stdout_offset.unwrap_or(0),
                        a.stderr_offset.unwrap_or(0),
                        a.limit.map(|n| n.get()).unwrap_or(12000) as usize,
                    )
                    .await
                    .map_err(Failure::from)
            }
            Operation::Input(a) => {
                let task = self
                    .jobs
                    .get_owned(&p.id, a.task_id)
                    .map_err(Failure::from)?;
                self.jobs
                    .input(&task, &a.text, a.eof)
                    .await
                    .map_err(Failure::from)?;
                Ok(json!({"task_id":a.task_id,"accepted_bytes":a.text.len(),"stdin_closed":a.eof}))
            }
        }
    }
    async fn submit(
        &self,
        p: &Principal,
        work: Work,
        cwd: PathBuf,
        policy: Policy,
        id: Option<Uuid>,
        wait: u64,
    ) -> Result<Value> {
        let (task, created) = self
            .jobs
            .start_owned(
                &p.id,
                work.name(),
                &cwd,
                &serde_json::to_value(&work).expect("work"),
                id,
            )
            .map_err(Failure::from)?;
        if created {
            let jobs = self.jobs.clone();
            let task = task.clone();
            tokio::spawn(async move {
                let permit = tokio::select! {
                    permit=jobs.slots.clone().acquire_owned()=>match permit {Ok(p)=>p,Err(_)=>{jobs.finish(&task,State::Interrupted,json!({"error":Failure::new(Code::InvalidState,"Executor is shutting down","Inspect Tasks after reconnecting.")}));return;}},
                    _=task.cancel.cancelled()=>{jobs.finish(&task,State::Cancelled,json!({"not_started":true}));return;}
                };
                let _gate = if matches!(&work, Work::Shell(_)) {
                    None
                } else {
                    Some(tokio::select! {
                        gate=jobs.mutation_gate.lock()=>gate,
                        _=task.cancel.cancelled()=>{jobs.finish(&task,State::Cancelled,json!({"not_started":true}));return;}
                    })
                };
                match jobs.running(&task) {
                    Ok(true) => {}
                    Ok(false) => {
                        jobs.finish(&task, State::Cancelled, json!({"not_started":true}));
                        return;
                    }
                    Err(e) => {
                        jobs.finish(&task, State::Failed, json!({"error":Failure::from(e)}));
                        return;
                    }
                }
                let outcome: Result<(State, Value)> = if let Work::Shell(a) = work {
                    crate::process::shell(jobs.clone(), task.clone(), a)
                        .await
                        .map_err(Failure::from)
                } else {
                    tokio::task::spawn_blocking(move || {
                        let r = match work {
                            Work::Write(a) => {
                                check(&policy, &files::resolve(&cwd, &a.path))?;
                                files::write(&cwd, &a)
                            }
                            Work::Edit(a) => {
                                check(&policy, &files::resolve(&cwd, &a.path))?;
                                files::edit(&cwd, &a)
                            }
                            Work::Patch(a) => {
                                crate::patch::apply_with_policy(&cwd, &a, Some(&policy))
                                    .map_err(Failure::from)
                            }
                            Work::Shell(_) => unreachable!(),
                        }?;
                        Ok((
                            if r.success {
                                State::Completed
                            } else {
                                State::Failed
                            },
                            json!(r),
                        ))
                    })
                    .await
                    .map_err(|e| Failure::from(anyhow::Error::from(e)))
                    .and_then(|r| r)
                };
                match outcome {
                    Ok((state, result)) => jobs.finish(&task, state, result),
                    Err(e) => jobs.finish(&task, State::Failed, json!({"error":e})),
                }
                drop(permit);
            });
        }
        Ok(self.jobs.report(&task, wait).await)
    }
}
fn check(policy: &Policy, path: &Path) -> Result<()> {
    policy.check(path).map_err(|e| {
        Failure::new(
            Code::PermissionDenied,
            e.to_string(),
            "Choose a granted workspace path outside protected executor data.",
        )
    })
}
