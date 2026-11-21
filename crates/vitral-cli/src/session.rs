// Each state directory owns one in-memory engine. Only the socket is stored on disk.
use vitral_core::commands::{execute, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use vitral_core::VitralCore;

fn socket_path(dir: &File) -> PathBuf {
    // A procfs descriptor keeps the socket address within sockaddr_un even for long paths.
    PathBuf::from(format!("/proc/self/fd/{}/engine.sock", dir.as_raw_fd()))
}

fn worker(dir: &File, state: &Path) -> Result<()> {
    let listener = UnixListener::bind(socket_path(dir))?;
    println!("ready");
    io::stdout().flush()?;
    let mut core = None;
    loop {
        let mut fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to one pollfd for the live listener.
        let ready = unsafe { libc::poll(&mut fd, 1, 5000) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if ready == 0 {
            if !socket_path(dir).exists() {
                break;
            }
            continue;
        }
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut stop = false;
        let result = (|| -> Result<Value> {
            let (cmd, v): (String, Value) = serde_json::from_reader(&stream)?;
            match cmd.as_str() {
                "init" => {
                    let mut cfg = v["config"]
                        .as_object()
                        .ok_or("expected config object")?
                        .clone();
                    cfg.entry("state_dir").or_insert_with(|| json!(state));
                    core = Some(VitralCore::init(&Value::Object(cfg).to_string())?);
                    Ok(json!({"ok":true}))
                }
                "shutdown" => {
                    stop = true;
                    Ok(json!({"ok":true}))
                }
                _ => execute(core.as_ref().ok_or("engine is not initialized")?, &cmd, &v),
            }
        })();
        let reply = match result {
            Ok(value) => json!({"result":value}),
            Err(e) => json!({"error":e.to_string()}),
        };
        if stop || core.is_none() {
            fs::remove_file(socket_path(dir))?;
        }
        // A disconnected caller must not terminate another active session.
        let _ = serde_json::to_writer(&mut stream, &reply);
        if stop || core.is_none() {
            break;
        }
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    let state = Path::new(args.get(1).ok_or("missing state directory")?);
    let private = state.join(".vitral-session");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&private)?;
    let meta = fs::symlink_metadata(&private)?;
    // SAFETY: geteuid takes no arguments.
    if !meta.is_dir() || meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
        return Err("session directory must be private and owned by the current user".into());
    }
    let dir = File::open(&private)?;
    if args[0] == "--session-worker" {
        return worker(&dir, state);
    }
    let cmd = args.get(2).ok_or("missing session command")?;
    let data: Value = serde_json::from_reader(io::stdin().lock())?;
    // Serialize startup, stale-socket recovery and shutdown across competing clients.
    // SAFETY: dir is open until the client has read its reply.
    if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let socket = socket_path(&dir);
    let mut stream = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e)
            if cmd == "init"
                && matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
        {
            if let Ok(m) = fs::symlink_metadata(&socket) {
                if !m.file_type().is_socket() {
                    return Err("session path occupied".into());
                }
                fs::remove_file(&socket)?;
            }
            let mut child = Command::new(std::env::current_exe()?)
                .arg("--session-worker")
                .arg(state)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            let mut line = String::new();
            BufReader::new(child.stdout.take().ok_or("missing startup pipe")?)
                .read_line(&mut line)?;
            if line.trim() != "ready" {
                return Err(format!("session failed to start: {}", child.wait()?).into());
            }
            UnixStream::connect(&socket)?
        }
        Err(e) => return Err(format!("engine session unavailable: {e}").into()),
    };
    serde_json::to_writer(&mut stream, &(cmd, data))?;
    stream.shutdown(Shutdown::Write)?;
    let reply: Value = serde_json::from_reader(stream)?;
    if let Some(e) = reply["error"].as_str() {
        return Err(e.to_string().into());
    }
    println!("{}", reply.get("result").ok_or("missing result")?);
    Ok(())
}
