mod cleaner;
mod config;
mod proc_guard;
mod trash;
mod ui;

use cleaner::{Cleaner, SweepReport, format_bytes};
use config::{XdgPaths, load_config};
use proc_guard::ProcessGuard;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::process::{Command, ExitCode};
use trash::{TrashBin, entry_disk_bytes};

#[derive(Debug, Serialize, Deserialize)]
struct LastRunSummary {
    timestamp_unix: i64,
    reclaimed_bytes: u64,
    total_items: usize,
    skipped_busy_count: usize,
    error_count: usize,
}

#[derive(Debug, Serialize)]
struct CacheEntrySize {
    name: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    cache_bytes: u64,
    trash_bytes: u64,
    timer_status: String,
    last_run: Option<LastRunSummary>,
    top_caches: Vec<CacheEntrySize>,
}

struct Style {
    color: bool,
}

impl Style {
    fn detect() -> Self {
        let no_color = env::var_os("NO_COLOR").is_some();
        Self {
            color: !no_color && std::io::stdout().is_terminal(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{}m{}\x1b[0m", code, text)
        } else {
            text.to_string()
        }
    }

    fn aqua(&self, s: &str) -> String {
        self.paint("1;38;2;142;192;124", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("1;38;2;184;187;38", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("1;38;2;250;189;47", s)
    }
    fn orange(&self, s: &str) -> String {
        self.paint("38;2;254;128;25", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("1;38;2;251;73;52", s)
    }
    fn blue(&self, s: &str) -> String {
        self.paint("38;2;131;165;152", s)
    }
    fn muted(&self, s: &str) -> String {
        self.paint("38;2;168;153;132", s)
    }
}

fn print_help() {
    println!(
        "\
sweep - smart non-destructive cache, temp & project artifact cleaner

USAGE:
  sweep [run] [OPTIONS]    Run cleanup (moves stale unused items to Trash)
  sweep scan  [OPTIONS]    Dry-run scan showing what would be cleaned
  sweep status             Show cache/trash sizes, last run & timer status
  sweep config             Show config file path and effective settings
  sweep help               Show this help

OPTIONS:
  -n, --dry-run            Do not modify anything; report candidates only
  -v, --verbose            List all candidate items (not just top entries)
      --json               Output machine-readable JSON report"
    );
}

fn main() -> ExitCode {
    rurich::theme::set_global_theme(ui::gruvbox_theme());
    let args: Vec<String> = env::args().skip(1).collect();
    let style = Style::detect();

    let mut subcmd = "run";
    let mut dry_run = false;
    let mut verbose = false;
    let mut json_out = false;

    for arg in &args {
        match arg.as_str() {
            "run" => subcmd = "run",
            "scan" | "dry-run" => {
                subcmd = "run";
                dry_run = true;
            }
            "status" => subcmd = "status",
            "config" => subcmd = "config",
            "help" | "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--json" => json_out = true,
            other => {
                eprintln!("Unknown argument: {}", other);
                print_help();
                return ExitCode::from(2);
            }
        }
    }

    let paths = match XdgPaths::discover() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sweep error: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let cfg_path = paths.config_file();
    let config = match load_config(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sweep config error: {}", e);
            return ExitCode::FAILURE;
        }
    };

    match subcmd {
        "config" => {
            println!("# Config file: {}", cfg_path.display());
            match toml::to_string_pretty(&config) {
                Ok(s) => print!("{}", s),
                Err(e) => {
                    eprintln!("failed to serialize config: {}", e);
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        "status" => {
            let status = collect_status(&paths);
            if json_out {
                match serde_json::to_string_pretty(&status) {
                    Ok(s) => println!("{}", s),
                    Err(e) => {
                        eprintln!("json serialization failed: {}", e);
                        return ExitCode::FAILURE;
                    }
                }
            } else if style.color {
                ui::render_status(&status);
            } else {
                show_status_plain(&status, &style);
            }
            ExitCode::SUCCESS
        }
        _ => {
            let proc_guard = ProcessGuard::scan();
            let mut trash_bin = if dry_run {
                None
            } else {
                match TrashBin::new(
                    &paths.trash_dir,
                    config.general.max_cross_device_mb,
                    config.general.max_trash_total_gb,
                ) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        eprintln!("sweep trash init error: {}", e);
                        return ExitCode::FAILURE;
                    }
                }
            };

            let cleaner = Cleaner::new(
                &config,
                &paths,
                &proc_guard,
                trash_bin.as_mut(),
                dry_run,
            );
            let report = cleaner.run_all();

            if !dry_run {
                save_last_run(&paths, &report);
            }

            if json_out {
                match serde_json::to_string_pretty(&report) {
                    Ok(s) => println!("{}", s),
                    Err(e) => {
                        eprintln!("json serialization failed: {}", e);
                        return ExitCode::FAILURE;
                    }
                }
            } else if style.color {
                ui::render_report(&report, verbose);
            } else {
                print_report(&report, verbose, &style);
            }

            if report.error_count > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

fn save_last_run(paths: &XdgPaths, report: &SweepReport) {
    let state_file = paths.state_file();
    if let Some(parent) = state_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let summary = LastRunSummary {
        timestamp_unix: report.timestamp_unix,
        reclaimed_bytes: report.total_reclaimed_bytes,
        total_items: report.total_items,
        skipped_busy_count: report.skipped_busy_count,
        error_count: report.error_count,
    };
    if let Ok(json) = serde_json::to_string_pretty(&summary) {
        let _ = fs::write(state_file, json);
    }
}

fn print_report(report: &SweepReport, verbose: bool, style: &Style) {
    let mode = if report.dry_run {
        style.yellow("DRY-RUN")
    } else {
        style.green("CLEANED")
    };

    println!(
        "{} [{}] reclaimed={} items={} skipped_busy={} errors={}",
        style.aqua("==> sweep"),
        mode,
        style.green(&format_bytes(report.total_reclaimed_bytes)),
        report.total_items,
        report.skipped_busy_count,
        report.error_count
    );

    if !report.tool_hooks.is_empty() {
        println!("{}", style.aqua("  Tool hooks:"));
        for hook in &report.tool_hooks {
            let status_str = match hook.status.as_str() {
                "ran" => style.green(&hook.status),
                "would_run" => style.yellow(&hook.status),
                "skipped_busy" => style.orange(&hook.status),
                "error" => style.red(&hook.status),
                _ => style.muted(&hook.status),
            };
            println!("    - {} [{}]: {}", hook.name, status_str, hook.detail);
        }
    }

    if !report.skipped_groups.is_empty() {
        println!("{}", style.aqua("  Protected (active processes):"));
        for g in &report.skipped_groups {
            println!(
                "    - {} {} ({})",
                style.orange("[busy]"),
                style.blue(&g.path),
                style.muted(&g.reason)
            );
        }
    }

    if !report.items.is_empty() {
        let mut by_cat: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
        for item in &report.items {
            if matches!(item.action.as_str(), "trashed" | "would_trash") {
                let entry = by_cat.entry(item.category.as_str()).or_insert((0, 0));
                entry.0 += 1;
                entry.1 = entry.1.saturating_add(item.size_bytes);
            }
        }

        if !by_cat.is_empty() {
            println!("{}", style.aqua("  Summary by category:"));
            for (cat, (count, bytes)) in &by_cat {
                println!(
                    "    - {:<16} {:>5} items   {}",
                    cat,
                    count,
                    style.green(&format_bytes(*bytes))
                );
            }
        }

        let mut sorted: Vec<_> = report.items.iter().collect();
        sorted.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

        let limit = if verbose { sorted.len() } else { 20 };
        println!(
            "{}",
            style.aqua(&format!(
                "  {} items{}:",
                if verbose { "All" } else { "Top" },
                if !verbose && sorted.len() > 20 {
                    format!(" (showing 20 of {})", sorted.len())
                } else {
                    String::new()
                }
            ))
        );

        for item in sorted.into_iter().take(limit) {
            let tag = match item.action.as_str() {
                "trashed" => style.green("[trashed]"),
                "would_trash" => style.yellow("[would]  "),
                "skipped_limit" => style.orange("[limit]  "),
                "error" => style.red("[error]  "),
                _ => style.muted(&format!("[{}]", item.action)),
            };
            println!(
                "    {} {:>10} {:>4}d  {}  {}",
                tag,
                format_bytes(item.size_bytes),
                item.age_days,
                style.blue(&item.path),
                style.muted(&format!("({})", item.note))
            );
        }
    }
}

fn collect_status(paths: &XdgPaths) -> StatusReport {
    let mut cache_bytes: u64 = 0;
    let mut top_caches: Vec<CacheEntrySize> = Vec::new();

    if let Ok(rd) = fs::read_dir(&paths.cache_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let sz = entry_disk_bytes(&p);
            cache_bytes = cache_bytes.saturating_add(sz);
            top_caches.push(CacheEntrySize {
                name: entry.file_name().to_string_lossy().to_string(),
                bytes: sz,
            });
        }
    }
    top_caches.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    top_caches.truncate(8);

    let trash_bytes = entry_disk_bytes(&paths.trash_dir.join("files"));

    let last_run = fs::read_to_string(paths.state_file())
        .ok()
        .and_then(|raw| serde_json::from_str::<LastRunSummary>(&raw).ok());

    let timer_status = Command::new("systemctl")
        .args(["--user", "is-active", "sweep.timer"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { "unknown".to_string() } else { s }
        })
        .unwrap_or_else(|_| "unknown".to_string());

    StatusReport {
        cache_bytes,
        trash_bytes,
        timer_status,
        last_run,
        top_caches,
    }
}

fn show_status_plain(status: &StatusReport, style: &Style) {
    println!("{}", style.aqua("==> sweep status"));
    println!(
        "  ~/.cache:              {}",
        style.yellow(&format_bytes(status.cache_bytes))
    );
    println!(
        "  ~/.local/share/Trash:  {}",
        style.blue(&format_bytes(status.trash_bytes))
    );

    if let Some(ref last) = status.last_run {
        println!(
            "  Last run:              {} (reclaimed {}, {} items, {} busy skipped)",
            format_unix_local(last.timestamp_unix),
            style.green(&format_bytes(last.reclaimed_bytes)),
            last.total_items,
            last.skipped_busy_count
        );
    } else {
        println!("  Last run:              {}", style.muted("never"));
    }

    let colored = if status.timer_status == "active" {
        style.green(&status.timer_status)
    } else {
        style.orange(&status.timer_status)
    };
    println!("  systemd sweep.timer:   {}", colored);
}

fn format_unix_local(ts: i64) -> String {
    unsafe {
        let t = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}
