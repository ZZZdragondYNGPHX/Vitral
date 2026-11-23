//! One input engine per Windows user session. TSF and the settings window share its state.
use crate::{commands, VitralCore};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{Pipes::*, RemoteDesktop::ProcessIdToSessionId, Threading::*, IO::*},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn sid_string(sid: PSID) -> io::Result<String> {
    unsafe {
        let mut ptr = null_mut();
        if ConvertSidToStringSidW(sid, &mut ptr) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut n = 0;
        while *ptr.add(n) != 0 {
            n += 1
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, n));
        LocalFree(ptr.cast());
        Ok(result)
    }
}
fn identity() -> io::Result<&'static (String, Vec<u16>)> {
    static ID: OnceLock<(String, Vec<u16>)> = OnceLock::new();
    if let Some(id) = ID.get() {
        return Ok(id);
    }
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = std::os::windows::io::OwnedHandle::from_raw_handle(token);
        let mut len = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut len);
        let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sid = sid_string((*(buf.as_ptr().cast::<TOKEN_USER>())).User.Sid)?;
        let mut session = 0;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session) == 0 {
            return Err(io::Error::last_os_error());
        }
        let pipe = wide(&format!(r"\\.\pipe\Vitral.{sid}.{session}"));
        Ok(ID.get_or_init(|| (sid, pipe)))
    }
}
pub fn settings_path() -> io::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|p| PathBuf::from(p).join("Vitral/settings.json"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA"))
}
fn instance(first: bool) -> io::Result<File> {
    let (sid, path) = identity()?;
    let acl = wide(&format!("O:{sid}D:P(A;;GA;;;{sid})"));
    unsafe {
        let mut sd = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            acl.as_ptr(),
            1,
            &mut sd,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as _,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let flags = PIPE_ACCESS_DUPLEX
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        let h = CreateNamedPipeW(
            path.as_ptr(),
            flags,
            PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            1000,
            &sa,
        );
        let err = io::Error::last_os_error();
        LocalFree(sd);
        if h == INVALID_HANDLE_VALUE {
            return Err(err);
        }
        Ok(File::from_raw_handle(h))
    }
}
fn connect() -> io::Result<File> {
    let (sid, name) = identity()?;
    use std::os::windows::ffi::OsStringExt;
    let name = std::ffi::OsString::from_wide(&name[..name.len() - 1]);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(name)?;
    unsafe {
        let mut owner = null_mut();
        let mut sd = null_mut();
        let rc = GetSecurityInfo(
            file.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as _));
        }
        let result = sid_string(owner);
        LocalFree(sd);
        if &result? != sid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unexpected Vitral pipe owner",
            ));
        }
    }
    Ok(file)
}
fn write_frame(file: &mut File, value: &impl serde::Serialize) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > 32 * 1024 * 1024 {
        return Err("message exceeds 32 MiB".into());
    }
    file.write_all(&(data.len() as u32).to_le_bytes())?;
    file.write_all(&data)?;
    Ok(())
}
fn read_frame<T: serde::de::DeserializeOwned>(file: &mut File) -> Result<T> {
    let mut len = [0; 4];
    file.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > 32 * 1024 * 1024 {
        return Err("message exceeds 32 MiB".into());
    }
    let mut data = vec![0; len];
    file.read_exact(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}
// The buffer and OVERLAPPED must outlive cancellation, including after a timeout.
struct PipeIo {
    overlapped: OVERLAPPED,
    data: Vec<u8>,
    event: OwnedHandle,
}
// hEvent belongs to event; no pointers reference another thread's stack.
unsafe impl Send for PipeIo {}
// LibraryLoader is only needed for the overlapped I/O reaper in this module.
#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut HMODULE) -> windows_sys::core::BOOL;
    fn FreeLibraryAndExitThread(module: HMODULE, exit_code: u32) -> !;
}
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 4;

struct PipeReaper {
    pending: Box<PipeIo>,
    module: HMODULE,
}

unsafe extern "system" fn reap_pipe_io(param: *mut std::ffi::c_void) -> u32 {
    let reaper = unsafe { Box::from_raw(param.cast::<PipeReaper>()) };
    unsafe { WaitForSingleObject(reaper.pending.event.as_raw_handle(), INFINITE) };
    let module = reaper.module;
    drop(reaper);
    // Release the DLL reference without returning into its code.
    unsafe { FreeLibraryAndExitThread(module, 0) }
}

fn reap_cancelled_io(pending: Box<PipeIo>) {
    unsafe {
        let mut module = null_mut();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            transfer as *const () as *const u16,
            &mut module,
        ) != 0
        {
            let param = Box::into_raw(Box::new(PipeReaper { pending, module }));
            let thread = CreateThread(null_mut(), 0, Some(reap_pipe_io), param.cast(), 0, null_mut());
            if !thread.is_null() {
                CloseHandle(thread);
                return;
            }
            let reaper = Box::from_raw(param);
            WaitForSingleObject(reaper.pending.event.as_raw_handle(), INFINITE);
            drop(reaper);
            FreeLibrary(module);
        } else {
            WaitForSingleObject(pending.event.as_raw_handle(), INFINITE);
        }
    }
}

fn transfer(
    file: &mut Option<File>,
    data: Vec<u8>,
    writing: bool,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    unsafe {
        let event = CreateEventW(null_mut(), 1, 0, null_mut());
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut pending = Box::new(PipeIo {
            overlapped: OVERLAPPED {
                hEvent: event,
                ..std::mem::zeroed()
            },
            data,
            event: OwnedHandle::from_raw_handle(event),
        });
        let handle = file.as_ref().unwrap().as_raw_handle();
        let mut offset = 0;
        while offset < pending.data.len() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Vitral pipe request timed out",
                ));
            }
            ResetEvent(event);
            pending.overlapped = OVERLAPPED {
                hEvent: event,
                ..std::mem::zeroed()
            };
            let data = &mut pending.data[offset..];
            let mut count = 0;
            let ok = if writing {
                WriteFile(
                    handle,
                    data.as_ptr(),
                    data.len() as u32,
                    null_mut(),
                    &mut pending.overlapped,
                )
            } else {
                ReadFile(
                    handle,
                    data.as_mut_ptr(),
                    data.len() as u32,
                    null_mut(),
                    &mut pending.overlapped,
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(error);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wait =
                    WaitForSingleObject(event, remaining.as_millis().min((INFINITE - 1) as u128) as u32);
                if wait != WAIT_OBJECT_0 {
                    let error = if wait == WAIT_TIMEOUT {
                        io::Error::new(io::ErrorKind::TimedOut, "Vitral pipe request timed out")
                    } else {
                        io::Error::last_os_error()
                    };
                    CancelIoEx(handle, &pending.overlapped);
                    drop(file.take());
                    reap_cancelled_io(pending);
                    return Err(error);
                }
            }
            if GetOverlappedResult(handle, &pending.overlapped, &mut count, 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                return Err(io::Error::new(
                    if writing {
                        io::ErrorKind::WriteZero
                    } else {
                        io::ErrorKind::UnexpectedEof
                    },
                    "Vitral pipe closed",
                ));
            }
            offset += count as usize;
        }
        Ok(std::mem::take(&mut pending.data))
    }
}
pub fn request(host: &Path, editor: &str, cmd: &str, args: &Value) -> Result<Value> {
    request_with_timeout(host, editor, cmd, args, Duration::from_secs(120))
}

pub fn request_with_timeout(
    host: &Path,
    editor: &str,
    cmd: &str,
    args: &Value,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid pipe timeout"))?;
    let mut launched = false;
    let mut connect_deadline = deadline.min(Instant::now() + Duration::from_millis(1500));
    let mut file = Some(loop {
        if Instant::now() >= connect_deadline {
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "Vitral pipe request timed out").into(),
            );
        }
        match connect() {
            Ok(f) => break f,
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) =>
            {
                if !launched && e.kind() == io::ErrorKind::NotFound {
                    std::process::Command::new(host).arg("--engine").spawn()?;
                    launched = true;
                    connect_deadline = deadline.min(Instant::now() + Duration::from_secs(5));
                }
                std::thread::sleep(
                    Duration::from_millis(25)
                        .min(connect_deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(e) => return Err(e.into()),
        }
    });
    let data = serde_json::to_vec(&(editor, cmd, args))?;
    if data.len() > 32 * 1024 * 1024 {
        return Err("message exceeds 32 MiB".into());
    }
    let mut frame = (data.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&data);
    transfer(&mut file, frame, true, deadline)?;
    let header = transfer(&mut file, vec![0; 4], false, deadline)?;
    let len = u32::from_le_bytes(header.try_into().unwrap()) as usize;
    if len > 32 * 1024 * 1024 {
        return Err("message exceeds 32 MiB".into());
    }
    let data = transfer(&mut file, vec![0; len], false, deadline)?;
    let reply: Value = serde_json::from_slice(&data)?;
    if let Some(e) = reply["error"].as_str() {
        return Err(e.to_string().into());
    }
    Ok(reply["result"].clone())
}
pub fn serve() -> Result<()> {
    let mut pipe = instance(true)?;
    let path = settings_path()?;
    let mut config: Value = match fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let data =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA")?).join("Vitral");
    fs::create_dir_all(&data)?;
    let id_path = data.join("device-id");
    let device = match fs::read_to_string(&id_path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut bytes = [0; 16];
            getrandom::getrandom(&mut bytes).map_err(|e| e.to_string())?;
            let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            fs::write(id_path, &id)?;
            id
        }
        Err(e) => return Err(e.into()),
    };
    config["device_id"] = json!(device);
    config["device_name"] =
        json!(std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows".into()));
    config["data_dir"] = json!(data);
    config["assets_dir"] = json!(std::env::current_exe()?
        .parent()
        .ok_or("install directory")?
        .join("assets"));
    config["background_sync"] = json!(true);
    config["platform"] = json!("windows");
    let core = Arc::new(VitralCore::init(&config.to_string())?);
    crate::windows_clipboard::start(core.clone());
    let poll = core.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3));
        let _ = poll.poll_services();
    });
    let active = Arc::new(Mutex::new(String::new()));
    loop {
        if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), null_mut()) } == 0
            && io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32)
        {
            pipe = instance(false)?;
            continue;
        }
        let mut client = std::mem::replace(&mut pipe, instance(false)?);
        let core = core.clone();
        let owner = active.clone();
        std::thread::spawn(move || {
            if let Ok((id, cmd, args)) = read_frame::<(String, String, Value)>(&mut client) {
                let result = (|| -> Result<Value> {
                    if id.is_empty() {
                        return commands::execute(&core, &cmd, &args);
                    }
                    let mut active = owner.lock().map_err(|_| "editor lock")?;
                    if cmd == "on-ime-open" {
                        if *active != id {
                            core.process_key(27, 0, crate::eng::SwipeDir::None)?;
                            core.on_ime_close();

                            *active = id;
                        }
                        let focus = core.on_app_focus(args["app_id"].as_str().unwrap_or(""))?;
                        let mut opened = serde_json::to_value(core.on_ime_open())?;
                        opened["lang_mode"] = serde_json::to_value(focus.lang_mode)?;
                        return Ok(opened);
                    } else if *active != id {
                        return Err("editor focus changed".into());
                    }
                    if cmd == "on-ime-close" {
                        active.clear();
                    }
                    if cmd == "clipboard-push" {
                        let payload = serde_json::from_value(json!({"content":args["payload"]}))?;
                        return Ok(serde_json::to_value(core.clipboard_push_local(payload))?);
                    }
                    commands::execute(&core, &cmd, &args)
                })();
                let response = match result {
                    Ok(v) => json!({"result":v}),
                    Err(e) => json!({"error":e.to_string()}),
                };
                let _ = write_frame(&mut client, &response);
                unsafe {
                    FlushFileBuffers(client.as_raw_handle());
                }
            }
        });
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_deadline_does_not_connect_or_launch_the_engine() {
        let error = request_with_timeout(Path::new("unused"), "", "profile-state", &json!({}), Duration::ZERO)
            .unwrap_err();
        assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn pending_pipe_read_times_out_and_releases_the_client() {
        let name = format!(
            r"\\.\pipe\Vitral.io-test.{}",
            unsafe { GetCurrentProcessId() },
        );
        let server = unsafe {
            let pipe = CreateNamedPipeW(
                wide(&name).as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_REJECT_REMOTE_CLIENTS, 1,
                64, 64, 0, null_mut(),
            );
            assert_ne!(pipe, INVALID_HANDLE_VALUE);
            File::from_raw_handle(pipe)
        };
        let mut client = Some(OpenOptions::new().read(true).write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED).open(name).unwrap());
        // Opening the client first already connects this pipe instance.
        let connected = unsafe { ConnectNamedPipe(server.as_raw_handle(), null_mut()) };
        assert!(connected != 0 || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32));
        let error = transfer(&mut client, vec![0; 4], false, Instant::now() + Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(client.is_none());
    }
}
