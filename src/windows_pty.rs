//! Native ConPTY backend. The child joins its kill-on-close job during
//! CreateProcess, before any user code executes. No post-spawn ownership race.
#![cfg(windows)]
use anyhow::{Context, Result, ensure};
use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, PtySystem,
    SlavePty,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{Console::*, JobObjects::*, Pipes::CreatePipe, Threading::*},
};

type Create = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type Resize = unsafe extern "system" fn(HPCON, COORD) -> i32;
type Close = unsafe extern "system" fn(HPCON);
struct Api {
    create: Create,
    resize: Resize,
    close: Close,
    _library: isize,
}
static API: std::sync::LazyLock<std::result::Result<Api, String>> =
    std::sync::LazyLock::new(|| load_api().map_err(|e| e.to_string()));
pub fn capabilities() -> crate::terminal::Backend {
    match API.as_ref() {
        Ok(api) => crate::terminal::Backend {
            implementation: if api._library == 0 {
                "windows_system_conpty"
            } else {
                "microsoft_redistributable_conpty"
            }
            .into(),
            available: true,
            alternate_screen: api._library != 0,
            process_tree_owned_before_start: true,
            error: None,
        },
        Err(e) => crate::terminal::Backend {
            implementation: "windows_conpty".into(),
            available: false,
            alternate_screen: false,
            process_tree_owned_before_start: true,
            error: Some(e.clone()),
        },
    }
}
fn load_api() -> Result<Api> {
    use windows_sys::Win32::System::LibraryLoader::*;
    let path = std::env::current_exe()?
        .parent()
        .context("Missing executable directory")?
        .join("conpty.dll");
    if !path.exists() {
        return Ok(Api {
            create: CreatePseudoConsole,
            resize: ResizePseudoConsole,
            close: ClosePseudoConsole,
            _library: 0,
        });
    }
    let path = wide(path.as_os_str())?;
    let lib = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    };
    ensure!(
        !lib.is_null(),
        "Bundled ConPTY could not load: {}",
        io::Error::last_os_error()
    );
    // The module stays loaded for the process lifetime, keeping every function pointer valid.
    let create = unsafe { GetProcAddress(lib, c"ConptyCreatePseudoConsole".as_ptr() as _) }
        .context("ConptyCreatePseudoConsole export missing")?;
    let resize = unsafe { GetProcAddress(lib, c"ConptyResizePseudoConsole".as_ptr() as _) }
        .context("ConptyResizePseudoConsole export missing")?;
    let close = unsafe { GetProcAddress(lib, c"ConptyClosePseudoConsole".as_ptr() as _) }
        .context("ConptyClosePseudoConsole export missing")?;
    Ok(Api {
        create: unsafe { mem::transmute::<unsafe extern "system" fn() -> isize, Create>(create) },
        resize: unsafe { mem::transmute::<unsafe extern "system" fn() -> isize, Resize>(resize) },
        close: unsafe { mem::transmute::<unsafe extern "system" fn() -> isize, Close>(close) },
        _library: lib as isize,
    })
}

#[derive(Debug)]
struct Job(OwnedHandle);
impl Job {
    fn new() -> Result<Arc<Self>> {
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        ensure!(
            !handle.is_null(),
            "CreateJobObject: {}",
            io::Error::last_os_error()
        );
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle as _) });
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        ensure!(
            unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as _,
                    mem::size_of_val(&info) as _,
                )
            } != 0,
            "SetInformationJobObject: {}",
            io::Error::last_os_error()
        );
        Ok(Arc::new(job))
    }
    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle() as _
    }
    fn terminate(&self) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.handle(), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
struct Console(HPCON, &'static Api);
impl Drop for Console {
    fn drop(&mut self) {
        unsafe { (self.1.close)(self.0) };
    }
}
struct Inner {
    console: Console,
    reader: File,
    writer: Option<File>,
    size: PtySize,
    job: Arc<Job>,
}
// HPCON is stored as an integer; all console mutation is serialized by Inner's Mutex.
struct Master(Arc<Mutex<Inner>>);
struct Slave(Arc<Mutex<Inner>>);
pub struct System;
fn pipe() -> io::Result<(File, File)> {
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            File::from_raw_handle(read as _),
            File::from_raw_handle(write as _),
        )
    })
}
impl PtySystem for System {
    fn openpty(&self, size: PtySize) -> Result<PtyPair> {
        let (input, writer) = pipe()?;
        let (reader, output) = pipe()?;
        let mut pc = 0;
        let api = API.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
        let result = unsafe {
            (api.create)(
                COORD {
                    X: size.cols as i16,
                    Y: size.rows as i16,
                },
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                0,
                &mut pc,
            )
        };
        ensure!(
            result >= 0,
            "CreatePseudoConsole failed: HRESULT {result:#x}"
        );
        let console = Console(pc, api);
        let job = Job::new()?;
        drop(input);
        drop(output);
        let inner = Arc::new(Mutex::new(Inner {
            console,
            reader,
            writer: Some(writer),
            size,
            job,
        }));
        Ok(PtyPair {
            master: Box::new(Master(inner.clone())),
            slave: Box::new(Slave(inner)),
        })
    }
}
impl MasterPty for Master {
    fn resize(&self, size: PtySize) -> Result<()> {
        let mut inner = self.0.lock().expect("ConPTY");
        let result = unsafe {
            (inner.console.1.resize)(
                inner.console.0,
                COORD {
                    X: size.cols as i16,
                    Y: size.rows as i16,
                },
            )
        };
        ensure!(
            result >= 0,
            "ResizePseudoConsole failed: HRESULT {result:#x}"
        );
        inner.size = size;
        Ok(())
    }
    fn get_size(&self) -> Result<PtySize> {
        Ok(self.0.lock().expect("ConPTY").size)
    }
    fn try_clone_reader(&self) -> Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(self.0.lock().expect("ConPTY").reader.try_clone()?))
    }
    fn take_writer(&self) -> Result<Box<dyn io::Write + Send>> {
        Ok(Box::new(
            self.0
                .lock()
                .expect("ConPTY")
                .writer
                .take()
                .context("PTY writer already taken")?,
        ))
    }
}
struct Attributes {
    storage: Vec<u128>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new() -> Result<Self> {
        let mut needed = 0;
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &mut needed) };
        let mut storage = vec![0u128; needed.div_ceil(mem::size_of::<u128>())];
        let list = storage.as_mut_ptr() as _;
        ensure!(
            unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut needed) } != 0,
            "InitializeProcThreadAttributeList: {}",
            io::Error::last_os_error()
        );
        Ok(Self { storage, list })
    }
    fn set(&mut self, key: u32, value: *const std::ffi::c_void, len: usize) -> Result<()> {
        ensure!(
            unsafe {
                UpdateProcThreadAttribute(
                    self.list,
                    0,
                    key as usize,
                    value,
                    len,
                    ptr::null_mut(),
                    ptr::null(),
                )
            } != 0,
            "UpdateProcThreadAttribute: {}",
            io::Error::last_os_error()
        );
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
        let _ = &self.storage;
    }
}

fn wide(s: &OsStr) -> Result<Vec<u16>> {
    let mut w: Vec<_> = s.encode_wide().collect();
    ensure!(!w.contains(&0), "NUL in process arguments");
    w.push(0);
    Ok(w)
}
fn quote(s: &OsStr, out: &mut Vec<u16>) -> Result<()> {
    let w: Vec<_> = s.encode_wide().collect();
    ensure!(!w.contains(&0), "NUL in process arguments");
    out.push(34);
    let mut slashes = 0;
    for c in w {
        match c {
            92 => slashes += 1,
            34 => {
                out.extend(std::iter::repeat_n(92, slashes * 2 + 1));
                out.push(34);
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n(92, slashes));
                slashes = 0;
                out.push(c);
            }
        }
    }
    out.extend(std::iter::repeat_n(92, slashes * 2));
    out.push(34);
    Ok(())
}
fn executable(cmd: &CommandBuilder) -> Result<PathBuf> {
    let program = Path::new(cmd.get_argv().first().context("Missing program")?);
    let cwd = cmd
        .get_cwd()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let candidates = if program.is_absolute() || program.components().count() > 1 {
        vec![cwd.join(program)]
    } else {
        std::iter::once(cwd)
            .chain(std::env::split_paths(
                cmd.get_env("PATH").unwrap_or(OsStr::new("")),
            ))
            .map(|p| p.join(program))
            .collect()
    };
    for p in candidates {
        let p = if p.extension().is_none() {
            p.with_extension("exe")
        } else {
            p
        };
        if p.is_file() {
            return Ok(p);
        }
    }
    anyhow::bail!("Terminal executable not found")
}
impl SlavePty for Slave {
    fn spawn_command(&self, cmd: CommandBuilder) -> Result<Box<dyn Child + Send + Sync>> {
        let inner = self.0.lock().expect("ConPTY");
        let exe = executable(&cmd)?;
        let mut command_line = Vec::new();
        quote(exe.as_os_str(), &mut command_line)?;
        for a in cmd.get_argv().iter().skip(1) {
            command_line.push(32);
            quote(a, &mut command_line)?;
        }
        command_line.push(0);
        ensure!(
            command_line.len() <= 32767,
            "Terminal command line exceeds Windows limit"
        );
        let exe = wide(exe.as_os_str())?;
        let cwd = cmd.get_cwd().map(|p| wide(p.as_os_str())).transpose()?;
        let mut env: Vec<_> = cmd.iter_full_env_as_str().collect();
        env.sort_by_key(|(k, _)| k.to_uppercase());
        let mut block = Vec::new();
        for (k, v) in env {
            block.extend(OsString::from(format!("{k}={v}")).encode_wide());
            block.push(0);
        }
        block.push(0);
        let mut attrs = Attributes::new()?;
        let jobs = [inner.job.handle()];
        attrs.set(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            inner.console.0 as _,
            mem::size_of::<HPCON>(),
        )?;
        attrs.set(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr() as _,
            mem::size_of_val(&jobs),
        )?;
        let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
        startup.StartupInfo.cb = mem::size_of_val(&startup) as _;
        startup.lpAttributeList = attrs.list;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        let mut info: PROCESS_INFORMATION = unsafe { mem::zeroed() };
        ensure!(
            unsafe {
                CreateProcessW(
                    exe.as_ptr(),
                    command_line.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                    block.as_ptr() as _,
                    cwd.as_ref().map_or(ptr::null(), |v| v.as_ptr()),
                    &startup.StartupInfo,
                    &mut info,
                )
            } != 0,
            "CreateProcessW: {}",
            io::Error::last_os_error()
        );
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess as _) };
        let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread as _) };
        // Job assignment was performed by CreateProcess. Only now release the main thread.
        if unsafe { ResumeThread(thread.as_raw_handle() as _) } == u32::MAX {
            let _ = inner.job.terminate();
            return Err(io::Error::last_os_error().into());
        }
        Ok(Box::new(Process {
            handle: Arc::new(process),
            job: inner.job.clone(),
            pid: info.dwProcessId,
        }))
    }
}
#[derive(Debug, Clone)]
struct Process {
    handle: Arc<OwnedHandle>,
    job: Arc<Job>,
    pid: u32,
}
impl ChildKiller for Process {
    fn kill(&mut self) -> io::Result<()> {
        self.job.terminate()
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}
impl Child for Process {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle() as _, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.handle.as_raw_handle() as _, &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitStatus::with_exit_code(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        if unsafe { WaitForSingleObject(self.handle.as_raw_handle() as _, INFINITE) }
            != WAIT_OBJECT_0
        {
            return Err(io::Error::last_os_error());
        }
        let _ = self.job.terminate();
        self.try_wait()?
            .ok_or_else(|| io::Error::other("Process did not exit"))
    }
    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        Some(self.handle.as_raw_handle())
    }
}
