//! One input engine per Windows user session. TSF and the settings window share its state.
use crate::{commands, VitralCore};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
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
    System::{Pipes::*, RemoteDesktop::ProcessIdToSessionId, Threading::*},
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
pub fn request(host: &Path, editor: &str, cmd: &str, args: &Value) -> Result<Value> {
    let mut launched = false;
    let start = Instant::now();
    let mut file = loop {
        match connect() {
            Ok(f) => break f,
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) =>
            {
                if start.elapsed() > Duration::from_secs(5) {
                    return Err(e.into());
                }
                if !launched && e.kind() == io::ErrorKind::NotFound {
                    std::process::Command::new(host).arg("--engine").spawn()?;
                    launched = true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.into()),
        }
    };
    write_frame(&mut file, &(editor, cmd, args))?;
    let reply: Value = read_frame(&mut file)?;
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
