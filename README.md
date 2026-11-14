# sweep

Smart non-destructive cache, temp, and project artifact cleaner for Linux.

Unlike blunt clean-up scripts that invoke `rm -rf`, `sweep` identifies stale build targets and caches across multiple ecosystems, verifies that no running process owns them, and safely relocates them to the FreeDesktop Trash bin with valid `.trashinfo` metadata.

## Core Principles

- **Non-destructive:** Stale files are moved to `~/.local/share/Trash/files` instead of being permanently unlinked. Any cleanup can be inspected or undone.
- **Process Protection:** Inspects `/proc` before touching artifacts. If a compiler, dev server, or IDE is actively using a directory, it is skipped.
- **Age and Size Thresholds:** Only targets exceeding configurable idle thresholds (e.g. 14+ days untouched) are candidates for cleanup.
- **Ecosystem Aware:** Built-in heuristics for Rust (`target`), Node.js (`node_modules`), Python (`.venv`, `__pycache__`), Go, Gradle, Docker, package managers, and browser caches.
- **Structured Reporting:** Clean terminal output styled with the Gruvbox Dark theme via `rurich`, with machine-readable `--json` output available for automation.

## Command Line Interface

```bash
# Preview candidates without touching the filesystem
sweep scan

# Run cleanup (moves stale items to Trash)
sweep run

# Show cache sizes, Trash status, and systemd timer telemetry
sweep status

# Inspect effective configuration and file paths
sweep config

# Non-interactive / machine-readable JSON output
sweep run --dry-run --json
```

### Options

| Flag | Description |
| :--- | :--- |
| `-n`, `--dry-run` | Report candidate items without modifying anything |
| `-v`, `--verbose` | List every individual candidate rather than just top entries |
| `--json` | Output machine-readable JSON report to stdout |
| `-h`, `--help` | Show command line help |

## Configuration

Configuration is loaded from `$XDG_CONFIG_HOME/sweep/config.toml` (or `~/.config/sweep/config.toml`).

```toml
[general]
trash_enabled = true
dry_run = false
min_age_days = 14
max_reclaim_bytes = 10737418240 # 10 GiB per run limit

[categories]
rust_target = true
node_modules = true
python_venvs = true
browser_cache = true
package_manager_cache = true
```

## Systemd Automation

`sweep` can run periodically in the background via systemd user timers.

### User Service (`~/.config/systemd/user/sweep.service`)

```ini
[Unit]
Description=Sweep smart cache and temp cleaner
Documentation=https://github.com/koresent/sweep

[Service]
Type=oneshot
ExecStart=%h/.local/bin/sweep run
Nice=19
IOSchedulingClass=idle
```

### User Timer (`~/.config/systemd/user/sweep.timer`)

```ini
[Unit]
Description=Daily sweep cache cleaner run

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

Enable and start the timer:
```bash
systemctl --user daemon-reload
systemctl --user enable --now sweep.timer
```

## Building from Source

```bash
cargo build --release
install -m 755 target/release/sweep ~/.local/bin/sweep
```
