use crate::results::{CommandOutput, TaskOutput};
use crate::{
    SshConfig,
    jobs::{Completion, Jobs, StreamStats, Task},
};
use anyhow::{Context, Result};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
pub(crate) mod tree {
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
        attach_handle(
            child
                .raw_handle()
                .context("Child process has no Windows handle")?,
        )
    }
    pub fn attach_handle(process: std::os::windows::io::RawHandle) -> Result<Owner> {
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
pub(crate) mod tree {
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
}

async fn capture(
    mut input: impl AsyncRead + Unpin,
    path: PathBuf,
    task: Arc<Task>,
    stream: usize,
) -> Result<Capture> {
    let mut file = tokio::fs::File::create(&path).await?;
    let mut result = Capture {
        preview: Vec::new(),
        total: 0,
        stored: 0,
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
        task.streams.lock().expect("streams")[stream] = Some(StreamStats {
            total: result.total,
            stored: result.stored,
        });
        task.changed.notify_waiters();
    }
    file.flush().await?;
    Ok(result)
}

pub async fn shell(
    jobs: Arc<Jobs>,
    task: Arc<Task>,
    args: crate::operation::ShellArgs,
) -> Result<Completion> {
    let (id, cwd) = {
        let record = task.record.lock().expect("task");
        (record.task_id, record.cwd.clone())
    };
    let command = &args.command;
    let timeout = args.timeout_seconds.map(|n| n.get()).unwrap_or(600);
    let mut shell = Command::new(if cfg!(windows) {
        "powershell.exe"
    } else {
        "bash"
    });
    let keep_stdin = args.keep_stdin;
    if cfg!(windows) {
        shell.args(["-NoProfile", "-NoLogo"]);
        if !keep_stdin {
            shell.arg("-NonInteractive");
        }
        shell.arg("-Command")
            .arg(format!("if ([Console]::ReadLine() -ne 'reach-start') {{ exit 125 }}; $OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = $OutputEncoding; & {{ {command}\n }}"));
    } else {
        shell.arg("-c").arg(command);
    }
    hide(&mut shell);
    let mut child = shell
        .current_dir(cwd)
        .stdin(if keep_stdin || cfg!(windows) {
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
    let mut stdin = child.stdin.take();
    #[cfg(windows)]
    {
        // No user command can run before job ownership is established. If Reach
        // dies before this handshake, EOF exits the shell without running it.
        let input = stdin.as_mut().context("Missing command launch gate")?;
        tokio::time::timeout(Duration::from_secs(5), input.write_all(b"reach-start\n"))
            .await
            .context("Command launch gate timed out")??;
    }
    if keep_stdin {
        *task.stdin.lock().await = stdin.take();
    }
    drop(stdin);
    let (out_path, err_path) = jobs.logs(id);
    let failure = CancellationToken::new();
    let out_task = task.clone();
    let out_failure = failure.clone();
    let stdout = child.stdout.take().context("stdout")?;
    let out = tokio::spawn(async move {
        let result = capture(stdout, out_path, out_task, 0).await;
        if result.is_err() {
            out_failure.cancel();
        }
        result
    });
    let err_task = task.clone();
    let err_failure = failure.clone();
    let stderr = child.stderr.take().context("stderr")?;
    let err = tokio::spawn(async move {
        let result = capture(stderr, err_path, err_task, 1).await;
        if result.is_err() {
            err_failure.cancel();
        }
        result
    });
    let (state, status) = tokio::select! {
        status=child.wait()=> (Reason::Exit,status?),
        _=failure.cancelled()=> {
            task.stdin.lock().await.take();drop(owner);let _=child.kill().await;let status=child.wait().await?;return collect(Reason::Io,status.code(),pid,out,err).await;
        },
        _=task.cancel.cancelled()=> {
            task.stdin.lock().await.take();
            drop(owner);
            let _=child.kill().await;
            let status=child.wait().await?;
            return collect(Reason::Cancelled,status.code(),pid,out,err).await;
        },
        _=tokio::time::sleep(Duration::from_secs(timeout))=> {
            task.stdin.lock().await.take();
            drop(owner);
            let _=child.kill().await;
            let status=child.wait().await?;
            return collect(Reason::Timeout,status.code(),pid,out,err).await;
        }
    };
    // Closing this task's job also closes any descendants still holding output pipes.
    task.stdin.lock().await.take();
    drop(owner);
    collect(state, status.code(), pid, out, err).await
}

enum Reason {
    Exit,
    Cancelled,
    Timeout,
    Io,
}
async fn collect(
    reason: Reason,
    code: Option<i32>,
    pid: Option<u32>,
    out: tokio::task::JoinHandle<Result<Capture>>,
    err: tokio::task::JoinHandle<Result<Capture>>,
) -> Result<Completion> {
    // Join both drains even when one failed, so no detached capture task survives a result.
    let (out, err) = tokio::join!(out, err);
    let out = out??;
    let err = err??;
    fn preview(bytes: &[u8]) -> (String, u64, bool) {
        let bytes = if let Err(e) = std::str::from_utf8(bytes)
            && e.error_len().is_none()
        {
            &bytes[..e.valid_up_to()]
        } else {
            bytes
        };
        (
            String::from_utf8_lossy(bytes).into_owned(),
            bytes.len() as u64,
            std::str::from_utf8(bytes).is_err(),
        )
    }
    let (stdout, stdout_next_offset, stdout_encoding_errors) = preview(&out.preview);
    let (stderr, stderr_next_offset, stderr_encoding_errors) = preview(&err.preview);
    let output = CommandOutput {
        exit_code: code,
        pid,
        stdout,
        stderr,
        stdout_next_offset,
        stderr_next_offset,
        stdout_bytes: out.total,
        stderr_bytes: err.total,
        stdout_stored: out.stored,
        stderr_stored: err.stored,
        stdout_encoding_errors,
        stderr_encoding_errors,
        output_truncated: out.total > out.preview.len() as u64
            || err.total > err.preview.len() as u64,
        logs_truncated: out.total > out.stored || err.total > err.stored,
    };
    Ok(match reason {
        Reason::Cancelled => Completion::Cancelled {
            result: Some(output),
            not_started: false,
        },
        Reason::Timeout => Completion::TimedOut { result: output },
        Reason::Exit if code == Some(0) => Completion::Completed {
            result: TaskOutput::Command(output),
        },
        _ => Completion::Failed {
            error: crate::error::Failure::new(
                crate::error::Code::ExecutionFailed,
                format!("Command exited with code {code:?}"),
                "Inspect stdout/stderr before submitting different work.",
            ),
            result: Some(TaskOutput::Command(output)),
        },
    })
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
