use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub tools: ToolsConfig,
    pub cache: CacheConfig,
    pub paru: ParuConfig,
    pub state_and_temp: StateTempConfig,
    pub projects: ProjectsConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Minimum age in hours before any file/dir can be considered for cleaning.
    pub min_grace_hours: u64,
    /// Maximum size (MiB) moved across filesystems (e.g. /var/tmp -> ~/.local/share/Trash) per item.
    pub max_cross_device_mb: u64,
    /// Maximum total size (GiB) allowed in Trash before sweep skips adding huge items (0 = unlimited).
    pub max_trash_total_gb: u64,
    /// Extra paths (directories or prefixes) that sweep must never touch.
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Run `uv cache prune` when `uv` is installed and not currently running.
    pub uv_prune: bool,
    /// Run `journalctl --user --vacuum-time=...` to cap user systemd logs.
    pub journal_vacuum: bool,
    pub journal_max_days: u64,
    pub journal_max_size_mb: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    /// Default age in days for generic entries in ~/.cache.
    pub default_days: u64,
    /// Per-subdirectory age overrides in days (key = top-level dir under ~/.cache).
    pub dir_days: HashMap<String, u64>,
    /// Subdirectories under ~/.cache handled specially or skipped from generic walk.
    pub protected_dirs: Vec<String>,
    /// App caches that must not be touched while matching process names are running.
    pub process_locked: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ParuConfig {
    pub enabled: bool,
    /// Trash build artifacts (src/, pkg/, *.pkg.tar.*, archives) older than this many days.
    pub artifact_days: u64,
    /// Trash entire clone dir for packages that are no longer installed and untouched for this many days.
    pub orphan_clone_days: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StateTempConfig {
    pub enabled: bool,
    /// Age in days for old log files in ~/.local/state and ~/.cache.
    pub log_days: u64,
    /// Age in days for crash dumps / playwright-mcp traces / temp outputs.
    pub crash_and_trace_days: u64,
    /// Age in days for user-owned files in /var/tmp.
    pub vartmp_days: u64,
    /// Directories inside ~/.local/state whose logs/traces may be pruned (never touches history/db).
    pub state_log_dirs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectsConfig {
    pub enabled: bool,
    /// Root directories to scan for project build artifacts.
    pub roots: Vec<String>,
    pub max_depth: usize,
    /// Age in days for lightweight caches (__pycache__, .pytest_cache, .ruff_cache, .mypy_cache).
    pub light_cache_days: u64,
    /// Inactivity age in days (based on project sources) for heavy dirs (target/, node_modules/).
    pub heavy_build_days: u64,
    /// Also trash stale .venv / venv directories when project is inactive (disabled by default).
    pub clean_venvs: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            min_grace_hours: 24,
            max_cross_device_mb: 512,
            max_trash_total_gb: 30,
            exclude_paths: vec![],
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            uv_prune: true,
            journal_vacuum: true,
            journal_max_days: 21,
            journal_max_size_mb: 256,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        let mut dir_days = HashMap::new();
        dir_days.insert("zen".into(), 14);
        dir_days.insert("thumbnails".into(), 30);
        dir_days.insert("go-build".into(), 30);
        dir_days.insert("pip".into(), 30);
        dir_days.insert("npm".into(), 30);
        dir_days.insert("yarn".into(), 30);
        dir_days.insert("pnpm".into(), 30);
        dir_days.insert("bun".into(), 30);
        dir_days.insert("typescript".into(), 30);
        dir_days.insert("ms-playwright".into(), 45);
        dir_days.insert("ms-playwright-mcp".into(), 14);
        dir_days.insert("noctalia-widgets-target".into(), 45);
        dir_days.insert("sweep-target".into(), 45);
        dir_days.insert("nv".into(), 60);
        dir_days.insert("mesa_shader_cache".into(), 60);
        dir_days.insert("mesa_shader_cache_db".into(), 60);
        dir_days.insert("fontconfig".into(), 90);
        dir_days.insert("huggingface".into(), 60);
        dir_days.insert("torch".into(), 60);
        dir_days.insert("uv".into(), 90);

        let mut process_locked = HashMap::new();
        process_locked.insert(
            "zen".into(),
            vec!["zen".into(), "zen-bin".into(), "zen-browser".into()],
        );
        process_locked.insert("vicinae".into(), vec!["vicinae".into()]);
        process_locked.insert(
            "vicinae-extensions-src".into(),
            vec!["vicinae".into(), "npm".into(), "node".into()],
        );
        process_locked.insert("uv".into(), vec!["uv".into()]);
        process_locked.insert("pip".into(), vec!["pip".into(), "python".into(), "python3".into()]);
        process_locked.insert("npm".into(), vec!["npm".into(), "node".into()]);
        process_locked.insert("go-build".into(), vec!["go".into()]);
        process_locked.insert(
            "ms-playwright".into(),
            vec!["playwright".into(), "chromium".into(), "chrome".into(), "firefox".into()],
        );
        process_locked.insert(
            "ms-playwright-mcp".into(),
            vec!["playwright".into(), "node".into(), "chromium".into()],
        );
        process_locked.insert("noctalia-widgets-target".into(), vec!["cargo".into(), "rustc".into()]);
        process_locked.insert("sweep-target".into(), vec!["cargo".into(), "rustc".into()]);
        process_locked.insert("nvim".into(), vec!["nvim".into()]);

        Self {
            enabled: true,
            default_days: 30,
            dir_days,
            protected_dirs: vec![
                "paru".into(),        // handled by dedicated paru cleaner
                "system-updates".into(),
                "waydroid-script".into(),
            ],
            process_locked,
        }
    }
}

impl Default for ParuConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            artifact_days: 14,
            orphan_clone_days: 90,
        }
    }
}

impl Default for StateTempConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_days: 21,
            crash_and_trace_days: 7,
            vartmp_days: 14,
            state_log_dirs: vec![
                "playwright-mcp".into(),
                "noctalia".into(),
                "vicinae".into(),
                "ghostty".into(),
                "zen".into(),
                "streamdeck_ui".into(),
                "nvim".into(),
            ],
        }
    }
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: vec!["~/Projects".into()],
            max_depth: 4,
            light_cache_days: 14,
            heavy_build_days: 60,
            clean_venvs: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            tools: ToolsConfig::default(),
            cache: CacheConfig::default(),
            paru: ParuConfig::default(),
            state_and_temp: StateTempConfig::default(),
            projects: ProjectsConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct XdgPaths {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub trash_dir: PathBuf,
}

impl XdgPaths {
    pub fn discover() -> Result<Self, String> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME environment variable is not set".to_string())?;

        let config_dir = env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let cache_dir = env_path("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
        let data_dir = env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let state_dir = env_path("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let trash_dir = data_dir.join("Trash");

        Ok(Self {
            home,
            config_dir,
            cache_dir,
            state_dir,
            trash_dir,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("sweep").join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("sweep").join("last_run.json")
    }

    pub fn expand_tilde(&self, raw: &str) -> PathBuf {
        if raw == "~" {
            self.home.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(raw)
        }
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn load_config(path: &Path) -> Result<Config, String> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;
    toml::from_str(&content)
        .map_err(|e| format!("failed to parse config {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::{Config, XdgPaths};
    use std::path::PathBuf;

    #[test]
    fn default_config_has_safe_bounds() {
        let cfg = Config::default();
        assert_eq!(cfg.general.min_grace_hours, 24);
        assert_eq!(cfg.general.max_cross_device_mb, 512);
        assert_eq!(cfg.general.max_trash_total_gb, 30);
        assert!(cfg.cache.enabled);
        assert!(cfg.cache.dir_days.contains_key("zen"));
        assert!(cfg.cache.process_locked.contains_key("zen"));
    }

    #[test]
    fn parses_partial_toml_preserving_defaults() {
        let toml_str = r#"
[general]
min_grace_hours = 48
exclude_paths = ["~/important", "/mnt/data"]

[cache]
default_days = 7
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.general.min_grace_hours, 48);
        assert_eq!(cfg.general.exclude_paths, vec!["~/important", "/mnt/data"]);
        assert_eq!(cfg.cache.default_days, 7);
        // Preserves defaults for other sections
        assert!(cfg.tools.uv_prune);
        assert_eq!(cfg.tools.journal_max_days, 21);
    }

    #[test]
    fn expands_tilde_paths_properly() {
        let paths = XdgPaths {
            home: PathBuf::from("/home/user"),
            config_dir: PathBuf::from("/home/user/.config"),
            cache_dir: PathBuf::from("/home/user/.cache"),
            state_dir: PathBuf::from("/home/user/.local/state"),
            trash_dir: PathBuf::from("/home/user/.local/share/Trash"),
        };

        assert_eq!(paths.expand_tilde("~"), PathBuf::from("/home/user"));
        assert_eq!(paths.expand_tilde("~/docs"), PathBuf::from("/home/user/docs"));
        assert_eq!(paths.expand_tilde("/etc/hosts"), PathBuf::from("/etc/hosts"));
    }
}
