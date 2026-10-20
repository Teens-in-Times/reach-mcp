//! Persistent PTYs are resources, not command tasks and not MCP transport sessions.
use crate::{
    error::{Code, Failure, Result},
    files::hash,
    operation::{Limit, Number, Wait},
    terminal_output::{Output, Screen, Stream},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenArgs {
    pub cwd: Option<PathBuf>,
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub rows: Option<Number<2, 120>>,
    pub cols: Option<Number<10, 240>>,
    /// Optional caller-generated retry ID. Reusing it never spawns a second terminal.
    pub operation_id: Option<Uuid>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Screen,
    Stream,
    Both,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub terminal_id: Uuid,
    pub cursor: Option<u64>,
    pub view: Option<View>,
    pub since_screen: Option<String>,
    pub start_row: Option<Number<0, 119>>,
    pub rows: Option<Number<1, 120>>,
    pub limit: Option<Limit>,
    pub wait_seconds: Option<Wait>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
    CtrlC,
    CtrlD,
    CtrlZ,
    CtrlL,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Input {
    Text {
        text: String,
        #[serde(default)]
        paste: bool,
    },
    Key {
        key: Key,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputArgs {
    pub terminal_id: Uuid,
    pub input: Input,
    pub input_id: Option<Uuid>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResizeArgs {
    pub terminal_id: Uuid,
    pub rows: Number<2, 120>,
    pub cols: Number<10, 240>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Id {
    pub terminal_id: Uuid,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    Running,
    Closing,
    Exited,
    Closed,
    IdleExpired,
    Interrupted,
}
impl State {
    fn active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Closing)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub terminal_id: Uuid,
    pub owner: String,
    pub cwd: PathBuf,
    pub program: String,
    pub pid: Option<u32>,
    pub state: State,
    pub rows: u16,
    pub cols: u16,
    pub created_ms: u64,
    pub last_client_ms: u64,
    pub ended_ms: Option<u64>,
    pub exit_code: Option<u32>,
    pub output_closed: bool,
    pub output_expired: bool,
    pub error: Option<Failure>,
    #[serde(skip)]
    pub fingerprint: String,
    pub close_reason: Option<State>,
}
// Record's durable representation must include the fingerprint, but public responses must not.
#[derive(Serialize, Deserialize)]
struct Stored {
    record: Record,
    fingerprint: String,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Queued,
    Written,
    Unknown,
}
struct Sent {
    fingerprint: String,
    delivery: Mutex<(Delivery, Option<Failure>)>,
    changed: Notify,
}
#[derive(Debug, Serialize)]
pub struct InputReceipt {
    pub terminal_id: Uuid,
    pub input_id: Uuid,
    pub delivery: Delivery,
    pub error: Option<Failure>,
}
enum Message {
    Input(Vec<u8>, Arc<Sent>),
    Response(Vec<u8>),
}
struct Controls {
    master: Box<dyn MasterPty + Send>,
    sender: mpsc::SyncSender<Message>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}
struct Session {
    record: Mutex<Record>,
    output: Mutex<Option<Output>>,
    controls: Mutex<Option<Controls>>,
    inputs: Mutex<HashMap<Uuid, Arc<Sent>>>,
    changed: Notify,
    root: PathBuf,
    lifecycle: Mutex<()>,
}
#[derive(Clone)]
pub struct Limits {
    pub total: usize,
    pub per_owner: usize,
    pub idle: Duration,
    pub retention: Duration,
    pub output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            total: 8,
            per_owner: 4,
            idle: Duration::from_secs(1800),
            retention: Duration::from_secs(3600),
            output_bytes: 1024 * 1024,
        }
    }
}
pub struct Terminals {
    root: PathBuf,
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
    limits: Limits,
    stopped: AtomicBool,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn unavailable() -> Failure {
    Failure::new(
        Code::NotFound,
        "Terminal not found",
        "Use Terminals to recover owned terminal IDs.",
    )
}
fn size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
fn save(root: &Path, r: &Record) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    temp.write_all(
        &serde_json::to_vec(&Stored {
            record: r.clone(),
            fingerprint: r.fingerprint.clone(),
        })
        .map_err(|e| Failure::new(Code::Internal, e.to_string(), "Inspect terminal state."))?,
    )?;
    temp.as_file().sync_all()?;
    temp.persist(root.join(format!("{}.json", r.terminal_id)))
        .map_err(|e| Failure::from(e.error))?;
    Ok(())
}
impl Session {
    fn update(&self, f: impl FnOnce(&mut Record)) {
        let mut r = self.record.lock().expect("terminal");
        f(&mut r);
        if let Err(e) = save(&self.root, &r) {
            r.error = Some(e);
        }
        self.changed.notify_waiters();
    }
    fn close(&self, state: State) {
        let _lifecycle = self.lifecycle.lock().expect("terminal lifecycle");
        self.update(|r| {
            if r.state.active() {
                r.state = State::Closing;
                r.close_reason = Some(state);
            }
        });
        let controls = self.controls.lock().expect("controls").take();
        if let Some(mut c) = controls {
            let _ = c.killer.kill();
            drop(c);
        }
    }
}
impl Terminals {
    pub fn open(state: &Path) -> Result<Arc<Self>> {
        Self::with_limits(state, Limits::default())
    }
    pub fn with_limits(state: &Path, limits: Limits) -> Result<Arc<Self>> {
        let root = state.join("terminals");
        fs::create_dir_all(&root)?;
        let mut sessions = HashMap::new();
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            if path.extension().and_then(|p| p.to_str()) != Some("json") {
                continue;
            }
            let stored: Stored = serde_json::from_slice(&fs::read(path)?).map_err(|e| {
                Failure::new(
                    Code::Internal,
                    e.to_string(),
                    "Repair damaged terminal metadata before serving.",
                )
            })?;
            let mut r = stored.record;
            r.fingerprint = stored.fingerprint;
            if r.state.active() {
                r.state = State::Interrupted;
                r.ended_ms = Some(now());
            }
            r.output_closed = true;
            r.output_expired = true;
            save(&root, &r)?;
            sessions.insert(
                r.terminal_id,
                Arc::new(Session {
                    record: Mutex::new(r),
                    output: Mutex::new(None),
                    controls: Mutex::new(None),
                    inputs: Mutex::new(HashMap::new()),
                    changed: Notify::new(),
                    root: root.clone(),
                    lifecycle: Mutex::new(()),
                }),
            );
        }
        let manager = Arc::new(Self {
            root,
            sessions: Mutex::new(sessions),
            limits,
            stopped: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&manager);
        std::thread::Builder::new()
            .name("reach-terminal-reaper".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(15));
                    let Some(m) = weak.upgrade() else { break };
                    if m.stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    m.sweep();
                }
            })?;
        Ok(manager)
    }
    fn get(&self, owner: &str, id: Uuid) -> Result<Arc<Session>> {
        let s = self
            .sessions
            .lock()
            .expect("terminals")
            .get(&id)
            .cloned()
            .ok_or_else(unavailable)?;
        if s.record.lock().expect("terminal").owner != owner {
            return Err(unavailable());
        }
        Ok(s)
    }
    pub fn create(&self, owner: &str, cwd: &Path, mut a: OpenArgs) -> Result<Record> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(Failure::new(
                Code::InvalidState,
                "Terminal system is shutting down",
                "Reconnect after the executor restarts.",
            ));
        }
        a.cwd = None;
        let id = a.operation_id.take().unwrap_or_else(Uuid::new_v4);
        let fingerprint = hash(&serde_json::to_vec(&(owner, cwd, &a)).expect("terminal args"));
        let mut sessions = self.sessions.lock().expect("terminals");
        if let Some(s) = sessions.get(&id) {
            let r = s.record.lock().expect("terminal");
            if r.owner != owner {
                return Err(unavailable());
            }
            if r.fingerprint != fingerprint {
                return Err(Failure::conflict(
                    "Terminal operation_id belongs to different arguments",
                ));
            }
            return Ok(r.clone());
        }
        let active = |s: &&Arc<Session>| s.record.lock().expect("terminal").state.active();
        if sessions.len() >= 10000
            || sessions.values().filter(active).count() >= self.limits.total
            || sessions
                .values()
                .filter(|s| {
                    let r = s.record.lock().expect("terminal");
                    r.owner == owner && r.state.active()
                })
                .count()
                >= self.limits.per_owner
        {
            return Err(Failure::new(
                Code::CapacityExceeded,
                "Terminal quota reached",
                "Close an owned active terminal, or wait for idle recycling. Durable retry identities are not silently forgotten.",
            ));
        }
        let rows = a.rows.map(|n| n.get() as u16).unwrap_or(24);
        let cols = a.cols.map(|n| n.get() as u16).unwrap_or(100);
        let default_program = if cfg!(windows) {
            "powershell.exe"
        } else {
            "bash"
        };
        let program = a.program.as_deref().unwrap_or(default_program);
        if program.is_empty()
            || a.args.len() > 128
            || a.env.len() > 128
            || a.env
                .iter()
                .any(|(k, v)| k.contains(['=', '\0']) || k.is_empty() || v.contains('\0'))
        {
            return Err(Failure::invalid(
                "Invalid terminal program, argument count or environment",
            ));
        }
        let mut command = CommandBuilder::new(program);
        if a.program.is_none() {
            if cfg!(windows) {
                command.args(["-NoLogo", "-NoProfile"]);
            } else {
                command.args(["--noprofile", "--norc"]);
            }
        }
        command.args(&a.args);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        for (k, v) in &a.env {
            command.env(k, v);
        }
        let record = Record {
            terminal_id: id,
            owner: owner.into(),
            cwd: cwd.to_owned(),
            program: program.into(),
            pid: None,
            state: State::Starting,
            rows,
            cols,
            created_ms: now(),
            last_client_ms: now(),
            ended_ms: None,
            exit_code: None,
            output_closed: false,
            output_expired: false,
            error: None,
            fingerprint,
            close_reason: None,
        };
        // The durable reservation precedes process creation, so a crash never replays an open.
        save(&self.root, &record)?;
        let s = Arc::new(Session {
            record: Mutex::new(record),
            output: Mutex::new(Some(Output::new(rows, cols, self.limits.output_bytes))),
            controls: Mutex::new(None),
            inputs: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            root: self.root.clone(),
            lifecycle: Mutex::new(()),
        });
        sessions.insert(id, s.clone());
        drop(sessions);
        if let Err(e) = spawn(s.clone(), command, rows, cols) {
            s.close(State::Interrupted);
            s.update(|r| {
                r.state = State::Interrupted;
                r.ended_ms = Some(now());
                r.error = Some(e);
                r.output_closed = true;
            });
        }
        let record = s.record.lock().expect("terminal").clone();
        Ok(record)
    }
    pub fn list(&self, owner: &str) -> Vec<Record> {
        self.sweep();
        let sessions = self.sessions.lock().expect("terminals");
        let mut r: Vec<_> = sessions
            .values()
            .map(|s| s.record.lock().expect("terminal").clone())
            .filter(|r| r.owner == owner)
            .collect();
        r.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
        r.truncate(100);
        r
    }
    pub async fn read(&self, owner: &str, a: ReadArgs) -> Result<ReadReceipt> {
        let s = self.get(owner, a.terminal_id)?;
        s.update(|r| r.last_client_ms = now());
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(a.wait_seconds.map(|n| n.get()).unwrap_or(0));
        loop {
            let notified = s.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let r = s.record.lock().expect("terminal").clone();
            let (screen, stream, changed) = {
                let o = s.output.lock().expect("output");
                let o=o.as_ref().ok_or_else(||Failure::new(Code::CursorInvalid,"Terminal output expired or executor restarted","Use Terminals for lifecycle metadata. A previous PTY is never silently recreated."))?;
                let view = a.view.as_ref().unwrap_or(&View::Screen);
                let screen = if matches!(view, View::Screen | View::Both) {
                    o.screen(
                        a.since_screen.as_deref(),
                        a.start_row.map(|n| n.get() as u16).unwrap_or(0),
                        a.rows.map(|n| n.get() as u16).unwrap_or(120),
                    )?
                } else {
                    None
                };
                let cursor = a.cursor.unwrap_or(0);
                let stream = if matches!(view, View::Stream | View::Both) {
                    Some(o.stream(cursor, a.limit.map(|n| n.get() as usize).unwrap_or(12000))?)
                } else {
                    None
                };
                let changed =
                    screen.is_some() || (!matches!(view, View::Screen) && o.end() > cursor);
                (screen, stream, changed)
            };
            if changed || r.output_closed || tokio::time::Instant::now() >= deadline {
                return Ok(ReadReceipt {
                    terminal: r,
                    screen,
                    stream,
                });
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }
    pub async fn input(&self, owner: &str, a: InputArgs) -> Result<InputReceipt> {
        let s = self.get(owner, a.terminal_id)?;
        let bytes = encode(&a.input, &s)?;
        if bytes.len() > 8192 {
            return Err(Failure::invalid("Terminal input exceeds 8 KiB"));
        }
        let id = a.input_id.unwrap_or_else(Uuid::new_v4);
        let fingerprint = hash(&bytes);
        let sent = {
            let mut inputs = s.inputs.lock().expect("terminal inputs");
            if let Some(sent) = inputs.get(&id) {
                if sent.fingerprint != fingerprint {
                    return Err(Failure::conflict("input_id belongs to different bytes"));
                }
                sent.clone()
            } else {
                if inputs.len() >= 4096 {
                    return Err(Failure::new(
                        Code::CapacityExceeded,
                        "Terminal input retry ledger reached 4096 entries",
                        "Open a new terminal when safe; previous input identities are not forgotten.",
                    ));
                }
                let r = s.record.lock().expect("terminal");
                if r.state != State::Running {
                    return Err(Failure::new(
                        Code::InvalidState,
                        "Terminal is not running",
                        "Read terminal state; input was not queued.",
                    ));
                }
                drop(r);
                let sent = Arc::new(Sent {
                    fingerprint,
                    delivery: Mutex::new((Delivery::Queued, None)),
                    changed: Notify::new(),
                });
                let controls = s.controls.lock().expect("controls");
                let c = controls.as_ref().ok_or_else(|| {
                    Failure::new(Code::InvalidState, "Terminal is closing", "Read its state.")
                })?;
                c.sender.try_send(Message::Input(bytes,sent.clone())).map_err(|_|Failure::new(Code::CapacityExceeded,"Terminal input queue is full","Read terminal state and retry later with the same input_id; this call was not queued."))?;
                inputs.insert(id, sent.clone());
                sent
            }
        };
        s.update(|r| r.last_client_ms = now());
        let notified = sent.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let queued = matches!(sent.delivery.lock().expect("delivery").0, Delivery::Queued);
        if queued {
            let _ = tokio::time::timeout(Duration::from_secs(5), notified).await;
        }
        let (delivery, error) = sent.delivery.lock().expect("delivery").clone();
        Ok(InputReceipt {
            terminal_id: a.terminal_id,
            input_id: id,
            delivery,
            error,
        })
    }
    pub fn resize(&self, owner: &str, a: ResizeArgs) -> Result<Record> {
        let s = self.get(owner, a.terminal_id)?;
        let rows = a.rows.get() as u16;
        let cols = a.cols.get() as u16;
        let controls = s.controls.lock().expect("controls");
        let c = controls.as_ref().ok_or_else(|| {
            Failure::new(
                Code::InvalidState,
                "Terminal is closed",
                "Open a new terminal when appropriate.",
            )
        })?;
        c.master.resize(size(rows, cols)).map_err(Failure::from)?;
        if let Some(o) = s.output.lock().expect("output").as_mut() {
            o.resize(rows, cols);
        }
        s.update(|r| {
            r.rows = rows;
            r.cols = cols;
            r.last_client_ms = now();
        });
        let r = s.record.lock().expect("terminal").clone();
        Ok(r)
    }
    pub fn close(&self, owner: &str, id: Uuid) -> Result<Record> {
        let s = self.get(owner, id)?;
        s.close(State::Closed);
        let r = s.record.lock().expect("terminal").clone();
        Ok(r)
    }
    pub fn sweep(&self) {
        let sessions = self
            .sessions
            .lock()
            .expect("terminals")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for s in sessions {
            let r = s.record.lock().expect("terminal").clone();
            if r.state == State::Running
                && now().saturating_sub(r.last_client_ms) > self.limits.idle.as_millis() as u64
            {
                s.close(State::IdleExpired);
            }
            if !r.state.active()
                && !r.output_expired
                && r.ended_ms.is_some_and(|end| {
                    now().saturating_sub(end) > self.limits.retention.as_millis() as u64
                })
            {
                s.output.lock().expect("output").take();
                s.inputs.lock().expect("inputs").clear();
                s.update(|r| r.output_expired = true);
            }
        }
    }
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        for s in self.sessions.lock().expect("terminals").values() {
            s.close(State::Closed);
        }
    }
}
impl Drop for Terminals {
    fn drop(&mut self) {
        self.shutdown();
    }
}
#[derive(Debug, Serialize)]
pub struct ReadReceipt {
    pub terminal: Record,
    pub screen: Option<Screen>,
    pub stream: Option<Stream>,
}
fn encode(input: &Input, s: &Session) -> Result<Vec<u8>> {
    let application_cursor = s
        .output
        .lock()
        .expect("output")
        .as_ref()
        .is_some_and(|o| o.application_cursor());
    Ok(match input {
        Input::Text { text, paste } => {
            let bracketed = s
                .output
                .lock()
                .expect("output")
                .as_ref()
                .is_some_and(|o| o.bracketed_paste());
            if *paste && bracketed {
                format!("\x1b[200~{text}\x1b[201~").into_bytes()
            } else {
                text.as_bytes().to_vec()
            }
        }
        Input::Key { key } => match key {
            Key::Up if application_cursor => b"\x1bOA" as &[u8],
            Key::Down if application_cursor => b"\x1bOB",
            Key::Right if application_cursor => b"\x1bOC",
            Key::Left if application_cursor => b"\x1bOD",
            Key::Home if application_cursor => b"\x1bOH",
            Key::End if application_cursor => b"\x1bOF",
            Key::Enter => b"\r" as &[u8],
            Key::Tab => b"\t",
            Key::Backspace => b"\x7f",
            Key::Escape => b"\x1b",
            Key::Up => b"\x1b[A",
            Key::Down => b"\x1b[B",
            Key::Right => b"\x1b[C",
            Key::Left => b"\x1b[D",
            Key::Home => b"\x1b[H",
            Key::End => b"\x1b[F",
            Key::Delete => b"\x1b[3~",
            Key::PageUp => b"\x1b[5~",
            Key::PageDown => b"\x1b[6~",
            Key::CtrlC => b"\x03",
            Key::CtrlD => b"\x04",
            Key::CtrlZ => b"\x1a",
            Key::CtrlL => b"\x0c",
        }
        .to_vec(),
    })
}
fn spawn(s: Arc<Session>, command: CommandBuilder, rows: u16, cols: u16) -> Result<()> {
    let lifecycle = s.lifecycle.lock().expect("terminal lifecycle");
    if s.record.lock().expect("terminal").state != State::Starting {
        return Err(Failure::new(
            Code::InvalidState,
            "Terminal creation was cancelled",
            "Inspect the terminal lifecycle.",
        ));
    }
    #[cfg(windows)]
    let system: Box<dyn portable_pty::PtySystem + Send> = Box::new(crate::windows_pty::System);
    #[cfg(not(windows))]
    let system = portable_pty::native_pty_system();
    let pair = system.openpty(size(rows, cols)).map_err(Failure::from)?;
    let mut reader = pair.master.try_clone_reader().map_err(Failure::from)?;
    let mut writer = pair.master.take_writer().map_err(Failure::from)?;
    // Drain output before spawning: ConPTY creation/startup can issue terminal queries.
    let (sender, receiver) = mpsc::sync_channel::<Message>(32);
    let read_session = s.clone();
    let responses = sender.clone();
    std::thread::Builder::new()
        .name("reach-pty-output".into())
        .spawn(move || {
            let mut b = [0; 8192];
            loop {
                match reader.read(&mut b) {
                    Ok(0) => break,
                    Ok(n) => {
                        let replies = read_session
                            .output
                            .lock()
                            .expect("output")
                            .as_mut()
                            .map(|o| o.push(&b[..n]))
                            .unwrap_or_default();
                        read_session.changed.notify_waiters();
                        for r in replies {
                            let _ = responses.try_send(Message::Response(r));
                        }
                    }
                    Err(e) => {
                        read_session.update(|r| r.error = Some(e.into()));
                        break;
                    }
                }
            }
            read_session.update(|r| r.output_closed = true);
        })?;
    std::thread::Builder::new().name("reach-pty-input".into()).spawn(move|| {
        while let Ok(message)=receiver.recv() {
            let (b,sent)=match message {Message::Input(b,s)=>(b,Some(s)),Message::Response(b)=>(b,None)};
            let result=writer.write_all(&b).and_then(|()|writer.flush());
            if let Some(sent)=sent {
                *sent.delivery.lock().expect("delivery")=match result {Ok(())=>(Delivery::Written,None),Err(e)=>(Delivery::Unknown,Some(Failure::new(Code::Io,e.to_string(),"Input may have been partly delivered. Inspect the terminal; do not resend under a new ID.")))};
                sent.changed.notify_waiters();
            }
        }
    })?;
    let mut child = pair.slave.spawn_command(command).map_err(Failure::from)?;
    drop(pair.slave);
    let pid = child.process_id();
    *s.controls.lock().expect("controls") = Some(Controls {
        master: pair.master,
        sender,
        killer: child.clone_killer(),
    });
    s.update(|r| {
        r.state = State::Running;
        r.pid = pid;
    });
    drop(lifecycle);
    std::thread::Builder::new()
        .name("reach-pty-exit".into())
        .spawn(move || {
            let result = child.wait();
            s.controls.lock().expect("controls").take();
            s.update(|r| {
                if r.state.active() {
                    r.state = r.close_reason.unwrap_or(if result.is_ok() {
                        State::Exited
                    } else {
                        State::Interrupted
                    });
                    r.ended_ms = Some(now());
                }
                match result {
                    Ok(status) => r.exit_code = Some(status.exit_code()),
                    Err(e) => r.error = Some(e.into()),
                }
            });
        })?;
    Ok(())
}
