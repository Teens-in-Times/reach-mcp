use super::*;
use serde_json::{Value, json};
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, engine::Hand) {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        auth_file: root.path().join("credentials.json"),
        requests_per_minute: 240,
        admin: None,
        protected_paths: vec![],
        default_cwd: root.path().to_owned(),
        state_dir: root.path().join("state"),
        ssh: None,
    };
    let jobs = Arc::new(jobs::Jobs::open(&config.state_dir).unwrap());
    (
        root,
        engine::Hand {
            config: Arc::new(config.clone()),
            audit: Arc::new(crate::audit::Audit::open(&config.state_dir).unwrap()),
            jobs,
        },
    )
}

#[tokio::test]
async fn file_calls_generate_ids_and_retries_survive_recovery() {
    let (_root, hand) = setup();
    let args = json!({"path":"sample.txt","content":"alpha\n"});
    let first = hand.execute("Write", args.clone()).await.unwrap();
    assert_eq!(first["status"], "completed");
    let mut retry = args;
    retry["operation_id"] = first["task_id"].clone();
    assert_eq!(hand.execute("Write", retry).await.unwrap(), first);
    let read = hand
        .execute("Read", json!({"paths":["sample.txt"]}))
        .await
        .unwrap();
    let stale=hand.execute("Edit",json!({"path":"sample.txt","old_string":"alpha","new_string":"beta","expected_sha256":"wrong"})).await.unwrap();
    assert_eq!(stale["status"], "failed");
    let edit=hand.execute("Edit",json!({"path":"sample.txt","old_string":"alpha","new_string":"beta","expected_sha256":read["files"][0]["sha256"]})).await.unwrap();
    assert_eq!(edit["status"], "completed");
    let list = hand.execute("Tasks", json!({"limit":1})).await.unwrap();
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1);
    let get = hand
        .execute("Tasks", json!({"task_id":edit["task_id"]}))
        .await
        .unwrap();
    assert_eq!(get, edit);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_shell_cwd_unicode_and_deadline() {
    let (root, hand) = setup();
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let (a, b) = tokio::join!(
        hand.execute(
            "Shell",
            json!({"cwd":a,"command":"Write-Output ((Get-Location).Path); Write-Output '东京测试'"})
        ),
        hand.execute(
            "Shell",
            json!({"cwd":b,"command":"Write-Output ((Get-Location).Path)"})
        )
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a["status"], "completed");
    assert!(a["result"]["stdout"].as_str().unwrap().contains("东京测试"));
    assert!(a["result"]["stdout"].as_str().unwrap().contains("\\a"));
    assert!(b["result"]["stdout"].as_str().unwrap().contains("\\b"));
    let timed = hand
        .execute(
            "Shell",
            json!({"command":"Start-Sleep -Seconds 20","timeout_seconds":1}),
        )
        .await
        .unwrap();
    assert_eq!(timed["status"], "timed_out");
}

#[cfg(windows)]
#[tokio::test]
async fn cancellation_owns_and_terminates_descendants() {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let (root, hand) = setup();
    let pid_file = root.path().join("child.pid");
    let command = format!(
        "$child=Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 40' -WindowStyle Hidden -PassThru; Set-Content -LiteralPath '{}' -Value $child.Id; Start-Sleep -Seconds 40",
        pid_file.display()
    );
    let task = hand
        .execute("Shell", json!({"command":command,"wait_seconds":0}))
        .await
        .unwrap();
    let id = task["task_id"].clone();
    for _ in 0..100 {
        if pid_file.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let cancelled = hand.execute("Cancel", json!({"task_id":id})).await.unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    let mut alive = true;
    for _ in 0..40 {
        alive = unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                false
            } else {
                let mut code = 0;
                let ok = GetExitCodeProcess(handle, &mut code);
                CloseHandle(handle);
                ok != 0 && code == 259
            }
        };
        if !alive {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!alive, "Descendant process survived cancellation");
}

#[test]
fn catalog_is_small_and_ids_are_not_required_for_mutations() {
    let tools = catalog::tools();
    assert_eq!(tools.len(), 13);
    assert!(serde_json::to_vec(&tools).unwrap().len() < 13000);
    for tool in tools {
        if matches!(tool.name.as_ref(), "Write" | "Edit" | "Shell") {
            assert!(
                !tool.input_schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&Value::String("operation_id".into()))
            );
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn running_output_pages_and_stdin_do_not_rerun_the_command() {
    let (_root, hand) = setup();
    let task=hand.execute("Shell",json!({"command":"[Console]::WriteLine('READY'); $line=[Console]::ReadLine(); [Console]::WriteLine('INPUT='+$line)","keep_stdin":true,"wait_seconds":0})).await.unwrap();
    let id = task["task_id"].clone();
    let mut first = Value::Null;
    for _ in 0..60 {
        first = hand
            .execute("Poll", json!({"task_id":id,"limit":12000}))
            .await
            .unwrap();
        if first["output"]["stdout"]["text"]
            .as_str()
            .unwrap()
            .contains("READY")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        first["output"]["stdout"]["text"]
            .as_str()
            .unwrap()
            .contains("READY")
    );
    hand.execute("Input", json!({"task_id":id,"text":"hello\n","eof":true}))
        .await
        .unwrap();
    let finished=hand.execute("Poll",json!({"task_id":id,"stdout_offset":first["output"]["stdout"]["next_offset"],"wait_seconds":10})).await.unwrap();
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["task_id"], id);
    assert!(
        finished["output"]["stdout"]["text"]
            .as_str()
            .unwrap()
            .contains("INPUT=hello")
    );
    assert!(
        !finished["output"]["stdout"]["text"]
            .as_str()
            .unwrap()
            .contains("READY")
    );
}
