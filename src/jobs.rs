use crate::{
    error::{Code, Failure, Result},
    operation::Kind,
    results::{CommandOutput, TaskOutput},
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// One state definition generates durable phases, the finish-only type, and summary statuses.
macro_rules! lifecycle {
    ($($name:ident {$($field:ident:$ty:ty),*}),+ $(,)?)=>{
        #[derive(Debug,Clone,Copy,Serialize,Deserialize,PartialEq,Eq)]
        #[serde(rename_all="snake_case")] pub enum State {Queued,Running,$($name),+}
        #[derive(Debug,Clone,Serialize,Deserialize)]
        #[serde(tag="status",rename_all="snake_case")] pub enum Phase {Queued,Running,$($name{$($field:$ty),*}),+}
        #[derive(Debug,Clone)] pub enum Completion {$($name{$($field:$ty),*}),+}
        impl From<Completion> for Phase {fn from(c:Completion)->Self {match c {$(Completion::$name{$($field),*}=>Self::$name{$($field),*}),+}}}
        impl Phase {pub fn status(&self)->State {match self {Self::Queued=>State::Queued,Self::Running=>State::Running,$(Self::$name{..}=>State::$name),+}}}
    }
}
lifecycle! {
    Completed{result:TaskOutput},
    Failed{error:Failure,result:Option<TaskOutput>},
    Cancelled{result:Option<CommandOutput>,not_started:bool},
    TimedOut{result:CommandOutput},
    Interrupted{error:Failure},
    Unknown{error:Failure,result:Option<TaskOutput>},
    Expired{},
}
impl Phase {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
    pub fn is_failure(&self) -> bool {
        self.is_terminal() && !matches!(self, Self::Completed { .. })
    }
    fn command(&self) -> Option<&CommandOutput> {
        match self {
            Self::Completed {
                result: TaskOutput::Command(c),
            }
            | Self::Failed {
                result: Some(TaskOutput::Command(c)),
                ..
            }
            | Self::Cancelled {
                result: Some(c), ..
            }
            | Self::TimedOut { result: c } => Some(c),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub version: u8,
    pub owner: String,
    pub task_id: Uuid,
    pub tool: Kind,
    pub cwd: PathBuf,
    #[serde(flatten)]
    pub phase: Phase,
    pub submitted_ms: u64,
    pub finished_ms: Option<u64>,
    pub fingerprint: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub task_id: Uuid,
    pub tool: Kind,
    pub cwd: PathBuf,
    #[serde(flatten)]
    pub phase: Phase,
    pub submitted_ms: u64,
    pub finished_ms: Option<u64>,
    pub recovery_available: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub task_id: Uuid,
    pub owner: String,
    pub tool: Kind,
    pub cwd: PathBuf,
    pub status: State,
    pub submitted_ms: u64,
    pub finished_ms: Option<u64>,
}
#[derive(Debug, Serialize)]
pub struct List {
    pub cwd: Option<PathBuf>,
    pub tasks: Vec<Summary>,
    pub total: usize,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStats {
    pub total: u64,
    pub stored: u64,
}
pub struct Task {
    pub record: Mutex<Record>,
    pub cancel: CancellationToken,
    pub stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    pub changed: Notify,
    pub streams: Mutex<[Option<StreamStats>; 2]>,
}
#[derive(Clone)]
pub struct Limits {
    pub active: usize,
    pub per_owner: usize,
    pub workers: usize,
    pub ledger: usize,
    pub retained: usize,
    pub log_bytes: u64,
    pub recovery_bytes: u64,
    pub retention: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            active: 32,
            per_owner: 8,
            workers: 4,
            ledger: 10000,
            retained: 128,
            log_bytes: 256 * 1024 * 1024,
            recovery_bytes: 256 * 1024 * 1024,
            retention: Duration::from_secs(7 * 24 * 3600),
        }
    }
}
pub struct Jobs {
    root: PathBuf,
    tasks: Mutex<HashMap<Uuid, Arc<Task>>>,
    cleanup_pending: Mutex<HashSet<Uuid>>,
    limits: Limits,
    pub mutation_gate: tokio::sync::Mutex<()>,
    pub slots: Arc<tokio::sync::Semaphore>,
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn not_found() -> Failure {
    Failure::new(
        Code::NotFound,
        "Task not found",
        "Use Tasks to recover owned work; do not blindly repeat a lost submission.",
    )
}
fn task(record: Record) -> Arc<Task> {
    let streams = record
        .phase
        .command()
        .map(|c| {
            [
                Some(StreamStats {
                    total: c.stdout_bytes,
                    stored: c.stdout_stored,
                }),
                Some(StreamStats {
                    total: c.stderr_bytes,
                    stored: c.stderr_stored,
                }),
            ]
        })
        .unwrap_or([None, None]);
    Arc::new(Task {
        record: Mutex::new(record),
        cancel: CancellationToken::new(),
        stdin: tokio::sync::Mutex::new(None),
        changed: Notify::new(),
        streams: Mutex::new(streams),
    })
}
impl Jobs {
    pub fn open(state: &Path) -> Result<Self> {
        Self::with_limits(state, Limits::default())
    }
    pub fn with_limits(state: &Path, limits: Limits) -> Result<Self> {
        let root = state.join("tasks");
        fs::create_dir_all(&root)?;
        let mut tasks = HashMap::new();
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if tasks.len() >= limits.ledger {
                return Err(Failure::new(
                    Code::CapacityExceeded,
                    "Task ledger exceeds configured capacity",
                    "Archive this state namespace before accepting new work.",
                ));
            }
            if fs::metadata(&path)?.len() > 1024 * 1024 {
                return Err(Failure::new(
                    Code::InvalidState,
                    "Oversized task record",
                    "Inspect state storage before starting.",
                ));
            }
            let mut record:Record=serde_json::from_slice(&fs::read(&path)?).map_err(|e|Failure::new(Code::InvalidState,format!("Cannot recover task {}: {e}",path.display()),"Use a separate state namespace or repair the damaged record; tasks are never silently replayed."))?;
            if record.version != 3 {
                return Err(Failure::new(
                    Code::InvalidState,
                    "Unsupported task storage version",
                    "Use the matching executor or a fresh state directory.",
                ));
            }
            if !record.phase.is_terminal() {
                record.phase = Phase::Interrupted {
                    error: Failure::new(
                        Code::Interrupted,
                        "Executor stopped before a durable outcome was recorded",
                        "Inspect logs and Recover before deciding whether to retry; this operation will not replay.",
                    ),
                };
                record.finished_ms = Some(now());
                Self::persist_at(&root, &record)?;
            }
            tasks.insert(record.task_id, task(record));
        }
        let cleanup_pending = tasks
            .values()
            .filter_map(|t| {
                let r = t.record.lock().expect("task");
                matches!(r.phase, Phase::Expired { .. }).then_some(r.task_id)
            })
            .collect();
        let jobs = Self {
            root,
            tasks: Mutex::new(tasks),
            cleanup_pending: Mutex::new(cleanup_pending),
            slots: Arc::new(tokio::sync::Semaphore::new(limits.workers)),
            mutation_gate: tokio::sync::Mutex::new(()),
            limits,
        };
        jobs.cleanup()?;
        Ok(jobs)
    }
    fn persist_at(root: &Path, r: &Record) -> Result<()> {
        let mut f = tempfile::NamedTempFile::new_in(root)?;
        let bytes = serde_json::to_vec(r)
            .map_err(|e| Failure::new(Code::Internal, e.to_string(), "Inspect task storage."))?;
        f.write_all(&bytes)?;
        f.as_file().sync_all()?;
        f.persist(root.join(format!("{}.json", r.task_id)))
            .map_err(|e| e.error)?;
        Ok(())
    }
    pub fn start_owned(
        &self,
        owner: &str,
        tool: Kind,
        cwd: &Path,
        args: &impl Serialize,
        id: Option<Uuid>,
    ) -> Result<(Arc<Task>, bool)> {
        self.cleanup()?;
        let fingerprint = crate::files::hash(
            &serde_json::to_vec(&(owner, tool, cwd, args))
                .map_err(|e| Failure::invalid(e.to_string()))?,
        );
        let id = id.unwrap_or_else(Uuid::new_v4);
        let mut tasks = self.tasks.lock().expect("task registry");
        if let Some(t) = tasks.get(&id) {
            let r = t.record.lock().expect("task");
            if r.owner != owner {
                return Err(not_found());
            }
            if r.fingerprint != fingerprint {
                return Err(Failure::conflict(
                    "operation_id belongs to different arguments; nothing was executed",
                ));
            }
            return Ok((t.clone(), false));
        }
        let active = tasks
            .values()
            .filter(|t| !t.record.lock().expect("task").phase.is_terminal())
            .count();
        let owned = tasks
            .values()
            .filter(|t| {
                let r = t.record.lock().expect("task");
                r.owner == owner && !r.phase.is_terminal()
            })
            .count();
        if tasks.len() >= self.limits.ledger
            || active >= self.limits.active
            || owned >= self.limits.per_owner
        {
            return Err(Failure::new(
                Code::CapacityExceeded,
                "Task or durable retry-ledger capacity reached",
                "Wait/cancel pending work. A full identity ledger requires an explicitly new state namespace; old IDs must not be retried there.",
            ));
        }
        let r = Record {
            version: 3,
            owner: owner.into(),
            task_id: id,
            tool,
            cwd: cwd.to_owned(),
            phase: Phase::Queued,
            submitted_ms: now(),
            finished_ms: None,
            fingerprint,
        };
        Self::persist_at(&self.root, &r)?;
        let t = task(r);
        tasks.insert(id, t.clone());
        Ok((t, true))
    }
    pub fn running(&self, t: &Task) -> Result<bool> {
        let mut r = t.record.lock().expect("task");
        if !matches!(r.phase, Phase::Queued) {
            return Err(Failure::new(
                Code::InvalidState,
                "Task is not queued",
                "Inspect its state.",
            ));
        }
        if t.cancel.is_cancelled() {
            return Ok(false);
        }
        r.phase = Phase::Running;
        Self::persist_at(&self.root, &r)?;
        t.changed.notify_waiters();
        Ok(true)
    }
    pub fn finish(&self, t: &Task, completion: Completion) {
        let mut r = t.record.lock().expect("task");
        if r.phase.is_terminal() {
            return;
        }
        r.phase = completion.into();
        r.finished_ms = Some(now());
        if let Err(e) = Self::persist_at(&self.root, &r) {
            let result = match &r.phase {
                Phase::Completed { result }
                | Phase::Failed {
                    result: Some(result),
                    ..
                }
                | Phase::Unknown {
                    result: Some(result),
                    ..
                } => Some(result.clone()),
                _ => None,
            };
            r.phase = Phase::Unknown {
                error: Failure::new(
                    Code::DurabilityLost,
                    format!("Outcome could not be saved: {e}"),
                    "Inspect files, logs and Recover. Never blindly resubmit unknown work.",
                ),
                result,
            };
        }
        drop(r);
        t.changed.notify_waiters();
        if let Err(e) = self.cleanup() {
            eprintln!("Task cleanup failed: {e}");
        }
    }
    pub fn get(&self, id: Uuid) -> Result<Arc<Task>> {
        self.tasks
            .lock()
            .expect("task registry")
            .get(&id)
            .cloned()
            .ok_or_else(not_found)
    }
    pub fn get_owned(&self, owner: &str, id: Uuid) -> Result<Arc<Task>> {
        let t = self.get(id)?;
        if t.record.lock().expect("task").owner != owner {
            return Err(not_found());
        }
        Ok(t)
    }
    fn snapshot(&self, t: &Task) -> Report {
        let r = t.record.lock().expect("task");
        Report {
            task_id: r.task_id,
            tool: r.tool,
            cwd: r.cwd.clone(),
            phase: r.phase.clone(),
            submitted_ms: r.submitted_ms,
            finished_ms: r.finished_ms,
            recovery_available: self
                .recovery_root()
                .join(r.task_id.to_string())
                .join("manifest.json")
                .is_file(),
        }
    }
    pub async fn report(&self, t: &Task, wait: u64) -> Report {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
        loop {
            let notified = t.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let r = self.snapshot(t);
            if r.phase.is_terminal() || tokio::time::Instant::now() >= deadline {
                return r;
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }
    fn summary(r: &Record) -> Summary {
        Summary {
            task_id: r.task_id,
            owner: r.owner.clone(),
            tool: r.tool,
            cwd: r.cwd.clone(),
            status: r.phase.status(),
            submitted_ms: r.submitted_ms,
            finished_ms: r.finished_ms,
        }
    }
    pub fn list(&self, owner: &str, cwd: Option<&Path>, limit: usize) -> List {
        let mut records = self
            .tasks
            .lock()
            .expect("task registry")
            .values()
            .filter_map(|t| {
                let r = t.record.lock().expect("task");
                (r.owner == owner && cwd.is_none_or(|cwd| r.cwd == cwd)).then(|| Self::summary(&r))
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|r| std::cmp::Reverse(r.submitted_ms));
        let total = records.len();
        records.truncate(limit);
        List {
            cwd: cwd.map(Path::to_owned),
            tasks: records,
            total,
        }
    }
    pub fn summaries(&self, limit: usize) -> Vec<Summary> {
        let mut rows = self
            .tasks
            .lock()
            .expect("task registry")
            .values()
            .map(|t| Self::summary(&t.record.lock().expect("task")))
            .collect::<Vec<_>>();
        rows.sort_by_key(|r| {
            (
                matches!(r.status, State::Queued | State::Running)
                    .then_some(0)
                    .unwrap_or(1),
                std::cmp::Reverse(r.submitted_ms),
            )
        });
        rows.truncate(limit);
        rows
    }
    pub fn logs(&self, id: Uuid) -> (PathBuf, PathBuf) {
        (
            self.root.join(format!("{id}.stdout.log")),
            self.root.join(format!("{id}.stderr.log")),
        )
    }
    pub fn recovery_root(&self) -> PathBuf {
        self.root.join("recovery")
    }
    pub async fn input(&self, t: &Task, text: &str, eof: bool) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        if text.len() > 64 * 1024 {
            return Err(Failure::invalid("Pipe input exceeds 64 KiB"));
        }
        let mut input = t.stdin.lock().await;
        let stdin=input.as_mut().ok_or_else(||Failure::new(Code::InvalidState,"Pipe stdin is not available","Use keep_stdin when submitting Shell and wait for process startup; read task state before retrying input."))?;
        tokio::time::timeout(Duration::from_secs(5), async {
            stdin.write_all(text.as_bytes()).await?;
            stdin.flush().await
        })
        .await
        .map_err(|_| {
            Failure::new(
                Code::Io,
                "Pipe input timed out and may be partly delivered",
                "Inspect output before sending more input; do not blindly resend.",
            )
        })??;
        if eof {
            input.take();
        }
        Ok(())
    }
    pub async fn poll(
        &self,
        t: &Task,
        wait: u64,
        out: u64,
        err: u64,
        limit: usize,
        encoding: OutputEncoding,
    ) -> Result<PollReport> {
        let (id, tool) = {
            let r = t.record.lock().expect("task");
            (r.task_id, r.tool)
        };
        if tool != Kind::Shell {
            return Err(Failure::invalid(
                "Poll only reads Shell stdout/stderr; use Tasks for file mutation outcomes",
            ));
        }
        let paths = self.logs(id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
        loop {
            let notified = t.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = t.record.lock().expect("task").phase.is_terminal();
            let ready = [(&paths.0, out), (&paths.1, err)]
                .iter()
                .any(|(p, o)| fs::metadata(p).is_ok_and(|m| m.len() > *o));
            if done || ready || tokio::time::Instant::now() >= deadline {
                break;
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
        let mut report = self.snapshot(t);
        if matches!(report.phase, Phase::Expired { .. }) {
            return Err(Failure::new(
                Code::OutputExpired,
                "Task result and logs expired",
                "The retry identity is retained and cannot execute again. Submit distinct new work only when intentional.",
            ));
        }
        // Poll returns each output page once, without repeating the stored preview as well.
        match &mut report.phase {
            Phase::Completed {
                result: TaskOutput::Command(c),
            }
            | Phase::Failed {
                result: Some(TaskOutput::Command(c)),
                ..
            }
            | Phase::Cancelled {
                result: Some(c), ..
            }
            | Phase::TimedOut { result: c } => {
                c.stdout.clear();
                c.stderr.clear();
            }
            _ => {}
        }
        let streams = *t.streams.lock().expect("streams");
        Ok(PollReport {
            task: report,
            output: OutputPages {
                stdout: page(&paths.0, out, limit, streams[0], encoding)?,
                stderr: page(&paths.1, err, limit, streams[1], encoding)?,
            },
        })
    }
    pub fn cancel(&self, t: &Task) -> Result<()> {
        let r = t.record.lock().expect("task");
        if matches!(r.phase, Phase::Running) && r.tool != Kind::Shell {
            return Err(Failure::new(
                Code::InvalidState,
                "A running file commit cannot be cancelled",
                "Wait for its per-file outcome and inspect Recover if interrupted.",
            ));
        }
        t.cancel.cancel();
        Ok(())
    }
    pub fn cancel_all(&self) {
        for t in self.tasks.lock().expect("task registry").values() {
            t.cancel.cancel();
        }
    }
    pub fn active_count(&self) -> usize {
        self.tasks
            .lock()
            .expect("task registry")
            .values()
            .filter(|t| !t.record.lock().expect("task").phase.is_terminal())
            .count()
    }
    /// Expire payloads before deleting data. Retry fingerprints remain durable tombstones.
    pub fn cleanup(&self) -> Result<()> {
        let tasks = self.tasks.lock().expect("task registry");
        let mut ended = tasks
            .values()
            .filter_map(|t| {
                let r = t.record.lock().expect("task");
                (r.phase.is_terminal() && !matches!(r.phase, Phase::Expired { .. })).then_some((
                    r.finished_ms.unwrap_or(r.submitted_ms),
                    r.task_id,
                    t.clone(),
                ))
            })
            .collect::<Vec<_>>();
        ended.sort_by_key(|(when, id, _)| (std::cmp::Reverse(*when), *id));
        let mut logs = 0;
        let mut recovery = 0;
        let mut retained = 0;
        let mut pending = self.cleanup_pending.lock().expect("cleanup queue");
        for (when, id, t) in ended {
            let paths = self.logs(id);
            let log_size = fs::metadata(&paths.0).map_or(0, |m| m.len())
                + fs::metadata(&paths.1).map_or(0, |m| m.len());
            let recovery_size =
                crate::journal::disk_usage(&self.recovery_root().join(id.to_string()))?;
            if retained >= self.limits.retained
                || logs + log_size > self.limits.log_bytes
                || recovery + recovery_size > self.limits.recovery_bytes
                || now().saturating_sub(when) > self.limits.retention.as_millis() as u64
            {
                let mut r = t.record.lock().expect("task");
                let mut expired = r.clone();
                expired.phase = Phase::Expired {};
                Self::persist_at(&self.root, &expired)?;
                *r = expired;
                pending.insert(id);
                t.changed.notify_waiters();
            } else {
                logs += log_size;
                recovery += recovery_size;
                retained += 1;
            }
        }
        // Finish deletions interrupted after writing the expiration tombstone.
        for id in pending.iter().copied().collect::<Vec<_>>() {
            let paths = self.logs(id);
            for p in [paths.0, paths.1] {
                if let Err(e) = fs::remove_file(p)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(e.into());
                }
            }
            crate::journal::remove(&self.recovery_root(), id)?;
            pending.remove(&id);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputEncoding {
    #[default]
    Text,
    Base64,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Data {
    Text { text: String, encoding_errors: bool },
    Base64 { data_base64: String },
}
#[derive(Debug, Serialize)]
pub struct Page {
    #[serde(flatten)]
    pub data: Data,
    pub next_offset: u64,
    pub available_bytes: u64,
    pub total_bytes: Option<u64>,
    pub has_more: bool,
    pub truncated: Option<bool>,
}
#[derive(Debug, Serialize)]
pub struct OutputPages {
    pub stdout: Page,
    pub stderr: Page,
}
#[derive(Debug, Serialize)]
pub struct PollReport {
    #[serde(flatten)]
    pub task: Report,
    pub output: OutputPages,
}
fn page(
    path: &Path,
    offset: u64,
    limit: usize,
    stats: Option<StreamStats>,
    encoding: OutputEncoding,
) -> Result<Page> {
    let mut file = match fs::File::open(path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let len = file
        .as_ref()
        .map(|f| f.metadata())
        .transpose()?
        .map_or(0, |m| m.len());
    if offset > len {
        return Err(Failure::new(
            Code::CursorInvalid,
            "Output offset exceeds retained log length",
            "Use a cursor returned by this task. Expired output is not reset to offset zero.",
        ));
    }
    let mut data = vec![0; limit];
    let n = if let Some(f) = file.as_mut() {
        f.seek(SeekFrom::Start(offset))?;
        f.read(&mut data)?
    } else {
        0
    };
    data.truncate(n);
    if matches!(encoding, OutputEncoding::Text)
        && let Err(e) = std::str::from_utf8(&data)
        && e.error_len().is_none()
        && e.valid_up_to() > 0
    {
        data.truncate(e.valid_up_to());
    }
    let next_offset = offset + data.len() as u64;
    let data = match encoding {
        OutputEncoding::Text => Data::Text {
            encoding_errors: std::str::from_utf8(&data).is_err(),
            text: String::from_utf8_lossy(&data).into_owned(),
        },
        OutputEncoding::Base64 => Data::Base64 {
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
        },
    };
    Ok(Page {
        data,
        next_offset,
        available_bytes: len,
        total_bytes: stats.map(|s| s.total),
        has_more: next_offset < len,
        truncated: stats.map(|s| s.total > s.stored),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn success() -> Completion {
        Completion::Completed {
            result: TaskOutput::Files(crate::changes::Receipt {
                preview: false,
                success: true,
                crash_atomic: false,
                files: vec![],
                error: None,
            }),
        }
    }
    #[tokio::test]
    async fn durable_retry_identity_ownership_cancel_and_restart_never_replay() {
        let d = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(d.path()).unwrap();
        let (first, new) = jobs
            .start_owned("a", Kind::Write, d.path(), &("write", 1), None)
            .unwrap();
        assert!(new);
        let id = first.record.lock().unwrap().task_id;
        assert!(
            !jobs
                .start_owned("a", Kind::Write, d.path(), &("write", 1), Some(id))
                .unwrap()
                .1
        );
        assert_eq!(
            jobs.start_owned("a", Kind::Write, d.path(), &("write", 2), Some(id))
                .err()
                .unwrap()
                .code,
            Code::Conflict
        );
        assert_eq!(
            jobs.start_owned("b", Kind::Write, d.path(), &("write", 1), Some(id))
                .err()
                .unwrap()
                .code,
            Code::NotFound
        );
        let (queued, _) = jobs
            .start_owned("a", Kind::Write, d.path(), &("write", 2), None)
            .unwrap();
        jobs.cancel(&queued).unwrap();
        assert!(!jobs.running(&queued).unwrap());
        jobs.finish(
            &queued,
            Completion::Cancelled {
                result: None,
                not_started: true,
            },
        );
        let (unfinished, _) = jobs
            .start_owned("a", Kind::Shell, d.path(), &"command", None)
            .unwrap();
        let unfinished_id = unfinished.record.lock().unwrap().task_id;
        jobs.running(&first).unwrap();
        jobs.finish(&first, success());
        drop(jobs);
        let recovered = Jobs::open(d.path()).unwrap();
        assert!(matches!(
            recovered.report(&recovered.get(id).unwrap(), 0).await.phase,
            Phase::Completed { .. }
        ));
        let (retried, new) = recovered
            .start_owned("a", Kind::Shell, d.path(), &"command", Some(unfinished_id))
            .unwrap();
        assert!(!new);
        assert!(matches!(
            retried.record.lock().unwrap().phase,
            Phase::Interrupted { .. }
        ));
    }
    #[tokio::test]
    async fn expiration_bounds_logs_and_recovery_but_retains_retry_tombstones() {
        let d = tempfile::tempdir().unwrap();
        let limits = Limits {
            retained: 1,
            ledger: 3,
            log_bytes: 4,
            ..Default::default()
        };
        let jobs = Jobs::with_limits(d.path(), limits.clone()).unwrap();
        let (first, _) = jobs
            .start_owned("a", Kind::Shell, d.path(), &1, None)
            .unwrap();
        let id = first.record.lock().unwrap().task_id;
        let path = jobs.logs(id).0;
        fs::write(&path, b"too large").unwrap();
        jobs.finish(&first, success());
        assert!(matches!(
            jobs.report(&first, 0).await.phase,
            Phase::Expired {}
        ));
        assert!(!path.exists());
        assert!(
            !jobs
                .start_owned("a", Kind::Shell, d.path(), &1, Some(id))
                .unwrap()
                .1
        );
        let journal_dir = jobs.recovery_root().join(id.to_string());
        fs::create_dir_all(&journal_dir).unwrap();
        fs::write(journal_dir.join("abandoned.before"), b"before").unwrap();
        drop(jobs);
        let recovered = Jobs::with_limits(d.path(), limits).unwrap();
        assert!(!journal_dir.exists());
        assert!(
            !recovered
                .start_owned("a", Kind::Shell, d.path(), &1, Some(id))
                .unwrap()
                .1
        );
        for n in 2..=3 {
            let (t, _) = recovered
                .start_owned("a", Kind::Write, d.path(), &n, None)
                .unwrap();
            recovered.finish(&t, success());
        }
        assert_eq!(
            recovered
                .start_owned("a", Kind::Write, d.path(), &4, None)
                .err()
                .unwrap()
                .code,
            Code::CapacityExceeded
        );
    }
    #[tokio::test]
    async fn output_wait_wakes_on_new_bytes_and_utf8_pages_keep_source_cursors() {
        let d = tempfile::tempdir().unwrap();
        let jobs = Arc::new(Jobs::open(d.path()).unwrap());
        let (task, _) = jobs
            .start_owned("a", Kind::Shell, d.path(), &"command", None)
            .unwrap();
        jobs.running(&task).unwrap();
        let j = jobs.clone();
        let t = task.clone();
        let waiting =
            tokio::spawn(async move { j.poll(&t, 5, 0, 0, 4, OutputEncoding::Text).await });
        tokio::task::yield_now().await;
        let id = task.record.lock().unwrap().task_id;
        fs::write(jobs.logs(id).0, "ab中").unwrap();
        task.streams.lock().unwrap()[0] = Some(StreamStats {
            total: 5,
            stored: 5,
        });
        task.changed.notify_waiters();
        let first = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.output.stdout.next_offset, 2);
        assert!(matches!(first.output.stdout.data,Data::Text{text,..}if text=="ab"));
        assert!(matches!(first.task.phase, Phase::Running));
        let second = jobs
            .poll(&task, 0, 2, 0, 4, OutputEncoding::Text)
            .await
            .unwrap();
        assert!(matches!(second.output.stdout.data,Data::Text{text,..}if text=="中"));
        assert_eq!(second.output.stdout.next_offset, 5);
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn failed_durable_outcome_becomes_unknown_and_restart_is_interrupted() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(d.path()).unwrap();
        let (task, _) = jobs
            .start_owned("a", Kind::Write, d.path(), &1, None)
            .unwrap();
        let id = task.record.lock().unwrap().task_id;
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(jobs.root.join(format!("{id}.json")))
            .unwrap();
        jobs.finish(&task, success());
        assert!(matches!(
            jobs.report(&task, 0).await.phase,
            Phase::Unknown {
                error: Failure {
                    code: Code::DurabilityLost,
                    ..
                },
                ..
            }
        ));
        drop(lock);
        drop(jobs);
        let recovered = Jobs::open(d.path()).unwrap();
        assert!(matches!(
            recovered.report(&recovered.get(id).unwrap(), 0).await.phase,
            Phase::Interrupted { .. }
        ));
        assert!(
            !recovered
                .start_owned("a", Kind::Write, d.path(), &1, Some(id))
                .unwrap()
                .1
        );
    }
}
