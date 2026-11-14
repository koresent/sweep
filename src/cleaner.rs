use crate::config::{Config, XdgPaths};
use crate::proc_guard::ProcessGuard;
use crate::trash::{TrashBin, TrashOutcome, entry_disk_bytes};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct CandidateItem {
    pub category: String,
    pub path: String,
    pub size_bytes: u64,
    pub age_days: u64,
    pub action: String, // "trashed" | "would_trash" | "skipped_busy" | "skipped_limit" | "error"
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolHookResult {
    pub name: String,
    pub status: String, // "ran" | "would_run" | "skipped_busy" | "skipped_missing" | "error"
    pub freed_bytes: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedGroup {
    pub category: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepReport {
    pub timestamp_unix: i64,
    pub dry_run: bool,
    pub total_reclaimed_bytes: u64,
    pub total_items: usize,
    pub skipped_busy_count: usize,
    pub error_count: usize,
    pub tool_hooks: Vec<ToolHookResult>,
    pub items: Vec<CandidateItem>,
    pub skipped_groups: Vec<SkippedGroup>,
}

pub struct Cleaner<'a> {
    config: &'a Config,
    paths: &'a XdgPaths,
    proc_guard: &'a ProcessGuard,
    trash: Option<&'a mut TrashBin>,
    dry_run: bool,
    now: i64,
    home_dev: Option<u64>,
    uid: u32,
    excludes: Vec<PathBuf>,
    report: SweepReport,
}

impl<'a> Cleaner<'a> {
    pub fn new(
        config: &'a Config,
        paths: &'a XdgPaths,
        proc_guard: &'a ProcessGuard,
        trash: Option<&'a mut TrashBin>,
        dry_run: bool,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let home_dev = fs::metadata(&paths.home).ok().map(|m| m.dev());
        let uid = unsafe { libc::getuid() };

        let mut excludes = vec![
            paths.trash_dir.clone(),
            paths.home.join(".local/bin"),
            paths.home.join(".local/lib"),
            paths.home.join(".local/src"),
            paths.config_dir.clone(),
        ];
        for raw in &config.general.exclude_paths {
            excludes.push(paths.expand_tilde(raw));
        }

        Self {
            config,
            paths,
            proc_guard,
            trash,
            dry_run,
            now,
            home_dev,
            uid,
            excludes,
            report: SweepReport {
                timestamp_unix: now,
                dry_run,
                total_reclaimed_bytes: 0,
                total_items: 0,
                skipped_busy_count: 0,
                error_count: 0,
                tool_hooks: Vec::new(),
                items: Vec::new(),
                skipped_groups: Vec::new(),
            },
        }
    }

    pub fn run_all(mut self) -> SweepReport {
        self.run_tool_hooks();
        if self.config.paru.enabled {
            self.clean_paru();
        }
        if self.config.cache.enabled {
            self.clean_xdg_cache();
        }
        if self.config.state_and_temp.enabled {
            self.clean_state_and_temp();
        }
        if self.config.projects.enabled {
            self.clean_projects();
        }
        self.report
    }

    fn run_tool_hooks(&mut self) {
        if self.config.tools.uv_prune {
            self.hook_uv_prune();
        }
        if self.config.tools.journal_vacuum {
            self.hook_journal_vacuum();
        }
    }

    fn hook_uv_prune(&mut self) {
        let uv_cache = self.paths.cache_dir.join("uv");
        if !uv_cache.is_dir() || !command_exists("uv") {
            return;
        }
        if self
            .proc_guard
            .is_any_proc_running(&["uv".to_string(), "uvx".to_string()])
        {
            self.report.tool_hooks.push(ToolHookResult {
                name: "uv cache prune".into(),
                status: "skipped_busy".into(),
                freed_bytes: 0,
                detail: "uv process is currently running".into(),
            });
            return;
        }

        let before = entry_disk_bytes(&uv_cache);
        if self.dry_run {
            self.report.tool_hooks.push(ToolHookResult {
                name: "uv cache prune".into(),
                status: "would_run".into(),
                freed_bytes: 0,
                detail: format!(
                    "current cache size {}; will prune dangling entries only",
                    format_bytes(before)
                ),
            });
            return;
        }

        match Command::new("uv").args(["cache", "prune"]).output() {
            Ok(out) if out.status.success() => {
                let after = entry_disk_bytes(&uv_cache);
                let freed = before.saturating_sub(after);
                self.report.total_reclaimed_bytes =
                    self.report.total_reclaimed_bytes.saturating_add(freed);
                self.report.tool_hooks.push(ToolHookResult {
                    name: "uv cache prune".into(),
                    status: "ran".into(),
                    freed_bytes: freed,
                    detail: format!(
                        "{} -> {} (freed {})",
                        format_bytes(before),
                        format_bytes(after),
                        format_bytes(freed)
                    ),
                });
            }
            Ok(out) => {
                self.report.error_count += 1;
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                self.report.tool_hooks.push(ToolHookResult {
                    name: "uv cache prune".into(),
                    status: "error".into(),
                    freed_bytes: 0,
                    detail: err,
                });
            }
            Err(e) => {
                self.report.error_count += 1;
                self.report.tool_hooks.push(ToolHookResult {
                    name: "uv cache prune".into(),
                    status: "error".into(),
                    freed_bytes: 0,
                    detail: e.to_string(),
                });
            }
        }
    }

    fn hook_journal_vacuum(&mut self) {
        if !command_exists("journalctl") {
            return;
        }
        let time_arg = format!("--vacuum-time={}d", self.config.tools.journal_max_days);
        let size_arg = format!("--vacuum-size={}M", self.config.tools.journal_max_size_mb);

        if self.dry_run {
            let detail = match Command::new("journalctl")
                .args(["--user", "--disk-usage"])
                .output()
            {
                Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
                Err(_) => "would vacuum user journal".into(),
            };
            self.report.tool_hooks.push(ToolHookResult {
                name: "journalctl --user vacuum".into(),
                status: "would_run".into(),
                freed_bytes: 0,
                detail,
            });
            return;
        }

        match Command::new("journalctl")
            .args(["--user", &time_arg, &size_arg])
            .output()
        {
            Ok(out) if out.status.success() => {
                let msg = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .last()
                    .unwrap_or("vacuum completed")
                    .trim()
                    .to_string();
                self.report.tool_hooks.push(ToolHookResult {
                    name: "journalctl --user vacuum".into(),
                    status: "ran".into(),
                    freed_bytes: 0,
                    detail: msg,
                });
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                self.report.tool_hooks.push(ToolHookResult {
                    name: "journalctl --user vacuum".into(),
                    status: "error".into(),
                    freed_bytes: 0,
                    detail: err,
                });
            }
            Err(e) => {
                self.report.tool_hooks.push(ToolHookResult {
                    name: "journalctl --user vacuum".into(),
                    status: "error".into(),
                    freed_bytes: 0,
                    detail: e.to_string(),
                });
            }
        }
    }

    fn clean_paru(&mut self) {
        let clone_dir = self.paths.cache_dir.join("paru").join("clone");
        if !clone_dir.is_dir() {
            return;
        }
        if self.proc_guard.is_any_proc_running(&[
            "paru".into(),
            "pacman".into(),
            "makepkg".into(),
        ]) {
            self.report.skipped_groups.push(SkippedGroup {
                category: "paru".into(),
                path: clone_dir.display().to_string(),
                reason: "paru/pacman/makepkg is running".into(),
            });
            return;
        }

        let installed = query_installed_packages();
        let Ok(entries) = fs::read_dir(&clone_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let pkg_dir = entry.path();
            let Ok(meta) = fs::symlink_metadata(&pkg_dir) else {
                continue;
            };
            if !meta.file_type().is_dir() || self.is_excluded(&pkg_dir) {
                continue;
            }
            if has_protection_marker(&pkg_dir) {
                continue;
            }

            let pkg_name = entry.file_name().to_string_lossy().to_string();
            let summary = self.inspect_dir(&pkg_dir);

            if summary.has_busy {
                self.report.skipped_busy_count += 1;
                self.report.skipped_groups.push(SkippedGroup {
                    category: "paru".into(),
                    path: pkg_dir.display().to_string(),
                    reason: "file in use by process".into(),
                });
                continue;
            }

            // If package is uninstalled and entire clone dir is very old -> trash whole clone dir
            if let Some(ref pkgs) = installed {
                if !pkgs.contains(&pkg_name)
                    && self.is_older_than(summary.newest_ts, self.config.paru.orphan_clone_days)
                {
                    let age_days = self.age_days(summary.newest_ts);
                    self.trash_candidate(
                        "paru-orphan",
                        &pkg_dir,
                        summary.total_bytes,
                        age_days,
                        "uninstalled AUR clone",
                    );
                    continue;
                }
            }

            // Otherwise keep .git, PKGBUILD, .SRCINFO; trash stale build/archive artifacts
            let Ok(children) = fs::read_dir(&pkg_dir) else {
                continue;
            };
            for child in children.flatten() {
                let child_path = child.path();
                let name = child.file_name().to_string_lossy().to_string();
                if matches!(
                    name.as_str(),
                    ".git" | "PKGBUILD" | ".SRCINFO" | ".gitignore" | ".SRCINFO.bak"
                ) {
                    continue;
                }
                if self.is_excluded(&child_path) {
                    continue;
                }

                let Ok(cmeta) = fs::symlink_metadata(&child_path) else {
                    continue;
                };
                if cmeta.file_type().is_dir() {
                    let csum = self.inspect_dir(&child_path);
                    if csum.has_busy {
                        self.report.skipped_busy_count += 1;
                        continue;
                    }
                    if self.is_older_than(csum.newest_ts, self.config.paru.artifact_days) {
                        let age_days = self.age_days(csum.newest_ts);
                        self.trash_candidate(
                            "paru-artifact",
                            &child_path,
                            csum.total_bytes,
                            age_days,
                            "AUR build directory",
                        );
                    }
                } else {
                    if self.proc_guard.is_busy(&child_path, Some(&cmeta)) {
                        self.report.skipped_busy_count += 1;
                        continue;
                    }
                    let ts = effective_ts(&cmeta, self.now);
                    if self.is_older_than(ts, self.config.paru.artifact_days)
                        && is_paru_artifact_file(&name)
                    {
                        let size = file_disk_bytes(&cmeta);
                        let age_days = self.age_days(ts);
                        self.trash_candidate(
                            "paru-artifact",
                            &child_path,
                            size,
                            age_days,
                            "AUR build archive/package",
                        );
                    }
                }
            }
        }
    }

    fn clean_xdg_cache(&mut self) {
        let cache_dir = self.paths.cache_dir.clone();
        if !cache_dir.is_dir() {
            return;
        }

        let Ok(entries) = fs::read_dir(&cache_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if self.is_excluded(&path) {
                continue;
            }
            if self
                .config
                .cache
                .protected_dirs
                .iter()
                .any(|p| p == &name)
            {
                continue;
            }
            // uv is managed safely via `uv cache prune` when enabled (avoids breaking hardlinked venvs)
            if name == "uv" && self.config.tools.uv_prune {
                continue;
            }

            if let Some(procs) = self.config.cache.process_locked.get(&name) {
                if self.proc_guard.is_any_proc_running(procs) {
                    self.report.skipped_groups.push(SkippedGroup {
                        category: "cache".into(),
                        path: path.display().to_string(),
                        reason: format!("app process running ({})", procs.join("/")),
                    });
                    continue;
                }
            }

            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };

            if !meta.file_type().is_dir() {
                if self.proc_guard.is_busy(&path, Some(&meta)) || is_lock_or_runtime_file(&name) {
                    if self.proc_guard.is_busy(&path, Some(&meta)) {
                        self.report.skipped_busy_count += 1;
                    }
                    continue;
                }
                let ts = effective_ts(&meta, self.now);
                if self.is_older_than(ts, self.config.cache.default_days) {
                    let size = file_disk_bytes(&meta);
                    self.trash_candidate(
                        "cache",
                        &path,
                        size,
                        self.age_days(ts),
                        "stale root cache file",
                    );
                }
                continue;
            }

            if !self.same_home_dev(&meta) || has_protection_marker(&path) {
                continue;
            }

            let days = self
                .config
                .cache
                .dir_days
                .get(&name)
                .copied()
                .unwrap_or(self.config.cache.default_days);

            // Special scoped rules for sensitive app caches
            if name == "zen" {
                self.clean_zen_cache(&path, days);
                continue;
            }
            if name == "vicinae-extensions-src" {
                self.clean_vicinae_extensions_src(&path, days);
                continue;
            }

            self.clean_dir_stale_files("cache", &path, days, &format!("cache/{}", name));
        }
    }

    fn clean_zen_cache(&mut self, zen_cache_root: &Path, days: u64) {
        let Ok(profiles) = fs::read_dir(zen_cache_root) else {
            return;
        };
        for prof in profiles.flatten() {
            let prof_path = prof.path();
            if !prof_path.is_dir() {
                continue;
            }
            for sub in ["cache2", "startupCache", "thumbnails", "OfflineCache"] {
                let target = prof_path.join(sub);
                if target.is_dir() && !has_protection_marker(&target) {
                    self.clean_dir_stale_files("cache-browser", &target, days, "zen browser cache");
                }
            }
        }
    }

    fn clean_vicinae_extensions_src(&mut self, root: &Path, days: u64) {
        let ext_dir = root.join("extensions");
        if !ext_dir.is_dir() {
            return;
        }
        let Ok(entries) = fs::read_dir(&ext_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let nm = entry.path().join("node_modules");
            if !nm.is_dir() {
                continue;
            }
            let sum = self.inspect_dir(&nm);
            if sum.has_busy {
                self.report.skipped_busy_count += 1;
                continue;
            }
            if self.is_older_than(sum.newest_ts, days) {
                self.trash_candidate(
                    "cache-vicinae",
                    &nm,
                    sum.total_bytes,
                    self.age_days(sum.newest_ts),
                    "stale extension build node_modules",
                );
            }
        }
    }

    fn clean_state_and_temp(&mut self) {
        let state_cfg = self.config.state_and_temp.clone();

        // 1. Old logs / crash reports in configured ~/.local/state subdirectories
        let state_dir = self.paths.state_dir.clone();
        for dir_name in &state_cfg.state_log_dirs {
            let sub = state_dir.join(dir_name);
            if !sub.is_dir() || self.is_excluded(&sub) || has_protection_marker(&sub) {
                continue;
            }
            let is_trace_dir = dir_name == "playwright-mcp";
            let max_days = if is_trace_dir {
                state_cfg.crash_and_trace_days
            } else {
                state_cfg.log_days
            };
            self.clean_logs_and_crashes(&sub, max_days, is_trace_dir);
        }

        // 2. Old crash dumps / logs anywhere in ~/.cache
        if self.paths.cache_dir.is_dir() {
            self.clean_cache_crashes(&self.paths.cache_dir.clone(), state_cfg.crash_and_trace_days);
        }

        // 3. User-owned files in /var/tmp
        let var_tmp = Path::new("/var/tmp");
        if var_tmp.is_dir() {
            self.clean_vartmp(var_tmp, state_cfg.vartmp_days);
        }
    }

    fn clean_logs_and_crashes(&mut self, dir: &Path, max_days: u64, allow_all_files: bool) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.is_excluded(&path) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();

            if meta.file_type().is_dir() {
                if !self.same_home_dev(&meta) || has_protection_marker(&path) {
                    continue;
                }
                if is_crash_dir_name(&name) {
                    let sum = self.inspect_dir(&path);
                    if !sum.has_busy && self.is_older_than(sum.newest_ts, self.config.state_and_temp.crash_and_trace_days) {
                        self.trash_candidate(
                            "crash-dump",
                            &path,
                            sum.total_bytes,
                            self.age_days(sum.newest_ts),
                            "crash/dump directory",
                        );
                    }
                } else {
                    self.clean_logs_and_crashes(&path, max_days, allow_all_files);
                }
                continue;
            }

            if self.proc_guard.is_busy(&path, Some(&meta)) || is_lock_or_runtime_file(&name) {
                if self.proc_guard.is_busy(&path, Some(&meta)) {
                    self.report.skipped_busy_count += 1;
                }
                continue;
            }

            if allow_all_files || is_log_or_dump_file(&name) {
                let ts = effective_ts(&meta, self.now);
                if self.is_older_than(ts, max_days) {
                    self.trash_candidate(
                        "state-log",
                        &path,
                        file_disk_bytes(&meta),
                        self.age_days(ts),
                        "old log/trace artifact",
                    );
                }
            }
        }
    }

    fn clean_cache_crashes(&mut self, root: &Path, crash_days: u64) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_dir() || !self.same_home_dev(&meta) {
                continue;
            }
            let Ok(sub_entries) = fs::read_dir(&path) else {
                continue;
            };
            for sub in sub_entries.flatten() {
                let sp = sub.path();
                let sname = sub.file_name().to_string_lossy().to_string();
                if sp.is_dir() && is_crash_dir_name(&sname) && !self.is_excluded(&sp) {
                    let sum = self.inspect_dir(&sp);
                    if !sum.has_busy && self.is_older_than(sum.newest_ts, crash_days) {
                        self.trash_candidate(
                            "crash-dump",
                            &sp,
                            sum.total_bytes,
                            self.age_days(sum.newest_ts),
                            "old crash reports",
                        );
                    }
                }
            }
        }
    }

    fn clean_vartmp(&mut self, var_tmp: &Path, max_days: u64) {
        let Ok(entries) = fs::read_dir(var_tmp) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.is_excluded(&path) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.uid() != self.uid {
                continue;
            }
            let ft = meta.file_type();
            if ft.is_socket() || ft.is_fifo() || ft.is_block_device() || ft.is_char_device() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("systemd-private-") || is_lock_or_runtime_file(&name) {
                continue;
            }

            if ft.is_dir() {
                let sum = self.inspect_dir(&path);
                if sum.has_busy {
                    self.report.skipped_busy_count += 1;
                    continue;
                }
                if self.is_older_than(sum.newest_ts, max_days) {
                    self.trash_candidate(
                        "var-tmp",
                        &path,
                        sum.total_bytes,
                        self.age_days(sum.newest_ts),
                        "old user /var/tmp dir",
                    );
                }
            } else {
                if self.proc_guard.is_busy(&path, Some(&meta)) {
                    self.report.skipped_busy_count += 1;
                    continue;
                }
                let ts = effective_ts(&meta, self.now);
                if self.is_older_than(ts, max_days) {
                    self.trash_candidate(
                        "var-tmp",
                        &path,
                        file_disk_bytes(&meta),
                        self.age_days(ts),
                        "old user /var/tmp file",
                    );
                }
            }
        }
    }

    fn clean_projects(&mut self) {
        let proj_cfg = self.config.projects.clone();
        for raw_root in &proj_cfg.roots {
            let root = self.paths.expand_tilde(raw_root);
            if root.is_dir() {
                self.walk_projects(&root, 0, &proj_cfg);
            }
        }
    }

    fn walk_projects(
        &mut self,
        dir: &Path,
        depth: usize,
        cfg: &crate::config::ProjectsConfig,
    ) {
        if depth > cfg.max_depth || self.is_excluded(dir) || has_protection_marker(dir) {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(dir) else {
            return;
        };
        if !meta.file_type().is_dir() || !self.same_home_dev(&meta) {
            return;
        }

        let is_cargo_proj = dir.join("Cargo.toml").is_file();
        let is_node_proj = dir.join("package.json").is_file();
        let is_py_proj = dir.join("pyproject.toml").is_file() || dir.join("requirements.txt").is_file();

        // Check if this directory itself is a project root; if so, evaluate its activity once
        let project_newest_ts = if is_cargo_proj || is_node_proj || (cfg.clean_venvs && is_py_proj) {
            Some(self.project_source_newest_ts(dir))
        } else {
            None
        };

        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(cmeta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !cmeta.file_type().is_dir() || self.is_excluded(&path) || has_protection_marker(&path) {
                continue;
            }
            if matches!(name.as_str(), ".git" | ".archive" | ".hg" | ".svn") {
                continue;
            }

            // Lightweight Python caches anywhere inside workspaces
            if matches!(
                name.as_str(),
                "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache"
            ) {
                let sum = self.inspect_dir(&path);
                if sum.has_busy {
                    self.report.skipped_busy_count += 1;
                    continue;
                }
                if self.is_older_than(sum.newest_ts, cfg.light_cache_days) {
                    self.trash_candidate(
                        "project-cache",
                        &path,
                        sum.total_bytes,
                        self.age_days(sum.newest_ts),
                        "stale Python cache",
                    );
                }
                continue;
            }

            // Heavy build directories (only if anchored by a project manifest and whole project is inactive)
            let is_heavy_rust = name == "target" && is_cargo_proj;
            let is_heavy_node = name == "node_modules" && is_node_proj;
            let is_heavy_venv = cfg.clean_venvs && matches!(name.as_str(), ".venv" | "venv") && is_py_proj;

            if is_heavy_rust || is_heavy_node || is_heavy_venv {
                let sum = self.inspect_dir(&path);
                if sum.has_busy || self.proc_guard.is_busy(dir, None) {
                    self.report.skipped_busy_count += 1;
                    continue;
                }
                let ref_ts = project_newest_ts.unwrap_or(sum.newest_ts).max(sum.newest_ts);
                if self.is_older_than(ref_ts, cfg.heavy_build_days) {
                    self.trash_candidate(
                        "project-build",
                        &path,
                        sum.total_bytes,
                        self.age_days(ref_ts),
                        "inactive project build artifacts",
                    );
                }
                continue;
            }

            self.walk_projects(&path, depth + 1, cfg);
        }
    }

    fn clean_dir_stale_files(&mut self, category: &str, dir: &Path, max_days: u64, note: &str) {
        if self.is_excluded(dir) || has_protection_marker(dir) {
            return;
        }
        let sum = self.inspect_dir(dir);
        if !sum.has_busy
            && sum.total_bytes > 0
            && self.is_older_than(sum.newest_ts, max_days)
            && !self.contains_protected_files(dir)
        {
            // Whole subdirectory is stale and untouched by any process -> trash in one atomic move!
            self.trash_candidate(
                category,
                dir,
                sum.total_bytes,
                self.age_days(sum.newest_ts),
                note,
            );
            return;
        }

        // Mixed freshness or busy descendant: walk children selectively
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.is_excluded(&path) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();

            if meta.file_type().is_dir() {
                if !self.same_home_dev(&meta) || has_protection_marker(&path) {
                    continue;
                }
                self.clean_dir_stale_files(category, &path, max_days, note);
                // Prune empty directory left behind if old enough
                if !self.dry_run && is_dir_empty(&path) && !self.proc_guard.is_busy(&path, Some(&meta)) {
                    let ts = effective_ts(&meta, self.now);
                    if self.is_older_than(ts, 0) {
                        let _ = fs::remove_dir(&path);
                    }
                }
            } else {
                if self.proc_guard.is_busy(&path, Some(&meta)) {
                    self.report.skipped_busy_count += 1;
                    continue;
                }
                if is_lock_or_runtime_file(&name) || matches!(name.as_str(), "CACHEDIR.TAG") {
                    continue;
                }
                let ts = effective_ts(&meta, self.now);
                if self.is_older_than(ts, max_days) {
                    self.trash_candidate(
                        category,
                        &path,
                        file_disk_bytes(&meta),
                        self.age_days(ts),
                        note,
                    );
                }
            }
        }
    }

    fn contains_protected_files(&self, dir: &Path) -> bool {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(cur) = stack.pop() {
            let Ok(rd) = fs::read_dir(&cur) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if self.is_excluded(&p) || is_lock_or_runtime_file(&name) || name == "CACHEDIR.TAG" {
                    return true;
                }
                if let Ok(m) = fs::symlink_metadata(&p) {
                    if m.file_type().is_dir() {
                        if has_protection_marker(&p) {
                            return true;
                        }
                        stack.push(p);
                    }
                }
            }
        }
        false
    }

    fn inspect_dir(&self, dir: &Path) -> DirSummary {
        let mut newest_ts = fs::symlink_metadata(dir)
            .ok()
            .map(|m| effective_ts(&m, self.now))
            .unwrap_or(self.now);
        let mut has_busy = self.proc_guard.is_busy(dir, None);
        let mut total_bytes: u64 = 0;

        let mut stack = vec![dir.to_path_buf()];
        while let Some(cur) = stack.pop() {
            let Ok(rd) = fs::read_dir(&cur) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                let Ok(m) = fs::symlink_metadata(&p) else {
                    continue;
                };
                if self.proc_guard.is_busy(&p, Some(&m)) {
                    has_busy = true;
                }
                let ts = effective_ts(&m, self.now);
                if ts > newest_ts {
                    newest_ts = ts;
                }
                if m.file_type().is_dir() {
                    if self.same_home_dev(&m) {
                        stack.push(p);
                    }
                } else {
                    total_bytes = total_bytes.saturating_add(file_disk_bytes(&m));
                }
            }
        }

        DirSummary {
            newest_ts,
            has_busy,
            total_bytes,
        }
    }

    fn project_source_newest_ts(&self, proj_dir: &Path) -> i64 {
        let mut newest_ts = fs::symlink_metadata(proj_dir)
            .ok()
            .map(|m| effective_ts(&m, self.now))
            .unwrap_or(self.now);

        let mut stack = vec![proj_dir.to_path_buf()];
        while let Some(cur) = stack.pop() {
            let Ok(rd) = fs::read_dir(&cur) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if matches!(
                    name.as_str(),
                    "target"
                        | "node_modules"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | ".pytest_cache"
                        | ".mypy_cache"
                        | ".ruff_cache"
                ) {
                    continue;
                }
                let Ok(m) = fs::symlink_metadata(&p) else {
                    continue;
                };
                if name == ".git" {
                    // Check .git/index and .git/HEAD for recent commits/checkouts
                    for gfile in ["index", "HEAD"] {
                        if let Ok(gm) = fs::symlink_metadata(p.join(gfile)) {
                            newest_ts = newest_ts.max(effective_ts(&gm, self.now));
                        }
                    }
                    continue;
                }
                newest_ts = newest_ts.max(effective_ts(&m, self.now));
                if m.file_type().is_dir() && self.same_home_dev(&m) {
                    stack.push(p);
                }
            }
        }
        newest_ts
    }

    fn trash_candidate(
        &mut self,
        category: &str,
        path: &Path,
        size_bytes: u64,
        age_days: u64,
        note: &str,
    ) {
        if self.dry_run {
            self.report.total_reclaimed_bytes =
                self.report.total_reclaimed_bytes.saturating_add(size_bytes);
            self.report.total_items += 1;
            self.report.items.push(CandidateItem {
                category: category.into(),
                path: path.display().to_string(),
                size_bytes,
                age_days,
                action: "would_trash".into(),
                note: note.into(),
            });
            return;
        }

        let Some(trash) = self.trash.as_deref_mut() else {
            return;
        };

        match trash.move_to_trash(path, size_bytes) {
            Ok(TrashOutcome::Trashed) => {
                self.report.total_reclaimed_bytes =
                    self.report.total_reclaimed_bytes.saturating_add(size_bytes);
                self.report.total_items += 1;
                self.report.items.push(CandidateItem {
                    category: category.into(),
                    path: path.display().to_string(),
                    size_bytes,
                    age_days,
                    action: "trashed".into(),
                    note: note.into(),
                });
            }
            Ok(TrashOutcome::SkippedLimit(reason)) => {
                self.report.items.push(CandidateItem {
                    category: category.into(),
                    path: path.display().to_string(),
                    size_bytes,
                    age_days,
                    action: "skipped_limit".into(),
                    note: reason,
                });
            }
            Err(err) => {
                self.report.error_count += 1;
                self.report.items.push(CandidateItem {
                    category: category.into(),
                    path: path.display().to_string(),
                    size_bytes,
                    age_days,
                    action: "error".into(),
                    note: err,
                });
            }
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.excludes.iter().any(|ex| path.starts_with(ex))
    }

    fn same_home_dev(&self, meta: &fs::Metadata) -> bool {
        self.home_dev.map_or(true, |d| d == meta.dev())
    }

    fn is_older_than(&self, ts: i64, days: u64) -> bool {
        let min_secs = (self.config.general.min_grace_hours as i64) * 3600;
        let days_secs = (days as i64).saturating_mul(86400);
        let threshold = min_secs.max(days_secs);
        self.now.saturating_sub(ts) >= threshold
    }

    fn age_days(&self, ts: i64) -> u64 {
        self.now.saturating_sub(ts).max(0) as u64 / 86400
    }
}

struct DirSummary {
    newest_ts: i64,
    has_busy: bool,
    total_bytes: u64,
}

/// On noatime filesystems (like CachyOS Btrfs), atime is frozen at creation time;
/// taking max(mtime, ctime, valid_atime) always yields the safest last-touched timestamp.
fn effective_ts(meta: &fs::Metadata, now: i64) -> i64 {
    let mtime = meta.mtime();
    let ctime = meta.ctime();
    let atime = meta.atime();
    let mut best = mtime.max(ctime);
    if atime > 0 && atime <= now + 60 {
        best = best.max(atime);
    }
    best
}

fn file_disk_bytes(meta: &fs::Metadata) -> u64 {
    let blocks = meta.blocks() * 512;
    if blocks > 0 { blocks } else { meta.len() }
}

fn has_protection_marker(dir: &Path) -> bool {
    dir.join(".nosweep").exists() || dir.join(".noclean").exists() || dir.join(".keep").exists()
}

fn is_dir_empty(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(false)
}

fn is_lock_or_runtime_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), ".lock" | "lock" | "parentlock" | ".parentlock")
        || lower.starts_with("singleton")
        || lower.ends_with(".lock")
        || lower.ends_with(".lck")
        || lower.ends_with(".pid")
        || lower.ends_with(".sock")
        || lower.ends_with("-wal")
        || lower.ends_with("-shm")
        || lower.ends_with(".part")
        || lower.ends_with(".crdownload")
}

fn is_paru_artifact_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains(".pkg.tar.")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".tar.zst")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".zip")
        || lower.ends_with(".deb")
        || lower.ends_with(".rpm")
        || lower.ends_with(".appimage")
        || lower.ends_with(".whl")
}

fn is_crash_dir_name(name: &str) -> bool {
    matches!(
        name,
        "Crash Reports" | "Crashpad" | "crashes" | "coredumps" | "dumps"
    )
}

fn is_log_or_dump_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".log")
        || lower.contains(".log.")
        || lower.ends_with(".dmp")
        || lower.ends_with(".crash")
        || lower.ends_with(".old")
}

fn command_exists(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file()
    })
}

fn query_installed_packages() -> Option<HashSet<String>> {
    let out = Command::new("pacman").arg("-Qq").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().map(|l| l.trim().to_string()).collect())
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_bytes, has_protection_marker, is_crash_dir_name, is_dir_empty,
        is_lock_or_runtime_file, is_log_or_dump_file, is_paru_artifact_file,
    };
    use std::fs;

    #[test]
    fn detects_runtime_and_lock_files() {
        assert!(is_lock_or_runtime_file(".lock"));
        assert!(is_lock_or_runtime_file("app.pid"));
        assert!(is_lock_or_runtime_file("socket.sock"));
        assert!(is_lock_or_runtime_file("database.db-wal"));
        assert!(is_lock_or_runtime_file("download.part"));
        assert!(is_lock_or_runtime_file("file.crdownload"));
        assert!(!is_lock_or_runtime_file("document.txt"));
        assert!(!is_lock_or_runtime_file("archive.tar.gz"));
    }

    #[test]
    fn detects_paru_build_artifacts() {
        assert!(is_paru_artifact_file("linux-cachyos-6.6.pkg.tar.zst"));
        assert!(is_paru_artifact_file("source.tar.gz"));
        assert!(is_paru_artifact_file("package.whl"));
        assert!(is_paru_artifact_file("app.AppImage"));
        assert!(!is_paru_artifact_file("PKGBUILD"));
        assert!(!is_paru_artifact_file("patch.diff"));
    }

    #[test]
    fn detects_crash_and_log_files() {
        assert!(is_crash_dir_name("Crashpad"));
        assert!(is_crash_dir_name("coredumps"));
        assert!(!is_crash_dir_name("cache"));

        assert!(is_log_or_dump_file("system.log"));
        assert!(is_log_or_dump_file("app.log.1"));
        assert!(is_log_or_dump_file("minidump.dmp"));
        assert!(!is_log_or_dump_file("logger.rs"));
    }

    #[test]
    fn detects_protection_markers() {
        let temp_dir = std::env::temp_dir().join(format!("test_sweep_protection_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        assert!(!has_protection_marker(&temp_dir));
        assert!(is_dir_empty(&temp_dir));

        fs::write(temp_dir.join(".nosweep"), "").unwrap();
        assert!(has_protection_marker(&temp_dir));
        assert!(!is_dir_empty(&temp_dir));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn formats_bytes_correctly() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1048576 * 10), "10.0 MiB");
        assert_eq!(format_bytes(1073741824 * 3), "3.00 GiB");
    }
}
