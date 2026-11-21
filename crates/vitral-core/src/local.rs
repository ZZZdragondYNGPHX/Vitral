use crate::error::VitralError;
use serde::{de::DeserializeOwned, Serialize};
use std::{fs, io::Write, path::Path};

pub fn load<T: DeserializeOwned>(dir: &Path, name: &str) -> Result<Option<T>, VitralError> {
    match fs::File::open(dir.join(name)) {
        Ok(f) => serde_json::from_reader(f)
            .map(Some)
            .map_err(|e| VitralError::Parse(format!("{name}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(VitralError::Io(e.to_string())),
    }
}

pub fn save<T: Serialize>(dir: &Path, name: &str, data: &T) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        ".{name}.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        let mut writer = std::io::BufWriter::new(&mut file);
        serde_json::to_writer(&mut writer, data)?;
        writer.flush()?;
        drop(writer);
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
