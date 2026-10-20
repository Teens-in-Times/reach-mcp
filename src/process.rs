use crate::{
    SshConfig,
    jobs::{Jobs, Task},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
mod tree {
    use super::*;
    use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};

    pub struct Owner(isize);
    impl Drop for Owner {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as _);
            }
        }
    }
    pub fn attach(child: &tokio::process::Child) -> Result<Owner> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            anyhow::ensure!(
                !handle.is_null(),
                "CreateJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            let owner = Owner(handle as isize);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            anyhow::ensure!(
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as _,
                    std::mem::size_of_val(&info) as u32
                ) != 0,
                "SetInformationJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            let process = child
                .raw_handle()
                .context("Child process has no Windows handle")?;
            anyhow::ensure!(
                AssignProcessToJobObject(handle, process as _) != 0,
                "AssignProcessToJobObject failed: {}",
                std::io::Error::last_os_error()
            );
            Ok(owner)
        }
    }
}

#[cfg(not(windows))]
mod tree {
    use super::*;
    pub struct Owner;
    pub fn attach(_: &tokio::process::Child) -> Result<Owner> {
        Ok(Owner)
    }
}

fn hide(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    #[cfg(not(windows))]
    let _ = command;
}

struct Capture {
    preview: Vec<u8>,
    total: u64,
    stored: u64,
    path: PathBuf,
}

async fn capture(mut input: impl AsyncRead + Unpin, path: PathBuf) -> Result<Capture> {
    let mut file = tokio::fs::File::create(&path).await?;
    let mut result = Capture {
        preview: Vec::new(),
        total: 0,
        stored: 0,
        path,
    };
    let mut buffer = [0u8; 8192];
    loop {
        let n = input.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        result.total += n as u64;
        let preview = (12_000usize.saturating_sub(result.preview.len())).min(n);
        result.preview.extend_from_slice(&buffer[..preview]);
        let store = (16 * 1024 * 1024u64)
            .saturating_sub(result.stored)
            .min(n as u64) as usize;
        if store > 0 {
            file.write_all(&buffer[..store]).await?;
            file.flush().await?;
            result.stored += store as u64;
        }
    }
    file.flush().await?;
    Ok(result)
}

pub async fn shell(jobs: Arc<Jobs>, task: Arc<Task>, args: Value) -> Result<(String, Value)> {
    let (id, cwd) = {
        let record = task.record.lock().expect("task");
        (record.task_id, record.cwd.clone())
    };
    let command = crate::files::text_arg(&args, "command")?;
    let timeout = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(600);
    let mut shell = Command::new(if cfg!(windows) {
        "powershell.exe"
    } else {
        "bash"
    });
    let keep_stdin = crate::files::flag(&args, "keep_stdin")?;
    if cfg!(windows) {
        shell.args(["-NoProfile", "-NoLogo"]);
        if !keep_stdin {
            shell.arg("-NonInteractive");
        }
        shell.arg("-Command")
            .arg(format!("$OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = $OutputEncoding; {command}"));
    } else {
        shell.arg("-c").arg(command);
    }
    hide(&mut shell);
    let mut child = shell
        .current_dir(cwd)
        .stdin(if keep_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let owner = match tree::attach(&child) {
        Ok(owner) => owner,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let pid = child.id();
    *task.stdin.lock().await = child.stdin.take();
    let (out_path, err_path) = jobs.logs(id);
    let out = tokio::spawn(capture(child.stdout.take().context("stdout")?, out_path));
    let err = tokio::spawn(capture(child.stderr.take().context("stderr")?, err_path));
    let (state, status) = tokio::select! {
        status=child.wait()=> ("completed",status?),
        _=task.cancel.cancelled()=> {
            task.stdin.lock().await.take();
            drop(owner);
            let _=child.kill().await;
            let status=child.wait().await?;
            return collect("cancelled",status.code(),pid,out,err).await;
        },
        _=tokio::time::sleep(Duration::from_secs(timeout))=> {
            task.stdin.lock().await.take();
            drop(owner);
            let _=child.kill().await;
            let status=child.wait().await?;
            return collect("timed_out",status.code(),pid,out,err).await;
        }
    };
    // Closing this task's job also closes any descendants still holding output pipes.
    task.stdin.lock().await.take();
    drop(owner);
    collect(
        if status.success() { state } else { "failed" },
        status.code(),
        pid,
        out,
        err,
    )
    .await
}

async fn collect(
    state: &str,
    code: Option<i32>,
    pid: Option<u32>,
    out: tokio::task::JoinHandle<Result<Capture>>,
    err: tokio::task::JoinHandle<Result<Capture>>,
) -> Result<(String, Value)> {
    let out = out.await??;
    let err = err.await??;
    Ok((
        state.into(),
        json!({"exit_code":code,"pid":pid,
        "stdout":String::from_utf8_lossy(&out.preview),"stderr":String::from_utf8_lossy(&err.preview),
        "stdout_bytes":out.total,"stderr_bytes":err.total,
        "stdout_path":out.path,"stderr_path":err.path,
        "output_truncated":out.total>out.preview.len() as u64 || err.total>err.preview.len() as u64,
        "logs_truncated":out.total>out.stored || err.total>err.stored}),
    ))
}

pub async fn tunnel(config: SshConfig, local_port: u16, stop: CancellationToken) {
    while !stop.is_cancelled() {
        let mut command = Command::new(if cfg!(windows) { "ssh.exe" } else { "ssh" });
        command
            .args([
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=10",
            ])
            .arg("-o")
            .arg(format!("HostName={}", config.hostname))
            .arg("-o")
            .arg(format!("HostKeyAlias={}", config.host_key_alias))
            .arg("-R")
            .arg(format!(
                "127.0.0.1:{}:127.0.0.1:{local_port}",
                config.remote_port
            ))
            .arg(&config.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        hide(&mut command);
        match command.spawn() {
            Ok(mut child) => match tree::attach(&child) {
                Ok(owner) => {
                    tokio::select! {
                        result=child.wait()=>{eprintln!("SSH transport exited: {result:?}");},
                        _=stop.cancelled()=>{let _=child.kill().await;}
                    }
                    drop(owner);
                }
                Err(error) => {
                    eprintln!("SSH process ownership failed: {error}");
                    let _ = child.kill().await;
                }
            },
            Err(error) => eprintln!("SSH transport could not start: {error}"),
        }
        tokio::select! {_=stop.cancelled()=>{},_=tokio::time::sleep(Duration::from_secs(5))=>{}}
    }
}
