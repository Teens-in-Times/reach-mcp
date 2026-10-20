use crate::{Config, auth::Principal, catalog, files, jobs::Jobs, patch, process};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;
#[derive(Clone)]
pub struct Hand {
    pub config: Arc<Config>,
    pub jobs: Arc<Jobs>,
    pub audit: Arc<crate::audit::Audit>,
}
fn seconds(args: &Value, key: &str, default: u64, max: u64, min: u64) -> Result<u64> {
    let value = match args.get(key) {
        None => default,
        Some(v) => v
            .as_u64()
            .with_context(|| format!("{key} must be an integer"))?,
    };
    ensure!(
        value >= min && value <= max,
        "{key} must be between {min} and {max}"
    );
    Ok(value)
}

impl Hand {
    fn cwd(&self, args: &Value, principal: &Principal) -> Result<PathBuf> {
        let cwd = match args.get("cwd") {
            Some(v) => PathBuf::from(v.as_str().context("cwd must be a string")?),
            None => principal
                .roots
                .first()
                .unwrap_or(&self.config.default_cwd)
                .clone(),
        };
        ensure!(
            cwd.is_absolute() && cwd.is_dir(),
            "cwd must be an existing absolute local directory"
        );
        Ok(std::fs::canonicalize(cwd)?)
    }

    #[cfg(test)]
    pub async fn execute(&self, name: &str, args: Value) -> Result<Value> {
        self.execute_for(&Principal::full("test"), name, args).await
    }
    pub async fn execute_for(
        &self,
        principal: &Principal,
        name: &str,
        mut args: Value,
    ) -> Result<Value> {
        ensure!(
            principal.allows(name),
            "Permission denied for this operation"
        );
        catalog::validate(name, &args)?;
        let policy = crate::policy::Policy::new(&self.config, principal)?;
        if name == "Info" {
            return Ok(
                json!({"server":"reach","version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"default_cwd":principal.roots.first().unwrap_or(&self.config.default_cwd),"principal":principal.id,"scopes":principal.scopes,"profile":principal.profile,"stateless_http":true,"pty":false}),
            );
        }
        if name == "Image" {
            let cwd = self.cwd(&args, principal)?;
            policy.check(&files::resolve(&cwd, files::text_arg(&args, "path")?))?;
            return crate::image::load(&cwd, &args);
        }
        if matches!(name, "Input" | "Poll") {
            let task = self.jobs.get_owned(
                &principal.id,
                Uuid::parse_str(files::text_arg(&args, "task_id")?)?,
            )?;
            if name == "Input" {
                let text = files::text_arg(&args, "text")?;
                let eof = files::flag(&args, "eof")?;
                self.jobs.input(&task, text, eof).await?;
                return Ok(
                    json!({"task_id":task.record.lock().expect("task").task_id,"accepted_bytes":text.len(),"stdin_closed":eof}),
                );
            }
            let offset = |key: &str| -> Result<u64> {
                match args.get(key) {
                    None => Ok(0),
                    Some(v) => v
                        .as_u64()
                        .context("Offset must be a nonnegative byte offset"),
                }
            };
            return self
                .jobs
                .poll(
                    &task,
                    seconds(&args, "wait_seconds", 0, 30, 0)?,
                    offset("stdout_offset")?,
                    offset("stderr_offset")?,
                    files::count(&args, "limit", 12000, 12000)?,
                )
                .await;
        }

        if name == "Cancel" {
            let id = Uuid::parse_str(files::text_arg(&args, "task_id")?)?;
            let task = self.jobs.get_owned(&principal.id, id)?;
            self.jobs.cancel(&task)?;
            return Ok(self.jobs.report(&task, 10).await);
        }
        if name == "Tasks" {
            let action = args.get("action").and_then(Value::as_str).unwrap_or(
                if args.get("task_id").is_some() {
                    "get"
                } else {
                    "list"
                },
            );
            return match action {
                "list" => Ok(self.jobs.list(
                    &principal.id,
                    &self.cwd(&args, principal)?,
                    files::count(&args, "limit", 5, 50)?,
                )),
                "get" => {
                    let task = self.jobs.get_owned(
                        &principal.id,
                        Uuid::parse_str(files::text_arg(&args, "task_id")?)?,
                    )?;
                    if args.get("cwd").is_some() {
                        ensure!(
                            task.record.lock().expect("task").cwd == self.cwd(&args, principal)?,
                            "Task belongs to another cwd"
                        );
                    }
                    Ok(self
                        .jobs
                        .report(&task, seconds(&args, "wait_seconds", 0, 30, 0)?)
                        .await)
                }
                _ => bail!("Unknown Tasks action; use list or get"),
            };
        }
        let cwd = self.cwd(&args, principal)?;
        if matches!(name, "Read" | "Glob" | "Grep") {
            let name = name.to_string();
            return tokio::task::spawn_blocking(move || {
                if name == "Read" {
                    files::read_with_policy(&cwd, &args, Some(&policy))
                } else {
                    files::search_with_policy(&name, &cwd, &args, Some(&policy))
                }
            })
            .await?;
        }
        ensure!(
            matches!(name, "Write" | "Edit" | "Shell" | "ApplyPatch"),
            "Unknown tool"
        );
        if matches!(name, "Write" | "Edit") {
            policy.check(&files::resolve(&cwd, files::text_arg(&args, "path")?))?;
        }
        if name == "Shell" {
            ensure!(
                !files::text_arg(&args, "command")?.trim().is_empty(),
                "command must not be empty"
            );
            seconds(&args, "timeout_seconds", 600, 86400, 1)?;
        }
        let wait = if name == "Shell" {
            seconds(&args, "wait_seconds", 10, 30, 0)?
        } else {
            10
        };
        let map = args
            .as_object_mut()
            .context("arguments must be an object")?;
        let id = map
            .remove("operation_id")
            .map(|v| {
                Uuid::parse_str(v.as_str().context("operation_id must be a UUID")?)
                    .map_err(anyhow::Error::from)
            })
            .transpose()?;
        map.remove("cwd");
        map.remove("wait_seconds");
        let (task, created) = self
            .jobs
            .start_owned(&principal.id, name, &cwd, &args, id)?;
        if created {
            let jobs = self.jobs.clone();
            let task = task.clone();
            let name = name.to_string();
            tokio::spawn(async move {
                let permit = tokio::select! {
                    permit=jobs.slots.clone().acquire_owned()=>match permit {
                        Ok(p)=>p,Err(_)=>{jobs.finish(&task,"failed",json!({"error":"Executor is shutting down"}));return;}
                    },
                    _=task.cancel.cancelled()=>{jobs.finish(&task,"cancelled",json!({"not_started":true}));return;}
                };
                let outcome = if name == "Shell" {
                    process::shell(jobs.clone(), task.clone(), args).await
                } else {
                    let _gate = jobs.mutation_gate.lock().await;
                    match tokio::task::spawn_blocking(move || {
                        if name == "ApplyPatch" {
                            patch::apply_with_policy(&cwd, &args, Some(&policy))
                        } else {
                            files::mutate(&name, &cwd, &args)
                        }
                    })
                    .await
                    {
                        Ok(result) => result.map(|value| ("completed".to_string(), value)),
                        Err(error) => Err(error.into()),
                    }
                };
                match outcome {
                    Ok((status, result)) => jobs.finish(&task, &status, result),
                    Err(error) => {
                        jobs.finish(&task, "failed", json!({"error":format!("{error:#}")}))
                    }
                }
                drop(permit);
            });
        }
        Ok(self.jobs.report(&task, wait).await)
    }
}
