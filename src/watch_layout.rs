//! Layout for the original Watch dashboard. Rendering has no telemetry or key bindings.
use crate::ui::{Palette, Panel};

unsafe extern "C" {
    fn wcwidth(ch: libc::wchar_t) -> libc::c_int;
}

/// Use the UTF-8 width table on this thread without changing the process locale.
pub(crate) struct Layout<'a> {
    p: &'a Palette,
    locale: libc::locale_t,
    previous_locale: libc::locale_t,
}

#[derive(Clone, Copy)]
struct Cell<'a> {
    text: &'a str,
    columns: usize,
    style: usize,
}

impl<'a> Layout<'a> {
    pub(crate) fn new(p: &'a Palette) -> Self {
        // Debian/Linux supplies C.UTF-8 independent of the user's LANG or NO_COLOR.
        let locale = unsafe {
            libc::newlocale(
                libc::LC_CTYPE_MASK,
                c"C.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        let previous_locale = if locale.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { libc::uselocale(locale) }
        };
        Self {
            p,
            locale,
            previous_locale,
        }
    }

    fn cells<'b>(&self, mut text: &'b str) -> (Vec<Cell<'b>>, Vec<String>) {
        let mut styles = vec![String::new()];
        let mut cells = Vec::new();
        while !text.is_empty() {
            if let Some(rest) = text.strip_prefix("\x1b[") {
                if let Some(i) = rest.find(|ch: char| !(ch.is_ascii_digit() || ch == ';')) {
                    if rest.as_bytes()[i] == b'm' {
                        let sgr = &text[..i + 3];
                        let mut style = if sgr == "\x1b[0m" || sgr == "\x1b[m" {
                            String::new()
                        } else {
                            styles.last().unwrap().clone()
                        };
                        if sgr != "\x1b[0m" && sgr != "\x1b[m" {
                            style.push_str(sgr);
                        }
                        styles.push(style);
                        text = &text[i + 3..];
                        continue;
                    }
                }
            }
            let ch = text.chars().next().unwrap();
            let end = ch.len_utf8();
            let columns = unsafe { wcwidth(ch as libc::wchar_t) };
            cells.push(Cell {
                text: &text[..end],
                columns: if columns < 0 { 1 } else { columns as usize },
                style: styles.len() - 1,
            });
            text = &text[end..];
        }
        (cells, styles)
    }

    fn width(&self, line: &str) -> usize {
        self.cells(line).0.iter().map(|cell| cell.columns).sum()
    }

    /// Wrap on spaces when possible and split long words only as required.
    /// Style spans restart after each border; no reset can bleed into another card.
    fn body(&self, line: &str, width: usize) -> Vec<String> {
        let (cells, styles) = self.cells(line);
        let mut rows = Vec::new();
        let mut start = 0;
        while start < cells.len() {
            let mut end = start;
            let mut columns = 0;
            let mut space = None;
            while let Some(cell) = cells.get(end) {
                if columns + cell.columns > width && end > start {
                    break;
                }
                if cell.text == " " && end > start {
                    space = Some(end);
                }
                columns += cell.columns;
                end += 1;
            }
            let (cut, next) = if end == cells.len() {
                (end, end)
            } else if let Some(pos) = space {
                (pos, pos + 1)
            } else {
                // A space at the boundary is a soft line break, not a new blank row.
                (end, end + usize::from(cells[end].text == " "))
            };
            let mut output = String::new();
            let mut style = "";
            let mut used = 0;
            for cell in &cells[start..cut] {
                let next_style = &styles[cell.style];
                if next_style != style {
                    if !style.is_empty() {
                        output.push_str(self.p.nc);
                    }
                    output.push_str(next_style);
                    style = next_style;
                }
                output.push_str(cell.text);
                used += cell.columns;
            }
            if !style.is_empty() {
                output.push_str(self.p.nc);
            }
            output.push_str(&" ".repeat(width.saturating_sub(used)));
            rows.push(output);
            start = next;
        }
        if rows.is_empty() {
            rows.push(" ".repeat(width));
        }
        rows
    }

    pub(crate) fn banner(
        &self,
        title: &str,
        center: &str,
        badge: &str,
        badge_col: &str,
        width: usize,
    ) -> Vec<String> {
        let p = self.p;
        let inner = width - 2;
        let left = self.width(title);
        let mid = self.width(center);
        let right = self.width(badge);
        let title = format!("{}{title}{}", p.bold, p.nc);
        let badge = format!("{badge_col}{badge}{}", p.nc);
        let mut inner_rows = Vec::new();
        if inner >= left + mid + right + 8 {
            let gap = inner - left - mid - right - 4;
            inner_rows.push(format!(
                "  {title}{}{center}{}{badge}  ",
                " ".repeat(gap / 2),
                " ".repeat(gap - gap / 2)
            ));
        } else {
            if inner >= left + right + 6 {
                inner_rows.push(format!(
                    "  {title}{}{badge}  ",
                    " ".repeat(inner - left - right - 4)
                ));
            } else {
                inner_rows.extend(self.body(&title, inner));
                inner_rows.extend(self.body(&badge, inner));
            }
            if !center.is_empty() {
                for row in self.body(center, inner - 2) {
                    inner_rows.push(format!(" {row} "));
                }
            }
        }
        let mut rows = vec![format!("{}╭{}╮{}", p.cyan, "─".repeat(inner), p.nc)];
        rows.extend(
            inner_rows
                .into_iter()
                .map(|row| format!("{}│{}{row}{}│{}", p.cyan, p.nc, p.cyan, p.nc)),
        );
        rows.push(format!("{}╰{}╯{}", p.cyan, "─".repeat(inner), p.nc));
        rows
    }

    fn row(&self, body: &str) -> String {
        let p = self.p;
        format!("{}│{} {body} {}│{}", p.dim, p.nc, p.dim, p.nc)
    }

    fn card(&self, panel: &Panel<'_>, width: usize) -> (String, Vec<String>, String) {
        let p = self.p;
        let inner = width - 2;
        let title = format!(" {}{}{} ", p.bold, panel.title, p.nc);
        let badge = if panel.badge.is_empty() {
            String::new()
        } else {
            format!("{} {} {}", panel.badge_col, panel.badge, p.nc)
        };
        let plain_header = format!("{title}{badge}");
        let fit = self.width(&plain_header) + 1 + usize::from(!badge.is_empty());
        let mut rows = Vec::new();
        let top = if fit <= inner {
            let fill = "─".repeat(inner - fit);
            let end = if badge.is_empty() { "" } else { "─" };
            format!(
                "{}╭─{}{title}{}{fill}{}{badge}{}{end}╮{}",
                p.dim, p.nc, p.dim, p.nc, p.dim, p.nc
            )
        } else {
            rows.extend(self.body(plain_header.trim(), width - 4));
            format!("{}╭{}╮{}", p.dim, "─".repeat(inner), p.nc)
        };
        for line in panel.lines {
            rows.extend(self.body(line, width - 4));
        }
        (
            top,
            rows,
            format!("{}╰{}╯{}", p.dim, "─".repeat(inner), p.nc),
        )
    }

    pub(crate) fn panel(&self, panel: &Panel<'_>, width: usize) -> Vec<String> {
        let (top, rows, bottom) = self.card(panel, width);
        std::iter::once(top)
            .chain(rows.iter().map(|row| self.row(row)))
            .chain(std::iter::once(bottom))
            .collect()
    }

    pub(crate) fn columns(&self, left: &Panel<'_>, right: &Panel<'_>, width: usize) -> Vec<String> {
        if width < 96 {
            let mut rows = self.panel(left, width);
            rows.extend(self.panel(right, width));
            return rows;
        }
        let lw = (width - 1) / 2;
        let rw = width - 1 - lw;
        let (lt, mut l, lb) = self.card(left, lw);
        let (rt, mut r, rb) = self.card(right, rw);
        let n = l.len().max(r.len());
        l.resize_with(n, || " ".repeat(lw - 4));
        r.resize_with(n, || " ".repeat(rw - 4));
        std::iter::once(format!("{lt} {rt}"))
            .chain(
                l.iter()
                    .zip(&r)
                    .map(|(a, b)| format!("{} {}", self.row(a), self.row(b))),
            )
            .chain(std::iter::once(format!("{lb} {rb}")))
            .collect()
    }

    pub(crate) fn text(&self, line: &str, width: usize) -> Vec<String> {
        self.body(line, width)
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    pub(crate) fn print(&self, rows: Vec<String>) {
        for row in rows {
            println!("{row}");
        }
    }
}

/// Terminal data must not inject control sequences or hide following text.
pub(crate) fn escape_controls(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() {
            escaped.extend(ch.escape_default());
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

impl Drop for Layout<'_> {
    fn drop(&mut self) {
        if !self.previous_locale.is_null() {
            unsafe {
                libc::uselocale(self.previous_locale);
            }
        }
        if !self.locale.is_null() {
            unsafe {
                libc::freelocale(self.locale);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(color: bool) -> Palette {
        let c = |code| if color { code } else { "" };
        Palette {
            color,
            bold: c("\x1b[1m"),
            dim: c("\x1b[2m"),
            green: c("\x1b[0;32m"),
            cyan: c("\x1b[0;36m"),
            yellow: c("\x1b[1;33m"),
            red: c("\x1b[0;31m"),
            nc: c("\x1b[0m"),
        }
    }

    fn visible(layout: &Layout<'_>, text: &str) -> String {
        layout
            .cells(text)
            .0
            .into_iter()
            .map(|cell| cell.text)
            .collect()
    }

    fn unpack(layout: &Layout<'_>, text: &str) -> String {
        visible(layout, text)
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '│')
            .collect()
    }

    #[test]
    fn wraps_original_dashboard_fields_at_supported_widths() {
        for color in [false, true] {
            let p = palette(color);
            let layout = Layout::new(&p);
            let mount =
                "/run/user/1000/long-mount-name/資料/very-long-project-directory-that-keeps-going-TMP-END";
            let swap = "/very/long/swapfile/for/project/workstation/heavy-fixture-SWAP-END";
            let name = "process-name-more-than-eighteen-columns-PROC-END";
            let reason = "RAM above configured threshold for long enough and cooldown completed";
            let sidebar_ram = format!("{mount} {swap} {}56.1%{} 20.6 GiB/32.0 GiB", p.yellow, p.nc);
            let sidebar_proc = format!("842 {name} {}██░░░░░░{} 1.26 GiB (4.4%)", p.cyan, p.nc);
            let history = format!("* 17:25:09 {reason} [+128 MiB RAM/0 MiB swap] [✔tmpfs,✔kernel]");
            let controls = "[c] Clean now   [+/-] RAM trigger (80%)   [n] Alerts   [r] Reload   [q] Quit   (2s refresh)";
            let center = "RAM 56% (11.4 GiB free) │ Swap 8% │ Load 0.61";
            let left_data = [sidebar_ram.clone()];
            let right_data = [sidebar_proc.clone()];
            let history_data = [history.clone()];
            let left = Panel {
                title: "MEMORY · SWAP · TMPFS",
                badge: "11.4 GiB free",
                badge_col: p.green,
                lines: &left_data,
            };
            let right = Panel {
                title: "PRESSURE & TOP CONSUMERS",
                badge: "Top 5",
                badge_col: p.cyan,
                lines: &right_data,
            };
            let activity = Panel {
                title: "AUTO-CLEAN ACTIVITY & TRIGGERS",
                badge: "STANDBY",
                badge_col: p.dim,
                lines: &history_data,
            };
            for width in [10, 20, 48, 60, 79, 80, 95, 96, 97, 119, 120, 121, 160, 220] {
                let banner = layout.banner(
                    "keep-ram-clean v2.3.0",
                    center,
                    "⚡ AUTO-CLEANING",
                    p.yellow,
                    width,
                );
                let columns = layout.columns(&left, &right, width);
                let history_rows = layout.panel(&activity, width);
                let footer = layout.text(controls, width);
                let boxed = banner.iter().chain(&columns).chain(&history_rows);
                assert!(
                    boxed.clone().all(|line| layout.width(line) == width),
                    "boxed width {width}"
                );
                assert!(
                    footer.iter().all(|line| layout.width(line) <= width),
                    "footer width {width}"
                );
                assert!(!boxed
                    .chain(&footer)
                    .any(|line| line.contains('…') || line.contains("...")));
                assert!(
                    unpack(&layout, &banner.join("")).contains(&unpack(&layout, center)),
                    "banner width {width}"
                );
                for (panel, row) in [
                    (&left, &sidebar_ram),
                    (&right, &sidebar_proc),
                    (&activity, &history),
                ] {
                    // Prove every visible value survives row wrapping without a replacement character.
                    assert!(
                        unpack(&layout, &layout.panel(panel, width).join(""))
                            .contains(&unpack(&layout, row)),
                        "row width {width}"
                    );
                }
                assert_eq!(unpack(&layout, &footer.join("")), unpack(&layout, controls));
            }
        }
    }

    #[test]
    fn wraps_wide_and_combining_characters_without_splitting_style() {
        let p = palette(true);
        let layout = Layout::new(&p);
        assert_eq!(layout.width("資料⚡e\u{301}"), 7);
        let input = format!("{}資料⚡e\u{301}資料abcdefgh{}", p.green, p.nc);
        let rows = layout.body(&input, 6);
        assert!(rows.len() > 1);
        for row in &rows {
            assert_eq!(layout.width(row), 6);
            assert!(row.starts_with(p.green));
            assert!(row.contains(p.nc));
        }
        assert_eq!(unpack(&layout, &rows.join("")), unpack(&layout, &input));
    }

    #[test]
    fn actual_activity_formatter_keeps_literal_ellipses_suffixes_and_three_events() {
        use crate::daemon::{DaemonState, LastCleanupState, LastCleanupStep};
        let p = palette(false);
        let layout = Layout::new(&p);
        let state = DaemonState {
            version: "2.3.0".into(),
            pid: 997,
            starttime: Some(1),
            status: "running".into(),
            uid: 1000,
            dry_run: false,
            config_path: None,
            started_at: 1_700_000_000.0,
            updated_at: 1_700_000_010.0,
            cleanups_run: 4,
            total_ram_freed_mb: 81,
            total_swap_freed_mb: 9,
            cleaning_now: true,
            cleaning_reason: Some(format!(
                "literal... and … {} CURRENT-END",
                "longtext ".repeat(28)
            )),
            last_cleanup: None,
            alive: true,
            state_file: String::new(),
            recent_cleanups: (1..=4)
                .map(|i| LastCleanupState {
                    finished_at: 1_700_000_000.0 + f64::from(i),
                    duration_seconds: 0.2,
                    reasons: vec![format!(
                        "history {i} {} HISTORY-{i}-END",
                        "longword".repeat(32)
                    )],
                    ram_freed_mb: 128,
                    swap_freed_mb: 32,
                    steps: vec![LastCleanupStep {
                        name: format!("{} STEP-{i}-END", "longstep".repeat(20)),
                        status: "ok".into(),
                        summary: "fabricated".into(),
                    }],
                })
                .collect(),
        };
        let lines = crate::status_activity_lines(
            &crate::Config::default(),
            &p,
            Some(&state),
            escape_controls,
        );
        let panel = Panel {
            title: "AUTO-CLEAN ACTIVITY & TRIGGERS",
            badge: "CLEANING",
            badge_col: p.yellow,
            lines: &lines,
        };
        for width in [48, 80, 95, 96, 120] {
            let rows = layout.panel(&panel, width);
            let all = unpack(&layout, &rows.join(""));
            for suffix in [
                "CURRENT-END",
                "HISTORY-1-END",
                "HISTORY-2-END",
                "HISTORY-3-END",
                "STEP-1-END",
                "STEP-2-END",
                "STEP-3-END",
            ] {
                assert!(all.contains(suffix), "{width}: {suffix}");
            }
            assert!(!all.contains("HISTORY-4-END") && !all.contains("STEP-4-END"));
            assert_eq!(all.matches("...").count(), 1);
            assert_eq!(all.matches('…').count(), 1);
        }
    }

    #[test]
    fn synthetic_before_after_excerpt() {
        let p = palette(false);
        let layout = Layout::new(&p);
        let title = "keep-ram-clean v2.3.0";
        let center = "RAM 56% (11.4 GiB free) │ Swap 8% │ Load 0.61";
        let badge = "● DAEMON RUNNING";
        let mount = "/run/user/1000/a-long/fixture/work-directory-TMP-END";
        let proc_name = "long-build-worker-name-PROC-END";
        let mount_line = format!("{mount} 20.0% 128 MiB/640 MiB");
        let proc_line = format!("482 {proc_name} 196 MiB (0.6%)");
        println!("80 columns, fabricated Watch values");
        let (before_mount, before_proc) = (
            crate::ui::truncate_str(mount, 5),
            crate::ui::truncate_str(proc_name, 18),
        );
        println!("Before: {title} / {badge}; compact header dropped RAM/swap/load");
        println!("Before: {before_mount} 20.0% 128 MiB/640 MiB; 482 {before_proc} 196 MiB (0.6%)");
        println!("After:");
        for line in layout
            .banner(title, center, badge, p.green, 80)
            .iter()
            .skip(1)
            .take(2)
        {
            println!("{line}");
        }
        for line in [&mount_line, &proc_line] {
            for row in layout.text(line, 76) {
                println!("  {row}");
            }
        }
        assert!(before_mount.contains('…') && before_proc.contains('…'));
        assert!(layout.text(&mount_line, 76).join("").contains("TMP-END"));
        assert!(layout.text(&proc_line, 76).join("").contains("PROC-END"));
    }

    #[test]
    fn escapes_data_and_preserves_the_literal_content() {
        let input = "/tmp/資料\x1b[2J\t\nrest";
        let result = escape_controls(input);
        assert_eq!(result, "/tmp/資料\\u{1b}[2J\\t\\nrest");
        assert!(!result.contains('\x1b'));
    }
}
