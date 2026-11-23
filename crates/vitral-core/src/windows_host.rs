//! One input engine per Windows user session. TSF and the settings window share its state.
use crate::{commands, VitralCore};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
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
    fn GetModuleHandleExW(
        flags: u32,
        address: *const u16,
        module: *mut HMODULE,
    ) -> windows_sys::core::BOOL;
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
            reap_pipe_io as *const () as *const u16,
            &mut module,
        ) != 0
        {
            let param = Box::into_raw(Box::new(PipeReaper { pending, module }));
            let thread = CreateThread(
                null_mut(),
                0,
                Some(reap_pipe_io),
                param.cast(),
                0,
                null_mut(),
            );
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

struct PipeConnection {
    file: File,
    io: Box<PipeIo>,
}

enum TransferError {
    Failed(io::Error),
    DisconnectedBeforeWrite(io::Error),
    Cancelling(io::Error),
}

fn pipe_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Vitral pipe request timed out")
}

impl PipeConnection {
    fn new(file: File) -> io::Result<Self> {
        let event = unsafe { CreateEventW(null_mut(), 1, 0, null_mut()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            io: Box::new(PipeIo {
                overlapped: unsafe { std::mem::zeroed() },
                data: Vec::new(),
                event: unsafe { OwnedHandle::from_raw_handle(event) },
            }),
        })
    }

    fn transfer(
        &mut self,
        data: &mut [u8],
        writing: bool,
        deadline: Instant,
    ) -> std::result::Result<(), TransferError> {
        let event = self.io.event.as_raw_handle();
        let handle = self.file.as_raw_handle();
        let mut offset = 0;
        unsafe {
            while offset < data.len() {
                if Instant::now() >= deadline {
                    return Err(TransferError::Failed(pipe_timeout()));
                }
                ResetEvent(event);
                let overlapped = &mut self.io.overlapped;
                *overlapped = OVERLAPPED {
                    hEvent: event,
                    ..std::mem::zeroed()
                };
                let remaining_data = &mut data[offset..];
                let ok = if writing {
                    WriteFile(
                        handle,
                        remaining_data.as_ptr(),
                        remaining_data.len() as u32,
                        null_mut(),
                        overlapped,
                    )
                } else {
                    ReadFile(
                        handle,
                        remaining_data.as_mut_ptr(),
                        remaining_data.len() as u32,
                        null_mut(),
                        overlapped,
                    )
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    let code = error.raw_os_error().unwrap_or_default() as u32;
                    if code != ERROR_IO_PENDING {
                        // Only an immediate rejection of the first write proves no bytes
                        // were sent. A queued write or a failed read may have executed.
                        if writing
                            && offset == 0
                            && matches!(
                                code,
                                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA
                            )
                        {
                            return Err(TransferError::DisconnectedBeforeWrite(error));
                        }
                        return Err(TransferError::Failed(error));
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let wait = WaitForSingleObject(
                        event,
                        remaining.as_millis().min((INFINITE - 1) as u128) as u32,
                    );
                    if wait != WAIT_OBJECT_0 {
                        let error = if wait == WAIT_TIMEOUT {
                            pipe_timeout()
                        } else {
                            io::Error::last_os_error()
                        };
                        CancelIoEx(handle, overlapped);
                        return Err(TransferError::Cancelling(error));
                    }
                }
                let mut count = 0;
                if GetOverlappedResult(handle, overlapped, &mut count, 0) == 0 {
                    return Err(TransferError::Failed(io::Error::last_os_error()));
                }
                if count == 0 {
                    return Err(TransferError::Failed(io::Error::new(
                        if writing {
                            io::ErrorKind::WriteZero
                        } else {
                            io::ErrorKind::UnexpectedEof
                        },
                        "Vitral pipe closed",
                    )));
                }
                offset += count as usize;
            }
        }
        Ok(())
    }
}

fn connect_to_host(host: &Path, deadline: Instant) -> Result<PipeConnection> {
    let mut launched = false;
    let mut connect_deadline = deadline.min(Instant::now() + Duration::from_millis(1500));
    loop {
        if Instant::now() >= connect_deadline {
            return Err(pipe_timeout().into());
        }
        match connect() {
            Ok(file) => return Ok(PipeConnection::new(file)?),
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
    }
}

#[derive(Default)]
pub struct PipeClient {
    connection: Option<PipeConnection>,
}

impl PipeClient {
    fn transfer(
        &mut self,
        data: &mut Vec<u8>,
        writing: bool,
        deadline: Instant,
    ) -> std::result::Result<(), TransferError> {
        let result = self
            .connection
            .as_mut()
            .unwrap()
            .transfer(data, writing, deadline);
        if result.is_err() {
            let PipeConnection { file, mut io } = self.connection.take().unwrap();
            drop(file);
            if matches!(&result, Err(TransferError::Cancelling(_))) {
                io.data = std::mem::take(data);
                reap_cancelled_io(io);
            }
        }
        result
    }

    fn read_reply(&mut self, deadline: Instant) -> Result<Value> {
        let mut header = vec![0; 4];
        self.transfer(&mut header, false, deadline)
            .map_err(TransferError::into_io_error)?;
        let len = u32::from_le_bytes(header.try_into().unwrap()) as usize;
        if len > 32 * 1024 * 1024 {
            return Err("message exceeds 32 MiB".into());
        }
        let mut data = vec![0; len];
        self.transfer(&mut data, false, deadline)
            .map_err(TransferError::into_io_error)?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn request_with_timeout(
        &mut self,
        host: &Path,
        editor: &str,
        cmd: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid pipe timeout"))?;
        if Instant::now() >= deadline {
            return Err(pipe_timeout().into());
        }
        let mut frame = vec![0; 4];
        serde_json::to_writer(&mut frame, &(editor, cmd, args))?;
        let len = frame.len() - 4;
        if len > 32 * 1024 * 1024 {
            return Err("message exceeds 32 MiB".into());
        }
        frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
        self.exchange(&mut frame, deadline, || connect_to_host(host, deadline))
    }

    fn exchange(
        &mut self,
        frame: &mut Vec<u8>,
        deadline: Instant,
        mut connect: impl FnMut() -> Result<PipeConnection>,
    ) -> Result<Value> {
        if self.connection.is_none() {
            self.connection = Some(connect()?);
        }
        match self.transfer(frame, true, deadline) {
            Err(TransferError::DisconnectedBeforeWrite(_)) => {
                self.connection = Some(connect()?);
                self.transfer(frame, true, deadline)
                    .map_err(TransferError::into_io_error)?;
            }
            result => result.map_err(TransferError::into_io_error)?,
        }
        let mut reply = self.read_reply(deadline).inspect_err(|_| {
            self.connection = None;
        })?;
        if let Some(e) = reply["error"].as_str() {
            return Err(e.to_string().into());
        }
        Ok(reply.get_mut("result").map_or(Value::Null, Value::take))
    }
}

impl TransferError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::Failed(error)
            | Self::DisconnectedBeforeWrite(error)
            | Self::Cancelling(error) => error,
        }
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
    thread_local! {
        static CLIENT: RefCell<PipeClient> = const { RefCell::new(PipeClient { connection: None }) };
    }
    CLIENT.with(|client| {
        client
            .borrow_mut()
            .request_with_timeout(host, editor, cmd, args, timeout)
    })
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
            while let Ok((id, cmd, args)) = read_frame::<(String, String, Value)>(&mut client) {
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
                if write_frame(&mut client, &response).is_err() {
                    break;
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
        let error = request_with_timeout(
            Path::new("unused"),
            "",
            "profile-state",
            &json!({}),
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
    }

    fn test_pipe(label: &str) -> (File, PipeConnection) {
        let name = format!(r"\\.\pipe\Vitral.io-test.{}.{label}", unsafe {
            GetCurrentProcessId()
        },);
        let server = unsafe {
            let pipe = CreateNamedPipeW(
                wide(&name).as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64,
                64,
                0,
                null_mut(),
            );
            assert_ne!(pipe, INVALID_HANDLE_VALUE);
            File::from_raw_handle(pipe)
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(name)
            .unwrap();
        // Opening the client first already connects this pipe instance.
        let connected = unsafe { ConnectNamedPipe(server.as_raw_handle(), null_mut()) };
        assert!(
            connected != 0
                || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32)
        );
        (server, PipeConnection::new(file).unwrap())
    }

    #[test]
    fn pending_pipe_read_times_out_and_releases_the_client() {
        let (_server, connection) = test_pipe("timeout");
        let mut client = PipeClient {
            connection: Some(connection),
        };
        let error = client
            .transfer(
                &mut vec![0; 4],
                false,
                Instant::now() + Duration::from_millis(100),
            )
            .unwrap_err()
            .into_io_error();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(client.connection.is_none());
    }

    #[test]
    fn reconnects_only_before_the_first_write_and_reuses_the_connection() {
        let (stale_server, stale) = test_pipe("stale");
        drop(stale_server);
        let (mut server, replacement) = test_pipe("replacement");
        let worker = std::thread::spawn(move || {
            for expected in 1..=3 {
                assert_eq!(read_frame::<i32>(&mut server).unwrap(), expected);
                if expected != 3 {
                    write_frame(&mut server, &json!({"result": expected})).unwrap();
                }
            }
            // The third request ran, then the server disconnected without a reply.
        });
        let mut client = PipeClient {
            connection: Some(stale),
        };
        let event = replacement.io.event.as_raw_handle();
        let mut replacement = Some(replacement);
        let mut connect = || Ok(replacement.take().expect("unexpected reconnect"));
        for n in 1..=3 {
            let mut frame = vec![1, 0, 0, 0, b'0' + n];
            let result = client.exchange(
                &mut frame,
                Instant::now() + Duration::from_secs(5),
                &mut connect,
            );
            if n == 3 {
                assert!(result.is_err());
                assert!(client.connection.is_none());
            } else {
                assert_eq!(result.unwrap(), json!(n));
                assert_eq!(
                    client.connection.as_ref().unwrap().io.event.as_raw_handle(),
                    event
                );
            }
        }
        worker.join().unwrap();
    }
}
