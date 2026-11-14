use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct ProcessGuard {
    /// Open or memory-mapped (st_dev, st_ino) pairs.
    busy_inodes: HashSet<(u64, u64)>,
    /// Exact open/mapped/cwd paths.
    busy_paths: HashSet<PathBuf>,
    /// Ancestor directories of open/mapped/cwd paths (so whole dirs are never trashed out from under a process).
    busy_ancestors: HashSet<PathBuf>,
    /// Lowercased running process names (comm, exe basename, argv[0..2] basenames).
    running_procs: HashSet<String>,
}

impl ProcessGuard {
    pub fn scan() -> Self {
        let mut guard = Self::default();
        let Ok(entries) = fs::read_dir("/proc") else {
            return guard;
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            if !name_str.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let pid_dir = entry.path();
            guard.inspect_pid(&pid_dir);
        }

        guard
    }

    fn inspect_pid(&mut self, pid_dir: &Path) {
        if let Ok(comm) = fs::read_to_string(pid_dir.join("comm")) {
            let trimmed = comm.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                self.running_procs.insert(trimmed);
            }
        }

        if let Ok(exe) = fs::read_link(pid_dir.join("exe")) {
            if let Some(base) = exe.file_name().and_then(|s| s.to_str()) {
                self.running_procs.insert(base.to_ascii_lowercase());
            }
        }

        if let Ok(cmdline) = fs::read(pid_dir.join("cmdline")) {
            for arg in cmdline.split(|&b| b == 0).take(2) {
                if arg.is_empty() {
                    continue;
                }
                if let Ok(s) = std::str::from_utf8(arg) {
                    if let Some(base) = Path::new(s).file_name().and_then(|b| b.to_str()) {
                        self.running_procs.insert(base.to_ascii_lowercase());
                    }
                }
            }
        }

        if let Ok(cwd) = fs::read_link(pid_dir.join("cwd")) {
            self.mark_path_busy(&cwd);
        }

        if let Ok(fds) = fs::read_dir(pid_dir.join("fd")) {
            for fd_entry in fds.flatten() {
                let fd_path = fd_entry.path();
                if let Ok(target) = fs::read_link(&fd_path) {
                    if let Some(clean) = normalize_proc_path(&target) {
                        self.mark_path_busy(&clean);
                    }
                }
                if let Ok(meta) = fs::metadata(&fd_path) {
                    self.busy_inodes.insert((meta.dev(), meta.ino()));
                }
            }
        }

        if let Ok(maps) = fs::read_to_string(pid_dir.join("maps")) {
            for line in maps.lines() {
                // Format: addr perms offset dev inode [pathname...]
                let mut parts = line.split_whitespace();
                let _addr = parts.next();
                let _perms = parts.next();
                let _offset = parts.next();
                let Some(dev_str) = parts.next() else { continue };
                let Some(ino_str) = parts.next() else { continue };

                if let Ok(ino) = ino_str.parse::<u64>() {
                    if ino != 0 {
                        if let Some(dev) = parse_proc_dev(dev_str) {
                            self.busy_inodes.insert((dev, ino));
                        }
                    }
                }

                if let Some(path_part) = parts.next() {
                    if path_part.starts_with('/') {
                        // Re-slice from original line to preserve spaces in pathnames
                        if let Some(idx) = line.find('/') {
                            let raw_path = Path::new(line[idx..].trim_end());
                            if let Some(clean) = normalize_proc_path(raw_path) {
                                self.mark_path_busy(&clean);
                            }
                        }
                    }
                }
            }
        }
    }

    fn mark_path_busy(&mut self, path: &Path) {
        if !path.is_absolute() {
            return;
        }
        self.busy_paths.insert(path.to_path_buf());
        let mut cur = path.parent();
        while let Some(p) = cur {
            if p == Path::new("/") {
                break;
            }
            if !self.busy_ancestors.insert(p.to_path_buf()) {
                break;
            }
            cur = p.parent();
        }
    }

    /// Returns true if the exact path, its inode, or any descendant inside it is open/mapped/cwd.
    pub fn is_busy(&self, path: &Path, meta: Option<&fs::Metadata>) -> bool {
        if self.busy_paths.contains(path) || self.busy_ancestors.contains(path) {
            return true;
        }
        if let Some(m) = meta {
            if self.busy_inodes.contains(&(m.dev(), m.ino())) {
                return true;
            }
        }
        false
    }

    /// Returns true if any process whose name matches one of `names` is currently running.
    pub fn is_any_proc_running(&self, names: &[String]) -> bool {
        names.iter().any(|n| {
            let lower = n.to_ascii_lowercase();
            self.running_procs.contains(&lower)
        })
    }
}

fn normalize_proc_path(raw: &Path) -> Option<PathBuf> {
    let s = raw.to_str()?;
    if !s.starts_with('/') {
        return None;
    }
    let trimmed = s.strip_suffix(" (deleted)").unwrap_or(s);
    Some(PathBuf::from(trimmed))
}

fn parse_proc_dev(dev_str: &str) -> Option<u64> {
    let (maj_s, min_s) = dev_str.split_once(':')?;
    let maj = u32::from_str_radix(maj_s, 16).ok()?;
    let min = u32::from_str_radix(min_s, 16).ok()?;
    Some(libc::makedev(maj, min) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn detects_open_file_and_parent_dir() {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sweep_proc_test_{}", uniq));
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("active.dat");
        let _open_handle = File::create(&file_path).unwrap();

        let guard = ProcessGuard::scan();
        let meta = fs::symlink_metadata(&file_path).unwrap();
        assert!(guard.is_busy(&file_path, Some(&meta)));
        assert!(guard.is_busy(&dir, None));

        drop(_open_handle);
        let _ = fs::remove_dir_all(&dir);
    }
}
