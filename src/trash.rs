use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct TrashBin {
    files_dir: PathBuf,
    info_dir: PathBuf,
    trash_dev: Option<u64>,
    max_cross_device_bytes: u64,
    max_trash_total_bytes: u64,
    current_trash_bytes: u64,
}

#[derive(Debug)]
pub enum TrashOutcome {
    Trashed,
    SkippedLimit(String),
}

impl TrashBin {
    pub fn new(
        trash_root: &Path,
        max_cross_device_mb: u64,
        max_trash_total_gb: u64,
    ) -> Result<Self, String> {
        let files_dir = trash_root.join("files");
        let info_dir = trash_root.join("info");

        fs::create_dir_all(&files_dir)
            .map_err(|e| format!("failed to create {}: {}", files_dir.display(), e))?;
        fs::create_dir_all(&info_dir)
            .map_err(|e| format!("failed to create {}: {}", info_dir.display(), e))?;

        let trash_dev = fs::metadata(&files_dir).ok().map(|m| m.dev());
        let current_trash_bytes = if max_trash_total_gb > 0 {
            entry_disk_bytes(&files_dir)
        } else {
            0
        };

        Ok(Self {
            files_dir,
            info_dir,
            trash_dev,
            max_cross_device_bytes: max_cross_device_mb.saturating_mul(1024 * 1024),
            max_trash_total_bytes: max_trash_total_gb.saturating_mul(1024 * 1024 * 1024),
            current_trash_bytes,
        })
    }

    pub fn move_to_trash(&mut self, path: &Path, size_bytes: u64) -> Result<TrashOutcome, String> {
        if self.max_trash_total_bytes > 0
            && self.current_trash_bytes.saturating_add(size_bytes) > self.max_trash_total_bytes
        {
            return Ok(TrashOutcome::SkippedLimit(format!(
                "trash cap ({:.1} GiB) would be exceeded",
                self.max_trash_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
            )));
        }

        let meta = fs::symlink_metadata(path)
            .map_err(|e| format!("stat failed for {}: {}", path.display(), e))?;

        let same_dev = self.trash_dev.is_some_and(|d| d == meta.dev());
        if same_dev {
            match self.atomic_trash(path) {
                Ok(()) => {
                    self.current_trash_bytes = self.current_trash_bytes.saturating_add(size_bytes);
                    return Ok(TrashOutcome::Trashed);
                }
                Err(AtomicErr::Exdev) => {
                    // Fall through to cross-device trash-put
                }
                Err(AtomicErr::Other(msg)) => return Err(msg),
            }
        }

        if self.max_cross_device_bytes > 0 && size_bytes > self.max_cross_device_bytes {
            return Ok(TrashOutcome::SkippedLimit(format!(
                "cross-device item exceeds {} MiB limit",
                self.max_cross_device_bytes / (1024 * 1024)
            )));
        }

        let status = Command::new("trash-put")
            .arg("--")
            .arg(path)
            .status()
            .map_err(|e| format!("failed to run trash-put for {}: {}", path.display(), e))?;

        if status.success() {
            self.current_trash_bytes = self.current_trash_bytes.saturating_add(size_bytes);
            Ok(TrashOutcome::Trashed)
        } else {
            Err(format!(
                "trash-put exited with {} for {}",
                status,
                path.display()
            ))
        }
    }

    fn atomic_trash(&self, path: &Path) -> Result<(), AtomicErr> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| AtomicErr::Other(e.to_string()))?
                .join(path)
        };

        let base_name = abs
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty() && *n != "." && *n != "..")
            .ok_or_else(|| AtomicErr::Other(format!("invalid filename: {}", abs.display())))?;

        let deletion_date = format_local_iso8601();
        let encoded_path = percent_encode_path(&abs);
        let trashinfo_body = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            encoded_path, deletion_date
        );

        let mut counter: u32 = 0;
        loop {
            let candidate_name = if counter == 0 {
                base_name.to_string()
            } else {
                format!("{}.{}", base_name, counter)
            };

            let dest_file = self.files_dir.join(&candidate_name);
            let dest_info = self.info_dir.join(format!("{}.trashinfo", candidate_name));

            if dest_file.symlink_metadata().is_ok() {
                counter += 1;
                continue;
            }

            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest_info)
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(trashinfo_body.as_bytes()) {
                        let _ = fs::remove_file(&dest_info);
                        return Err(AtomicErr::Other(format!(
                            "failed writing {}: {}",
                            dest_info.display(),
                            e
                        )));
                    }
                    drop(f);

                    match fs::rename(&abs, &dest_file) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                            let _ = fs::remove_file(&dest_info);
                            return Err(AtomicErr::Exdev);
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&dest_info);
                            return Err(AtomicErr::Other(format!(
                                "rename {} -> {} failed: {}",
                                abs.display(),
                                dest_file.display(),
                                e
                            )));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    counter += 1;
                }
                Err(e) => {
                    return Err(AtomicErr::Other(format!(
                        "failed creating {}: {}",
                        dest_info.display(),
                        e
                    )));
                }
            }
        }
    }
}

enum AtomicErr {
    Exdev,
    Other(String),
}

pub fn entry_disk_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.file_type().is_dir() {
        let blocks = meta.blocks() * 512;
        return if blocks > 0 { blocks } else { meta.len() };
    }
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(m) = fs::symlink_metadata(&p) else {
                continue;
            };
            if m.file_type().is_dir() {
                stack.push(p);
            } else {
                let blocks = m.blocks() * 512;
                total = total.saturating_add(if blocks > 0 { blocks } else { m.len() });
            }
        }
    }
    total
}

fn format_local_iso8601() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn percent_encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn atomic_trash_moves_and_writes_valid_trashinfo() {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sweep_trash_test_{}", uniq));
        let trash_dir = root.join("Trash");
        let src = root.join("old file.log");
        fs::create_dir_all(&root).unwrap();
        fs::write(&src, b"hello").unwrap();

        let mut bin = TrashBin::new(&trash_dir, 64, 0).unwrap();
        let outcome = bin.move_to_trash(&src, 5).unwrap();
        assert!(matches!(outcome, TrashOutcome::Trashed));
        assert!(!src.exists());
        assert!(trash_dir.join("files/old file.log").exists());

        let info = fs::read_to_string(trash_dir.join("info/old file.log.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath="));
        assert!(info.contains("old%20file.log"));
        assert!(info.contains("DeletionDate="));

        let _ = fs::remove_dir_all(&root);
    }
}
