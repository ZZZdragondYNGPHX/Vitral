use crate::{error::VitralError, ret::RetentionEngine};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs,
    io::{ErrorKind, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

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

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BackupOrder {
    timestamp: u64,
    pid: u32,
    seq: u64,
}

fn retention_backups(dir: &Path) -> std::io::Result<Vec<(BackupOrder, std::path::PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(order) = entry.file_name().to_str().and_then(retention_backup_order) {
            backups.push((order, entry.path()));
        }
    }
    Ok(backups)
}

fn retention_backup_order(name: &str) -> Option<BackupOrder> {
    let stem = name.strip_prefix("retention-")?.strip_suffix(".json")?;
    let (timestamp, pid, seq) = match stem.split_once("-p") {
        Some((timestamp, suffix)) => {
            let (pid, seq) = suffix.split_once('-')?;
            (timestamp, pid.parse().ok()?, seq.parse().ok()?)
        }
        None => (stem, 0, 0),
    };
    Some(BackupOrder {
        timestamp: timestamp.parse().ok()?,
        pid,
        seq,
    })
}

pub fn backup_count(state_dir: &Path) -> std::io::Result<usize> {
    Ok(retention_backups(&state_dir.join("backups"))?.len())
}

pub fn backup_retention(state_dir: &Path, retention: &RetentionEngine) -> std::io::Result<()> {
    backup_retention_at(state_dir, retention, RetentionEngine::now_epoch_ms())
}

fn backup_retention_at(
    state_dir: &Path,
    retention: &RetentionEngine,
    now: u64,
) -> std::io::Result<()> {
    static ROTATION: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let _rotation = ROTATION.lock();
    let dir = state_dir.join("backups");
    let mut backups = retention_backups(&dir)?;
    backups.sort_unstable();
    let timestamp = match backups.last() {
        Some((last, _)) if last.timestamp >= now => last.timestamp.checked_add(1).ok_or_else(|| {
            std::io::Error::new(ErrorKind::InvalidData, "retention backup timestamp overflow")
        })?,
        _ => now,
    };
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    save(
        &dir,
        &format!("retention-{timestamp}-p{pid}-{seq:020}.json"),
        retention,
    )?;

    let expired = backups.len().saturating_sub(4);
    for (_, path) in backups.into_iter().take(expired) {
        match fs::remove_file(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            result => result?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_retention_backups_get_distinct_timestamps() {
        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = TempDir(
            std::env::temp_dir().join(format!("vitral-retention-race-{}", std::process::id())),
        );
        let backups = dir.0.join("backups");
        save(&backups, "retention-42.json", &RetentionEngine::default()).unwrap();

        let write_concurrently = |writers| {
            let start = std::sync::Barrier::new(writers);
            std::thread::scope(|scope| {
                for writer in 0..writers {
                    let (start, dir) = (&start, &dir.0);
                    scope.spawn(move || {
                        let mut retention = RetentionEngine::default();
                        retention.pin_word(&format!("本地词{writer}"));
                        start.wait();
                        backup_retention_at(dir, &retention, 42).unwrap();
                    });
                }
            });
        };
        write_concurrently(4);
        assert_eq!(backup_count(&dir.0).unwrap(), 5);
        let mut saved = retention_backups(&backups).unwrap();
        saved.sort_unstable();
        assert!(saved.iter().map(|(order, _)| order.timestamp).eq(42..=46));
        let words: std::collections::BTreeSet<_> = saved
            .into_iter()
            .flat_map(|(_, path)| {
                load::<RetentionEngine>(&backups, path.file_name().unwrap().to_str().unwrap())
                    .unwrap()
                    .unwrap()
                    .pinned_words
            })
            .collect();
        assert_eq!(
            words,
            (0..4).map(|writer| format!("本地词{writer}")).collect()
        );
        assert!(backups.join("retention-42.json").is_file());

        write_concurrently(8);
        assert_eq!(backup_count(&dir.0).unwrap(), 5);
        assert!(!backups.join("retention-42.json").exists());

        for (now, expected) in [(54, 55), (10, 56)] {
            let mut retention = RetentionEngine::default();
            let word = format!("最新词{expected}");
            retention.pin_word(&word);
            backup_retention_at(&dir.0, &retention, now).unwrap();
            let mut saved = retention_backups(&backups).unwrap();
            saved.sort_unstable();
            assert!(saved.iter().map(|(order, _)| order.timestamp).eq(expected - 4..=expected));
            let (_, newest) = saved.last().unwrap();
            let restored = load::<RetentionEngine>(&backups, newest.file_name().unwrap().to_str().unwrap())
                .unwrap()
                .unwrap();
            assert!(restored.pinned_words.contains(&word));
        }
    }
}
