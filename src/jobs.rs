use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    #[serde(default = "legacy_owner")]
    pub owner: String,
    pub task_id: Uuid,
    pub tool: String,
    pub cwd: PathBuf,
    pub status: String,
    pub submitted_ms: u64,
    pub finished_ms: Option<u64>,
    pub fingerprint: String,
    pub result: Option<Value>,
}

pub struct Task {
    pub record: Mutex<Record>,
    pub cancel: CancellationToken,
    pub stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
}

fn legacy_owner() -> String {
    "legacy".into()
}

pub struct Jobs {
    root: PathBuf,
    tasks: Mutex<HashMap<Uuid, Arc<Task>>>,
    pub mutation_gate: tokio::sync::Mutex<()>,
    pub slots: Arc<tokio::sync::Semaphore>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn terminal(status: &str) -> bool {
    status != "running"
}

impl Jobs {
    pub fn open(state: &Path) -> Result<Self> {
        let root = state.join("tasks");
        fs::create_dir_all(&root)?;
        let mut tasks = HashMap::new();
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let mut record: Record = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("Cannot recover task metadata {}", path.display()))?;
            if !terminal(&record.status) {
                record.status = "interrupted".into();
                record.finished_ms = Some(now());
                record.result = Some(
                    json!({"error":"Executor restarted before the outcome was recorded. Inspect the files/logs; do not blindly repeat this operation."}),
                );
                Self::persist_at(&root, &record)?;
            }
            tasks.insert(
                record.task_id,
                Arc::new(Task {
                    record: Mutex::new(record),
                    cancel: CancellationToken::new(),
                    stdin: tokio::sync::Mutex::new(None),
                }),
            );
        }
        Ok(Self {
            root,
            tasks: Mutex::new(tasks),
            mutation_gate: tokio::sync::Mutex::new(()),
            slots: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }

    fn persist_at(root: &Path, record: &Record) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        temp.write_all(&serde_json::to_vec(record)?)?;
        temp.as_file().sync_all()?;
        temp.persist(root.join(format!("{}.json", record.task_id)))
            .map_err(|e| e.error)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn start(
        &self,
        tool: &str,
        cwd: &Path,
        args: &Value,
        id: Option<Uuid>,
    ) -> Result<(Arc<Task>, bool)> {
        self.start_owned("test", tool, cwd, args, id)
    }

    pub fn start_owned(
        &self,
        owner: &str,
        tool: &str,
        cwd: &Path,
        args: &Value,
        id: Option<Uuid>,
    ) -> Result<(Arc<Task>, bool)> {
        let fingerprint = crate::files::hash(
            serde_json::to_string(&json!({"owner":owner,"tool":tool,"cwd":cwd,"args":args}))?
                .as_bytes(),
        );
        let id = id.unwrap_or_else(Uuid::new_v4);
        let mut tasks = self.tasks.lock().expect("task registry");
        if let Some(task) = tasks.get(&id) {
            ensure!(
                task.record.lock().expect("task").owner == owner,
                "Task not found"
            );
            ensure!(
                task.record.lock().expect("task").fingerprint == fingerprint,
                "operation_id already belongs to different arguments; no operation was executed"
            );
            return Ok((task.clone(), false));
        }
        ensure!(
            tasks
                .values()
                .filter(|t| !terminal(&t.record.lock().expect("task").status))
                .count()
                < 32,
            "Too many active/queued tasks; query Tasks first"
        );
        let record = Record {
            owner: owner.into(),
            task_id: id,
            tool: tool.into(),
            cwd: cwd.to_path_buf(),
            status: "running".into(),
            submitted_ms: now(),
            finished_ms: None,
            fingerprint,
            result: None,
        };
        Self::persist_at(&self.root, &record)?;
        let task = Arc::new(Task {
            record: Mutex::new(record),
            cancel: CancellationToken::new(),
            stdin: tokio::sync::Mutex::new(None),
        });
        tasks.insert(id, task.clone());
        Ok((task, true))
    }

    pub fn finish(&self, task: &Task, status: &str, result: Value) {
        let mut record = task.record.lock().expect("task");
        record.status = status.into();
        record.finished_ms = Some(now());
        record.result = Some(result);
        if let Err(error) = Self::persist_at(&self.root, &record) {
            record.status = "unknown".into();
            record.result = Some(
                json!({"error":"The operation finished but its durable result could not be saved. Inspect before retrying.","storage_error":error.to_string()}),
            );
        }
    }

    pub fn get(&self, id: Uuid) -> Result<Arc<Task>> {
        self.tasks
            .lock()
            .expect("task registry")
            .get(&id)
            .cloned()
            .context("Task not found; use Tasks list")
    }

    pub fn get_owned(&self, owner: &str, id: Uuid) -> Result<Arc<Task>> {
        let task = self.get(id)?;
        ensure!(
            task.record.lock().expect("task").owner == owner,
            "Task not found"
        );
        Ok(task)
    }

    pub async fn report(&self, task: &Task, wait_seconds: u64) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds);
        loop {
            let record = task.record.lock().expect("task").clone();
            if terminal(&record.status) || tokio::time::Instant::now() >= deadline {
                return json!({"task_id":record.task_id,"tool":record.tool,"cwd":record.cwd,"status":record.status,
                    "submitted_ms":record.submitted_ms,"finished_ms":record.finished_ms,"result":record.result});
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn list(&self, owner: &str, cwd: &Path, limit: usize) -> Value {
        let mut records: Vec<_> = self
            .tasks
            .lock()
            .expect("task registry")
            .values()
            .map(|t| t.record.lock().expect("task").clone())
            .filter(|r| r.cwd == cwd && r.owner == owner)
            .collect();
        records.sort_by_key(|r| std::cmp::Reverse(r.submitted_ms));
        let total = records.len();
        let rows: Vec<_>=records.into_iter().take(limit).map(|r|json!({"task_id":r.task_id,
            "tool":r.tool,"status":r.status,"submitted_ms":r.submitted_ms,"finished_ms":r.finished_ms})).collect();
        json!({"cwd":cwd,"tasks":rows,"total":total})
    }

    pub fn logs(&self, id: Uuid) -> (PathBuf, PathBuf) {
        (
            self.root.join(format!("{id}.stdout.log")),
            self.root.join(format!("{id}.stderr.log")),
        )
    }

    pub async fn input(&self, task: &Task, text: &str, eof: bool) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        ensure!(text.len() <= 64 * 1024, "Input exceeds 64 KiB");
        let mut input = task.stdin.lock().await;
        let stdin=input.as_mut().context("Task input is unavailable; use Shell keep_stdin=true, and wait for the process to start")?;
        tokio::time::timeout(Duration::from_secs(5), async {
            stdin.write_all(text.as_bytes()).await?;
            stdin.flush().await
        })
        .await
        .context("Input timed out")??;
        if eof {
            input.take();
        }
        Ok(())
    }

    pub async fn poll(
        &self,
        task: &Task,
        wait: u64,
        out_offset: u64,
        err_offset: u64,
        limit: usize,
    ) -> Result<Value> {
        use std::io::{Read, Seek, SeekFrom};
        let mut report = self.report(task, wait).await;
        if let Some(result) = report.get_mut("result").and_then(Value::as_object_mut) {
            result.remove("stdout");
            result.remove("stderr");
        }
        let id = task.record.lock().expect("task").task_id;
        let (stdout, stderr) = self.logs(id);
        fn page(path: &Path, offset: u64, limit: usize) -> Result<Value> {
            let mut file = match fs::File::open(path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(json!({"text":"","next_offset":0,"available_bytes":0}));
                }
                Err(e) => return Err(e.into()),
            };
            let length = file.metadata()?.len();
            ensure!(offset <= length, "Output offset exceeds stored log length");
            file.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0; limit];
            let n = file.read(&mut data)?;
            data.truncate(n);
            // Return only complete UTF-8 code points; offsets always count source bytes.
            if let Err(e) = std::str::from_utf8(&data)
                && e.error_len().is_none()
                && e.valid_up_to() > 0
            {
                data.truncate(e.valid_up_to());
            }
            Ok(
                json!({"text":String::from_utf8_lossy(&data),"next_offset":offset+data.len() as u64,"available_bytes":length,"has_more":offset+(data.len() as u64)<length}),
            )
        }
        report["output"] = json!({"stdout":page(&stdout,out_offset,limit)?,"stderr":page(&stderr,err_offset,limit)?});
        Ok(report)
    }

    pub fn cancel(&self, task: &Task) -> Result<()> {
        let record = task.record.lock().expect("task");
        if !terminal(&record.status) && record.tool != "Shell" {
            bail!("Only Shell tasks can be cancelled; file writes finish atomically");
        }
        task.cancel.cancel();
        Ok(())
    }

    pub fn cancel_all(&self) {
        for task in self.tasks.lock().expect("task registry").values() {
            task.cancel.cancel();
        }
    }

    pub fn active_count(&self) -> usize {
        self.tasks
            .lock()
            .expect("task registry")
            .values()
            .filter(|t| !terminal(&t.record.lock().expect("task").status))
            .count()
    }
    pub fn summaries(&self, limit: usize) -> Value {
        let mut rows: Vec<_> = self
            .tasks
            .lock()
            .expect("task registry")
            .values()
            .map(|t| t.record.lock().expect("task").clone())
            .collect();
        rows.sort_by_key(|r| (terminal(&r.status), std::cmp::Reverse(r.submitted_ms)));
        json!(rows.into_iter().take(limit).map(|r|json!({"task_id":r.task_id,"owner":r.owner,"tool":r.tool,"cwd":r.cwd,"status":r.status,"submitted_ms":r.submitted_ms})).collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn automatic_ids_explicit_retry_and_restart_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(dir.path()).unwrap();
        let args = json!({"path":"a","content":"x"});
        let (one, created) = jobs.start("Write", dir.path(), &args, None).unwrap();
        assert!(created);
        let id = one.record.lock().unwrap().task_id;
        assert!(!jobs.start("Write", dir.path(), &args, Some(id)).unwrap().1);
        assert!(
            jobs.start(
                "Write",
                dir.path(),
                &json!({"path":"a","content":"y"}),
                Some(id)
            )
            .is_err()
        );
        let (two, _) = jobs.start("Write", dir.path(), &args, None).unwrap();
        assert_ne!(id, two.record.lock().unwrap().task_id);
        jobs.finish(&one, "completed", json!({"ok":true}));
        drop(jobs);
        let recovered = Jobs::open(dir.path()).unwrap();
        assert_eq!(
            recovered.report(&recovered.get(id).unwrap(), 0).await["status"],
            "completed"
        );
        let second_id = two.record.lock().unwrap().task_id;
        assert_eq!(
            recovered
                .report(&recovered.get(second_id).unwrap(), 0)
                .await["status"],
            "interrupted"
        );
    }
}
