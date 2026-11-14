use crate::cleaner::{SweepReport, format_bytes};
use crate::StatusReport;
use rurich::box_styles::ROUNDED;
use rurich::console::Console;
use rurich::panel::Panel;
use rurich::protocol::JustifyMethod;
use rurich::style::Style;
use rurich::table::{Column, Table};
use rurich::theme::Theme;
use std::collections::BTreeMap;

#[allow(dead_code)]
pub mod gruvbox {
    pub const BG: &str = "#282828";
    pub const BORDER: &str = "#504945";
    pub const FG: &str = "#ebdbb2";
    pub const MUTED: &str = "#a89984";
    pub const GREEN: &str = "#b8bb26";
    pub const YELLOW: &str = "#fabd2f";
    pub const RED: &str = "#fb4934";
    pub const BLUE: &str = "#83a598";
    pub const AQUA: &str = "#8ec07c";
    pub const ORANGE: &str = "#fe8019";
    pub const PURPLE: &str = "#d3869b";
}

pub fn gruvbox_theme() -> Theme {
    let mut theme = Theme::new();
    let mappings = [
        ("ok", "bold #b8bb26"),
        ("err", "bold #fb4934"),
        ("warn", "bold #fabd2f"),
        ("info", "bold #83a598"),
        ("accent", "bold #8ec07c"),
        ("orange", "bold #fe8019"),
        ("purple", "bold #d3869b"),
        ("muted", "#a89984"),
        ("key", "bold #fabd2f"),
        ("val", "#ebdbb2"),
        ("path", "#83a598"),
        ("header", "bold #8ec07c"),
        ("border", "#504945"),
    ];
    for (name, style_str) in mappings {
        if let Ok(style) = Style::parse(style_str) {
            theme.set(name, style);
        }
    }
    theme
}

pub fn make_console() -> Console {
    let theme = gruvbox_theme();
    rurich::theme::set_global_theme(theme.clone());
    let mut console = Console::new();
    console.theme = theme;
    console
}

pub fn kv_table(rows: &[(&str, String)], key_width: usize) -> Table {
    let mut t = Table::grid();
    t.padding = (0, 1);
    t.add_column(
        Column::new()
            .header("Key")
            .style(Style::parse("bold #fabd2f").unwrap_or_default())
            .width(key_width)
            .no_wrap(true),
    );
    t.add_column(
        Column::new()
            .header("Val")
            .style(Style::parse("#ebdbb2").unwrap_or_default()),
    );
    for (k, v) in rows {
        t.add_row(vec![(*k).to_string(), v.clone()]);
    }
    t
}

pub fn share_bar(part: u64, total: u64, color: &str, width: usize) -> String {
    let denom = total.max(1) as f64;
    let filled = if total > 0 {
        ((part as f64 / denom) * width as f64).round() as usize
    } else {
        0
    };
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    format!(
        "[{color}]{}[/{color}][{border}]{}[/{border}]",
        "█".repeat(filled),
        "░".repeat(empty),
        border = gruvbox::BORDER
    )
}

pub fn render_status(status: &StatusReport) {
    let console = make_console();
    let timer_fmt = if status.timer_status == "active" {
        format!("[ok]● {}[/ok]", status.timer_status)
    } else {
        format!("[orange]○ {}[/orange]", status.timer_status)
    };

    let last_fmt = if let Some(last) = &status.last_run {
        format!(
            "[muted]reclaimed [ok]{}[/ok], {} items, {} busy skipped[/muted]",
            format_bytes(last.reclaimed_bytes),
            last.total_items,
            last.skipped_busy_count
        )
    } else {
        "[muted]never[/muted]".to_string()
    };

    let rows = [
        ("~/.cache", format!("[warn]{}[/warn]", format_bytes(status.cache_bytes))),
        ("~/.local/share/Trash", format!("[info]{}[/info]", format_bytes(status.trash_bytes))),
        ("systemd sweep.timer", timer_fmt),
        ("Last Run", last_fmt),
    ];

    let mut panel = Panel::new(kv_table(&rows, 22));
    panel.box_style = ROUNDED;
    panel = panel.title("[header]🧹 Sweep Status[/header]");
    console.println(panel);

    if !status.top_caches.is_empty() {
        let mut tbl = Table::new();
        tbl.box_style = Some(ROUNDED);
        tbl = tbl.title("Top ~/.cache Consumers");
        tbl.add_column(
            Column::new()
                .header("Directory")
                .style(Style::parse("bold #fabd2f").unwrap_or_default()),
        );
        tbl.add_column(
            Column::new()
                .header("Size")
                .style(Style::parse("#ebdbb2").unwrap_or_default())
                .justify(JustifyMethod::Right)
                .no_wrap(true),
        );
        tbl.add_column(Column::new().header("Share").no_wrap(true));
        tbl.add_column(
            Column::new()
                .header("%")
                .style(Style::parse("#a89984").unwrap_or_default())
                .justify(JustifyMethod::Right)
                .no_wrap(true),
        );

        let palette = [gruvbox::ORANGE, gruvbox::YELLOW, gruvbox::AQUA, gruvbox::BLUE, gruvbox::PURPLE];
        for (idx, entry) in status.top_caches.iter().enumerate() {
            if entry.bytes == 0 {
                continue;
            }
            let pct = (entry.bytes as f64 / status.cache_bytes.max(1) as f64) * 100.0;
            let color = palette[idx % palette.len()];
            tbl.add_row(vec![
                entry.name.clone(),
                format_bytes(entry.bytes),
                share_bar(entry.bytes, status.cache_bytes, color, 16),
                format!("{pct:.1}%"),
            ]);
        }
        console.println(tbl);
    }
}

pub fn render_report(report: &SweepReport, verbose: bool) {
    let console = make_console();
    let border_color = if report.error_count > 0 {
        gruvbox::RED
    } else if report.dry_run {
        gruvbox::YELLOW
    } else {
        gruvbox::GREEN
    };

    let mode_str = if report.dry_run {
        "[warn]DRY-RUN (no files modified)[/warn]".to_string()
    } else {
        "[ok]CLEANED (moved to Trash)[/ok]".to_string()
    };

    let rows = [
        ("Mode", mode_str),
        ("Reclaimed", format!("[ok]{}[/ok]", format_bytes(report.total_reclaimed_bytes))),
        ("Items", format!("{}", report.total_items)),
        (
            "Skipped Busy",
            if report.skipped_busy_count > 0 {
                format!("[orange]{}[/orange]", report.skipped_busy_count)
            } else {
                "[muted]0[/muted]".to_string()
            },
        ),
        (
            "Errors",
            if report.error_count > 0 {
                format!("[err]{}[/err]", report.error_count)
            } else {
                "[ok]0[/ok]".to_string()
            },
        ),
    ];

    let mut panel = Panel::new(kv_table(&rows, 14));
    panel.box_style = ROUNDED;
    panel = panel.title("[header]🧹 Sweep Report[/header]");
    panel.border_style = Style::parse(border_color).ok();
    console.println(panel);

    if !report.tool_hooks.is_empty() {
        let mut htbl = Table::new();
        htbl.box_style = Some(ROUNDED);
        htbl = htbl.title("Tool Hooks");
        htbl.add_column(
            Column::new()
                .header("Hook")
                .style(Style::parse("bold #fabd2f").unwrap_or_default())
                .no_wrap(true),
        );
        htbl.add_column(Column::new().header("Status").no_wrap(true));
        htbl.add_column(
            Column::new()
                .header("Freed")
                .style(Style::parse("#ebdbb2").unwrap_or_default())
                .justify(JustifyMethod::Right)
                .no_wrap(true),
        );
        htbl.add_column(
            Column::new()
                .header("Detail")
                .style(Style::parse("#a89984").unwrap_or_default()),
        );
        for h in &report.tool_hooks {
            let st_fmt = match h.status.as_str() {
                "ran" => "[ok]✔ ran[/ok]",
                "would_run" => "[warn]◎ would_run[/warn]",
                "skipped_busy" => "[orange]~ skipped_busy[/orange]",
                "error" => "[err]✘ error[/err]",
                _ => &h.status,
            };
            let freed_str = if h.freed_bytes > 0 {
                format_bytes(h.freed_bytes)
            } else {
                "-".to_string()
            };
            htbl.add_row(vec![h.name.clone(), st_fmt.to_string(), freed_str, h.detail.clone()]);
        }
        console.println(htbl);
    }

    if !report.skipped_groups.is_empty() {
        let mut stbl = Table::new();
        stbl.box_style = Some(ROUNDED);
        stbl = stbl.title("Protected (Active Processes)");
        stbl.add_column(
            Column::new()
                .header("Category")
                .style(Style::parse("bold #8ec07c").unwrap_or_default())
                .no_wrap(true),
        );
        stbl.add_column(
            Column::new()
                .header("Path")
                .style(Style::parse("#83a598").unwrap_or_default()),
        );
        stbl.add_column(
            Column::new()
                .header("Reason")
                .style(Style::parse("bold #fe8019").unwrap_or_default()),
        );
        for g in &report.skipped_groups {
            stbl.add_row(vec![g.category.clone(), g.path.clone(), g.reason.clone()]);
        }
        console.println(stbl);
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
            let total_cat_bytes: u64 = by_cat.values().map(|v| v.1).sum();
            let mut ctbl = Table::new();
            ctbl.box_style = Some(ROUNDED);
            ctbl = ctbl.title("Summary by Category");
            ctbl.add_column(
                Column::new()
                    .header("Category")
                    .style(Style::parse("bold #fabd2f").unwrap_or_default())
                    .no_wrap(true),
            );
            ctbl.add_column(
                Column::new()
                    .header("Items")
                    .style(Style::parse("#ebdbb2").unwrap_or_default())
                    .justify(JustifyMethod::Right)
                    .no_wrap(true),
            );
            ctbl.add_column(
                Column::new()
                    .header("Size")
                    .style(Style::parse("bold #b8bb26").unwrap_or_default())
                    .justify(JustifyMethod::Right)
                    .no_wrap(true),
            );
            ctbl.add_column(Column::new().header("Share").no_wrap(true));

            for (cat, (cnt, sz)) in &by_cat {
                ctbl.add_row(vec![
                    (*cat).to_string(),
                    format!("{cnt}"),
                    format_bytes(*sz),
                    share_bar(*sz, total_cat_bytes, gruvbox::GREEN, 14),
                ]);
            }
            console.println(ctbl);
        }

        let mut sorted: Vec<_> = report.items.iter().collect();
        sorted.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        let limit = if verbose { sorted.len() } else { 20 };
        let title = if verbose {
            "All Items".to_string()
        } else if sorted.len() > 20 {
            format!("Top Items (20 of {})", sorted.len())
        } else {
            "Items".to_string()
        };

        let mut itbl = Table::new();
        itbl.box_style = Some(ROUNDED);
        itbl = itbl.title(title);
        itbl.add_column(Column::new().header("Action").no_wrap(true));
        itbl.add_column(
            Column::new()
                .header("Size")
                .style(Style::parse("#ebdbb2").unwrap_or_default())
                .justify(JustifyMethod::Right)
                .no_wrap(true),
        );
        itbl.add_column(
            Column::new()
                .header("Age")
                .style(Style::parse("#a89984").unwrap_or_default())
                .justify(JustifyMethod::Right)
                .no_wrap(true),
        );
        itbl.add_column(
            Column::new()
                .header("Path")
                .style(Style::parse("#83a598").unwrap_or_default()),
        );
        itbl.add_column(
            Column::new()
                .header("Note")
                .style(Style::parse("#a89984").unwrap_or_default()),
        );

        for it in sorted.into_iter().take(limit) {
            let act_fmt = match it.action.as_str() {
                "trashed" => "[ok]✔ trashed[/ok]",
                "would_trash" => "[warn]◎ would[/warn]",
                "skipped_limit" => "[orange]! limit[/orange]",
                "error" => "[err]✘ error[/err]",
                _ => &it.action,
            };
            itbl.add_row(vec![
                act_fmt.to_string(),
                format_bytes(it.size_bytes),
                format!("{}d", it.age_days),
                it.path.clone(),
                it.note.clone(),
            ]);
        }
        console.println(itbl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gruvbox_theme() {
        let theme = gruvbox_theme();
        assert!(theme.get("ok").is_some());
        assert!(theme.get("err").is_some());
    }

    #[test]
    fn test_share_bar() {
        let bar = share_bar(25, 100, gruvbox::GREEN, 10);
        assert!(bar.contains("█"));
        assert!(bar.contains("░"));
    }
}
