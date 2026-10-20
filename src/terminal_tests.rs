#![cfg(windows)]
use crate::terminal::{self, State, Terminals};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
fn args<T: DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}
async fn screen(m: &Arc<Terminals>, owner: &str, id: Uuid, needle: &str) -> terminal::ReadReceipt {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        let r = m
            .read(owner, args(json!({"terminal_id":id})))
            .await
            .unwrap();
        if r.screen.as_ref().unwrap().lines.join("\n").contains(needle) {
            return r;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Missing {needle:?}: {r:?}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
async fn send(m: &Arc<Terminals>, owner: &str, id: Uuid, text: &str) {
    let r = m
        .input(
            owner,
            args(json!({"terminal_id":id,"input":{"kind":"text","text":text}})),
        )
        .await
        .unwrap();
    assert!(matches!(r.delivery, terminal::Delivery::Written), "{r:?}");
}
async fn closed(m: &Arc<Terminals>, id: Uuid) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let rows = m.list("a");
        let r = rows.iter().find(|r| r.terminal_id == id).unwrap();
        if !matches!(r.state, State::Running | State::Starting | State::Closing) && r.output_closed
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Terminal did not close: {r:?}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn windows_real_pty_persistent_shell_resize_unicode_and_independent_agents() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("workspace");
    std::fs::create_dir(&cwd).unwrap();
    let m = Terminals::open(&root.path().join("state")).unwrap();
    let operation_id = Uuid::new_v4();
    let a: terminal::OpenArgs =
        args(json!({"env":{"REACH_PTY_TEST":"isolated-a"},"operation_id":operation_id}));
    let first = m.create("a", &cwd, a.clone()).unwrap();
    assert_eq!(first.state, State::Running);
    assert_eq!(
        m.create("a", &cwd, a).unwrap().terminal_id,
        first.terminal_id
    );
    let second = m
        .create(
            "b",
            root.path(),
            args(json!({"env":{"REACH_PTY_TEST":"isolated-b"}})),
        )
        .unwrap();
    let id = first.terminal_id;
    // Construct markers at runtime so input echo cannot satisfy assertions.
    send(&m,"a",id,"$global:reachValue='PERSIST'; Set-Location ..; Write-Output ('PTY_' + [Console]::IsOutputRedirected); Write-Output ('ENV_' + $env:REACH_PTY_TEST)\r").await;
    screen(&m, "a", id, "PTY_False").await;
    screen(&m, "a", id, "ENV_isolated-a").await;
    send(&m,"a",id,"Write-Output ($global:reachValue + '_VALUE'); Write-Output ('TEXT_' + [char]0x4e1c + [char]0x4eac); Write-Output ('DIR_' + (Get-Location).Path)\r").await;
    screen(&m, "a", id, "PERSIST_VALUE").await;
    screen(&m, "a", id, "TEXT_东京").await;
    let r = screen(&m, "a", id, "DIR_").await;
    assert!(
        r.screen
            .unwrap()
            .lines
            .join("\n")
            .contains(root.path().to_str().unwrap())
    );
    send(
        &m,
        "b",
        second.terminal_id,
        "Write-Output ('ENV_' + $env:REACH_PTY_TEST)\r",
    )
    .await;
    screen(&m, "b", second.terminal_id, "ENV_isolated-b").await;
    assert!(m.read("b", args(json!({"terminal_id":id}))).await.is_err());
    m.resize("a", args(json!({"terminal_id":id,"rows":32,"cols":120})))
        .unwrap();
    send(
        &m,
        "a",
        id,
        "Write-Output ('SIZE_' + [Console]::WindowWidth + '_' + [Console]::WindowHeight)\r",
    )
    .await;
    let r = screen(&m, "a", id, "SIZE_120_32").await;
    assert_eq!(r.screen.unwrap().cols, 120);
    m.close("a", id).unwrap();
    closed(&m, id).await;
    assert!(
        m.input(
            "a",
            args(json!({"terminal_id":id,"input":{"kind":"key","key":"enter"}}))
        )
        .await
        .is_err()
    );
    m.close("b", second.terminal_id).unwrap();
}

#[tokio::test]
async fn windows_pty_interactive_keys_input_retry_gaps_idle_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let limits = terminal::Limits {
        output_bytes: 256,
        idle: Duration::from_millis(100),
        retention: Duration::from_millis(50),
        ..Default::default()
    };
    let m = Terminals::with_limits(root.path(), limits).unwrap();
    let script = "[Console]::TreatControlCAsInput=$true; [Console]::WriteLine(('K'+'READY')); $key=[Console]::ReadKey($true); [Console]::WriteLine(('CODE_'+[int]$key.KeyChar)); Start-Sleep -Seconds 30";
    let r=m.create("a",root.path(),args(json!({"program":"powershell.exe","args":["-NoLogo","-NoProfile","-Command",script]}))).unwrap();
    let id = r.terminal_id;
    screen(&m, "a", id, "KREADY").await;
    let input_id = Uuid::new_v4();
    let input: terminal::InputArgs =
        args(json!({"terminal_id":id,"input_id":input_id,"input":{"kind":"key","key":"ctrl_c"}}));
    let sent = m.input("a", input.clone()).await.unwrap();
    assert!(matches!(sent.delivery, terminal::Delivery::Written));
    assert_eq!(m.input("a", input).await.unwrap().input_id, input_id);
    screen(&m, "a", id, "CODE_3").await;
    let raw = m
        .read(
            "a",
            args(json!({"terminal_id":id,"view":"both","cursor":0})),
        )
        .await
        .unwrap();
    assert!(raw.stream.unwrap().end_cursor > 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    m.sweep();
    closed(&m, id).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    m.sweep();
    assert!(m.read("a", args(json!({"terminal_id":id}))).await.is_err());
    assert_eq!(m.list("a")[0].state, State::IdleExpired);
    m.shutdown();
    drop(m);
    let recovered = Terminals::open(root.path()).unwrap();
    assert_eq!(recovered.list("a")[0].state, State::IdleExpired);
    assert!(recovered.list("a")[0].output_expired);
}

#[tokio::test]
async fn windows_fullscreen_vt_and_output_overflow_are_readable_without_replaying_frames() {
    let root = tempfile::tempdir().unwrap();
    let m = Terminals::with_limits(
        root.path(),
        terminal::Limits {
            output_bytes: 512,
            ..Default::default()
        },
    )
    .unwrap();
    let script = "$e=[char]27; [Console]::Write(($e+'[?1049h'+$e+'[2J'+$e+'[H'+('FRAME_'+'READY'))); [Console]::ReadLine() | Out-Null; [Console]::Write(($e+'[?1049l')); [Console]::WriteLine(('x'*4096)); [Console]::WriteLine(('FINAL_'+'READY')); Start-Sleep -Seconds 30";
    let r=m.create("a",root.path(),args(json!({"program":"powershell.exe","args":["-NoLogo","-NoProfile","-Command",script]}))).unwrap();
    let id = r.terminal_id;
    let frame = screen(&m, "a", id, "FRAME_READY").await;
    assert!(frame.screen.as_ref().unwrap().alternate_screen, "{frame:?}");
    send(&m, "a", id, "\r").await;
    let final_frame = screen(&m, "a", id, "FINAL_READY").await;
    let snapshot = final_frame.screen.unwrap();
    assert!(!snapshot.alternate_screen);
    let same = m
        .read(
            "a",
            args(json!({"terminal_id":id,"since_screen":snapshot.version})),
        )
        .await
        .unwrap();
    assert!(same.screen.is_none());
    let raw = m
        .read(
            "a",
            args(json!({"terminal_id":id,"view":"stream","cursor":0,"limit":100})),
        )
        .await
        .unwrap()
        .stream
        .unwrap();
    assert!(raw.gap.is_some());
    assert!(raw.has_more);
    assert_eq!(raw.next_cursor, raw.oldest_cursor + 100);
    m.close("a", id).unwrap();
    closed(&m, id).await;
}
