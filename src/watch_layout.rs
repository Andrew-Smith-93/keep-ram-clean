//! Layout for the original Watch dashboard. Rendering has no telemetry or key bindings.
use crate::ui::{Palette, Panel};

unsafe extern "C" {
    fn wcwidth(ch: libc::wchar_t) -> libc::c_int;
}

/// Use the UTF-8 width table on this thread without changing the process locale.
pub(crate) struct Layout<'a> {
    p: &'a Palette,
    framed: bool,
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
            framed: true,
            locale,
            previous_locale,
        }
    }

    pub(crate) fn board(p: &'a Palette) -> Self {
        let mut layout = Self::new(p);
        layout.framed = false;
        layout
    }

    /// One full border per operational section. Equal outer edges, without
    /// stretching any of the content-sized tables inside.
    pub(crate) fn board_groups(
        &self,
        width: usize,
        groups: [(&str, Vec<String>); 3],
    ) -> Vec<String> {
        let p = self.p;
        let content = groups
            .iter()
            .flat_map(|(_, rows)| rows)
            .map(|row| self.width(row))
            .max()
            .unwrap_or_default();
        let headings = groups
            .iter()
            .map(|(title, _)| self.width(title) + 1)
            .max()
            .unwrap_or_default();
        let area = (width - 4).min(content.max(headings).max(1));
        let inner = area + 2;
        let line = |row: &str| {
            let pad = " ".repeat(area.saturating_sub(self.width(row)));
            format!("{}│{} {row}{pad} {}│{}", p.cyan, p.nc, p.cyan, p.nc)
        };
        let mut output = Vec::new();
        for (title, rows) in groups {
            if self.width(title) < area {
                let fill = "─".repeat(inner - self.width(title) - 3);
                output.push(format!(
                    "{}╭─ {}{title}{} {}{fill}╮{}",
                    p.cyan, p.bold, p.nc, p.cyan, p.nc
                ));
            } else {
                output.push(format!("{}╭{}╮{}", p.cyan, "─".repeat(inner), p.nc));
                output.extend(
                    self.text(&format!("{}{title}{}", p.bold, p.nc), area)
                        .iter()
                        .map(|row| line(row)),
                );
            }
            output.extend(rows.iter().map(|row| line(row)));
            output.push(format!("{}╰{}╯{}", p.cyan, "─".repeat(inner), p.nc));
        }
        output
    }

    fn table_surface(&self, panel: &Panel<'_>, width: usize) -> Vec<String> {
        if self.framed {
            self.panel(panel, width)
        } else {
            let p = self.p;
            let indent = 2;
            let mut output = self.text(
                &format!("{}{title}{}", p.bold, p.nc, title = panel.title),
                width - indent,
            );
            for line in panel.lines {
                output.extend(self.text(line, width - indent));
            }
            output.into_iter().map(|row| format!("  {row}")).collect()
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

    #[cfg(test)]
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

    pub(crate) fn swap_devices(
        &self,
        devices: &[crate::metrics::SwapDevice],
        width: usize,
    ) -> Vec<String> {
        if devices.is_empty() {
            return Vec::new();
        }
        let mut rows = Vec::new();
        let kind = |device: &crate::metrics::SwapDevice| {
            let kernel_type = escape_controls(&device.swap_type);
            if device.is_zram() {
                format!("{kernel_type} / zram")
            } else {
                kernel_type
            }
        };
        let mut frame_width = width;
        if width >= 60 {
            let headers = ["DEVICE / PATH", "TYPE", "USED", "SIZE"];
            let records: Vec<_> = devices
                .iter()
                .map(|device| {
                    [
                        escape_controls(&device.path),
                        kind(device),
                        crate::ui::fmt_mib(device.used_mb),
                        crate::ui::fmt_mib(device.size_mb),
                    ]
                })
                .collect();
            let minimum = headers.map(|header| self.width(header));
            let limits = [44, 20, 14, 14];
            let mut widths = minimum;
            for record in &records {
                for (i, value) in record.iter().enumerate() {
                    widths[i] = widths[i].max(self.width(value).min(limits[i]));
                }
            }
            let available = width - 4 - 9;
            while widths.iter().sum::<usize>() > available {
                let slot = (0..widths.len())
                    .max_by_key(|&i| widths[i] - minimum[i])
                    .unwrap();
                widths[slot] -= 1;
            }
            frame_width = 4 + 9 + widths.iter().sum::<usize>();
            let table_row = |cells: [&str; 4]| {
                let columns: Vec<_> = cells
                    .iter()
                    .enumerate()
                    .map(|(i, text)| {
                        let mut body = self.body(text, widths[i]);
                        if i >= 2 {
                            for line in &mut body {
                                let text = line.trim_end();
                                *line =
                                    format!("{}{text}", " ".repeat(widths[i] - self.width(text)));
                            }
                        }
                        body
                    })
                    .collect();
                let height = columns.iter().map(Vec::len).max().unwrap_or(1);
                (0..height)
                    .map(|line| {
                        columns
                            .iter()
                            .enumerate()
                            .map(|(i, parts)| {
                                parts
                                    .get(line)
                                    .cloned()
                                    .unwrap_or_else(|| " ".repeat(widths[i]))
                            })
                            .collect::<Vec<_>>()
                            .join(" │ ")
                    })
                    .collect::<Vec<_>>()
            };
            rows.extend(table_row(headers));
            rows.push(widths.map(|width| "─".repeat(width)).join("─┼─"));
            for record in &records {
                rows.extend(table_row(record.each_ref().map(String::as_str)));
            }
        } else {
            for (index, device) in devices.iter().enumerate() {
                if index != 0 {
                    rows.push(String::new());
                }
                rows.push(format!("Device: {}", escape_controls(&device.path)));
                rows.push(format!("Type: {}", kind(device)));
                rows.push(format!(
                    "Used: {}  Size: {}",
                    crate::ui::fmt_mib(device.used_mb),
                    crate::ui::fmt_mib(device.size_mb)
                ));
            }
        }
        self.table_surface(
            &Panel {
                title: "SWAP DEVICES",
                badge: "",
                badge_col: self.p.dim,
                lines: &rows,
            },
            frame_width,
        )
    }

    /// Lay out two independently wrapped tables without repeating padding.
    pub(crate) fn pair(
        &self,
        width: usize,
        left: impl FnOnce(usize) -> Vec<String>,
        right: impl FnOnce(usize) -> Vec<String>,
    ) -> Vec<String> {
        self.pair_at(width, (width - 1) / 2, left, right)
    }

    pub(crate) fn pair_at(
        &self,
        width: usize,
        lw: usize,
        left: impl FnOnce(usize) -> Vec<String>,
        right: impl FnOnce(usize) -> Vec<String>,
    ) -> Vec<String> {
        let gutter = if self.framed { 1 } else { 3 };
        let rw = width - lw - gutter;
        let mut l = left(lw);
        let mut r = right(rw);
        let used_left = if self.framed {
            l.iter()
                .map(|line| self.width(line))
                .max()
                .unwrap_or_default()
        } else {
            lw
        };
        let n = l.len().max(r.len());
        l.resize(n, String::new());
        r.resize(n, String::new());
        l.into_iter()
            .zip(r)
            .map(|(a, b)| {
                if b.is_empty() {
                    a
                } else {
                    let padding = used_left.saturating_sub(self.width(&a)) + gutter;
                    format!("{a}{}{b}", " ".repeat(padding))
                }
            })
            .collect()
    }

    pub(crate) fn table(
        &self,
        title: &str,
        headers: &[&str],
        rows: &[Vec<String>],
        width: usize,
    ) -> Vec<String> {
        let inner = width - 4;
        let mut frame_width = width;
        let gaps = 2 * (headers.len() - 1);
        let mut lines = Vec::new();
        let mut sizes = headers
            .iter()
            .map(|&header| match header {
                "USED / TOTAL" | "AMOUNTS" => 18, // fit two numbers with units
                "USAGE" | "RAM SHARE" => 14,      // fit the mini gauge and percentage
                "WHEN / CHANGE" => 27, // keep time/deltas together instead of starving this column
                _ => 8,
            })
            .collect::<Vec<_>>();
        let minimum = sizes.iter().sum::<usize>() + gaps;
        if inner >= minimum.max(34) {
            let usable = inner - gaps;
            let mut left = usable - sizes.iter().sum::<usize>();
            let wants: Vec<_> = headers
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    rows.iter()
                        .map(|r| self.width(&r[i]))
                        .max()
                        .unwrap_or(0)
                        .max(self.width(h))
                        .min(match *h {
                            "RESOURCE" => 36,
                            "PROCESS" => 40,
                            "FIELD" | "SETTING" | "METRIC" | "KEY" => 30,
                            "WHEN / CHANGE" => 38,
                            "REASON" => 70,
                            "STEPS" => 32,
                            _ => 52,
                        })
                })
                .collect();
            if headers.len() == 2 {
                let label = wants[0].min(usable / 2).max(sizes[0]).min(sizes[0] + left);
                left -= label - sizes[0];
                sizes[0] = label;
            }
            while left != 0 {
                let slot = (0..sizes.len())
                    .filter(|&i| sizes[i] < wants[i])
                    .max_by_key(|&i| wants[i].saturating_sub(sizes[i]));
                let Some(i) = slot else {
                    break;
                };
                sizes[i] += 1;
                left -= 1;
            }
            // Leave unused terminal space outside the table; never stretch a column.
            frame_width = (4 + gaps + sizes.iter().sum::<usize>())
                .max(self.width(title) + 5)
                .min(width);
            let wrapped = |row: &[String]| {
                let cells: Vec<_> = row
                    .iter()
                    .zip(&sizes)
                    .map(|(s, &size)| self.body(s, size))
                    .collect();
                let height = cells.iter().map(Vec::len).max().unwrap_or(1);
                (0..height)
                    .map(|y| {
                        cells
                            .iter()
                            .zip(&sizes)
                            .map(|(c, &n)| c.get(y).cloned().unwrap_or_else(|| " ".repeat(n)))
                            .collect::<Vec<_>>()
                            .join("│ ")
                    })
                    .collect::<Vec<_>>()
            };
            lines.extend(wrapped(
                &headers
                    .iter()
                    .map(|s| format!("{}{s}{}", self.p.bold, self.p.nc))
                    .collect::<Vec<_>>(),
            ));
            for row in rows {
                lines.extend(wrapped(row));
            }
        } else {
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    lines.push(String::new());
                }
                lines.extend(headers.iter().zip(row).map(|(h, v)| format!("{h}: {v}")));
            }
        }
        self.table_surface(
            &Panel {
                title,
                badge: "",
                badge_col: "",
                lines: &lines,
            },
            frame_width,
        )
    }

    pub(crate) fn editor(&self, editor: &crate::watch_editor::Editor, width: usize) -> Vec<String> {
        let (table, hints) = editor.view(width);
        let mut lines = self.banner(
            "WATCH SETTINGS / EDITOR",
            &format!(
                "Target: {}",
                escape_controls(&editor.draft.path.display().to_string())
            ),
            "NO AUTO-SAVE",
            self.p.yellow,
            width.min(96),
        );
        lines.extend(self.table(
            "STAGED SETTINGS",
            &["KEY", "ORIGINAL", "PROPOSED", "UNIT"],
            &table,
            width,
        ));
        if !editor.note.is_empty() {
            lines.extend(self.text(&escape_controls(&editor.note), width));
        }
        for hint in hints {
            lines.extend(self.text(&hint, width));
        }
        lines
    }

    pub(crate) fn settings(
        &self,
        cfg: &crate::Config,
        width: usize,
        interactive: bool,
    ) -> Vec<String> {
        use crate::ui::fmt_mib;
        let source = cfg.source_path.as_ref().map_or_else(
            || "built-in defaults".into(),
            |path| escape_controls(&path.display().to_string()),
        );
        let mut rows = self.banner(
            "WATCH SETTINGS",
            &format!("Source: {source}"),
            "READ ONLY UNTIL EDIT",
            self.p.cyan,
            width.min(96),
        );
        let t = &cfg.triggers;
        let n = &cfg.notifications;
        let toggle = |enabled| if enabled { "on" } else { "off" };
        let pct = |v: f64| {
            if v > 0.0 {
                format!("{v}%")
            } else {
                "off".into()
            }
        };
        let amount = |v: u64| if v > 0 { fmt_mib(v) } else { "off".into() };
        let triggers = [
            vec!["RAM used".into(), pct(t.ram_used_percent)],
            vec!["Available floor".into(), amount(t.min_available_mb)],
            vec![
                "Swap used % / MiB".into(),
                format!("{} / {}", pct(t.swap_used_percent), amount(t.swap_used_mb)),
            ],
            vec!["Tmpfs used".into(), pct(t.tmpfs_used_percent)],
            vec![
                "PSI some / full (avg10)".into(),
                format!("{} / {}", pct(t.psi_some_avg10), pct(t.psi_full_avg10)),
            ],
            vec![
                "Poll / cooldown / rearm".into(),
                format!(
                    "{}s / {}s / {} pp",
                    t.poll_interval_seconds, t.cooldown_seconds, t.hysteresis_percent
                ),
            ],
            vec![
                "Interval / schedules".into(),
                format!(
                    "{} / {}",
                    if t.interval_minutes == 0 {
                        "off".into()
                    } else {
                        format!("{}m", t.interval_minutes)
                    },
                    if t.schedules.is_empty() {
                        "none".into()
                    } else {
                        escape_controls(&t.schedules.join(", "))
                    }
                ),
            ],
        ];
        let actions = [
            vec![
                "Kernel / drop caches".into(),
                format!(
                    "{} / level {}{}",
                    toggle(cfg.kernel.enabled),
                    cfg.kernel.drop_caches_level,
                    if cfg.kernel.drop_caches_on_critical_only {
                        " (critical only)"
                    } else {
                        ""
                    }
                ),
            ],
            vec![
                "Swap / tmpfs".into(),
                format!(
                    "{} / {}",
                    toggle(cfg.swap.enabled),
                    toggle(cfg.tmpfs.enabled)
                ),
            ],
            vec![
                "User caches / watchdog".into(),
                format!(
                    "{} / {}",
                    toggle(cfg.user_caches.enabled),
                    toggle(cfg.watchdog.enabled)
                ),
            ],
            vec![
                "Notifications".into(),
                format!(
                    "{} (minimum {} MiB{})",
                    toggle(n.enabled),
                    n.min_freed_mb,
                    if n.on_critical_only {
                        ", critical only"
                    } else {
                        ""
                    }
                ),
            ],
            vec!["Logging".into(), escape_controls(&cfg.logging.level)],
        ];
        let when = |w| self.table("WHEN TO ACT", &["SETTING", "VALUE"], &triggers, w);
        let actions = |w| self.table("WHAT TO DO", &["SETTING", "VALUE"], &actions, w);
        if width >= 100 {
            rows.extend(self.pair(width, when, actions));
        } else {
            rows.extend(when(width));
            rows.extend(actions(width));
        }
        rows.extend(self.text(
            "Values used by this Watch, not the daemon's loaded settings.",
            width,
        ));
        if interactive {
            rows.extend(self.text(
                "[e] Edit existing config  |  [s] or Esc Back to Watch  |  [q] Quit",
                width,
            ));
        }
        rows
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
    fn board_groups_have_matching_edges_and_preserve_wrapped_text() {
        for color in [false, true] {
            let p = palette(color);
            let layout = Layout::board(&p);
            for width in [10, 24, 36, 80, 120, 200] {
                let content = "縦/this-is-a-long-path/end-marker";
                let groups = [
                    ("01  LIVE READINGS", layout.text(content, width - 4)),
                    (
                        "02  RESOURCE DETAILS",
                        layout.text("Pressure / load", width - 4),
                    ),
                    ("03  AUTO-CLEAN", layout.text("Status: normal", width - 4)),
                ];
                let rows = layout.board_groups(width, groups);
                let clean = rows.iter().map(|s| visible(&layout, s)).collect::<Vec<_>>();
                let edge = layout.width(&rows[0]);
                assert!(edge <= width);
                assert!(rows.iter().all(|row| layout.width(row) == edge));
                assert_eq!(
                    clean
                        .iter()
                        .filter(|r| r.starts_with('╭') && r.ends_with('╮'))
                        .count(),
                    3
                );
                assert_eq!(
                    clean
                        .iter()
                        .filter(|r| r.starts_with('╰') && r.ends_with('╯'))
                        .count(),
                    3
                );
                let body = clean
                    .iter()
                    .filter_map(|r| r.strip_prefix('│'))
                    .map(|r| r.strip_suffix('│').unwrap().trim())
                    .collect::<String>();
                assert!(body.contains(content));
                assert!(!body.contains('…'));
            }
        }
    }

    #[test]
    fn swap_table_aligns_columns_and_preserves_all_devices() {
        use crate::metrics::SwapDevice;
        let devices = [
            SwapDevice {
                path: "/dev/zram0".into(),
                swap_type: "partition".into(),
                size_mb: 4096,
                used_mb: 127,
                priority: 100,
            },
            SwapDevice {
                path: format!(
                    "/home/資料/{}/swapfile-SWAP-END",
                    "verylongdirectory/".repeat(5)
                ),
                swap_type: "file".into(),
                size_mb: 1024,
                used_mb: 2,
                priority: -2,
            },
        ];
        for color in [false, true] {
            let p = palette(color);
            let layout = Layout::new(&p);
            for width in [80, 120, 200] {
                let boxed = layout.swap_devices(&devices, width);
                let frame = layout.width(&boxed[0]);
                assert!(frame <= width && frame <= 87);
                assert!(boxed.iter().all(|line| layout.width(line) == frame));
                assert!(!boxed.join("").contains('…'));
                let table: Vec<_> = boxed
                    .iter()
                    .filter_map(|line| {
                        let row = visible(&layout, line);
                        let row = row.strip_prefix("│ ")?.strip_suffix(" │")?;
                        let cols: Vec<_> = row.split(" │ ").map(str::to_owned).collect();
                        (cols.len() == 4).then_some(cols)
                    })
                    .collect();
                let heading_widths = table[0]
                    .iter()
                    .map(|col| layout.width(col))
                    .collect::<Vec<_>>();
                assert_eq!(&heading_widths[1..], &[16, 7, 7]);
                for row in &table {
                    assert_eq!(
                        row.iter()
                            .map(|cell| layout.width(cell))
                            .collect::<Vec<_>>(),
                        heading_widths
                    );
                }
                assert_eq!(
                    table[0].iter().map(|col| col.trim()).collect::<Vec<_>>(),
                    ["DEVICE / PATH", "TYPE", "USED", "SIZE"]
                );
                assert_eq!(
                    table[1].iter().map(|col| col.trim()).collect::<Vec<_>>(),
                    ["/dev/zram0", "partition / zram", "127 MiB", "4.0 GiB"]
                );
                assert_eq!(
                    table[2..]
                        .iter()
                        .map(|row| row[0].trim())
                        .collect::<String>(),
                    devices[1].path
                );
                assert_eq!(
                    table[2][1..]
                        .iter()
                        .map(|col| col.trim())
                        .collect::<Vec<_>>(),
                    ["file", "2 MiB", "1.0 GiB"]
                );
            }
            for width in [24, 40, 59] {
                let rows = layout.swap_devices(&devices, width);
                let text = unpack(&layout, &rows.join(""));
                assert!(rows.iter().all(|line| layout.width(line) == width));
                assert!(text.contains("Device:/dev/zram0") && text.contains("Type:partition/zram"));
                assert!(text.contains(&format!("Device:{}", devices[1].path)));
                assert!(text.contains("Used:2MiBSize:1.0GiB"));
                assert!(!text.contains('…'));
            }
            assert!(layout.swap_devices(&[], 80).is_empty());
        }
    }

    #[test]
    fn watch_settings_uses_parser_fixture_without_hidden_values() {
        let mut cfg: crate::Config =
            toml::from_str("[triggers]\nram_used_percent = 87.0\n[notifications]\nenabled=true\n")
                .unwrap();
        assert!(cfg.validate().is_empty());
        cfg.source_path =
            Some("/sample/long/source/with/nested/parts/settings-SOURCE-END.toml".into());
        let p = palette(false);
        let layout = Layout::new(&p);
        for width in [32, 80, 120] {
            let rows = layout.settings(&cfg, width, true);
            assert!(rows.iter().all(|line| layout.width(line) <= width));
            let values = unpack(&layout, &rows.join(""))
                .replace(':', "")
                .replace("SETTING", "")
                .replace("VALUE", "");
            for item in [
                "READONLY",
                "SOURCE-END.toml",
                "RAMused87%",
                "Notificationson",
                "[s]orEscBacktoWatch",
                "[q]Quit",
            ] {
                assert!(values.contains(item), "{item}: {values}");
            }
            assert!(!values.contains('…'));
        }
        assert!(!layout.settings(&cfg, 80, false).join("").contains("[q]"));
    }

    #[test]
    fn settings_navigation_and_cli_gates_do_not_need_live_watch() {
        use crate::{watch_input, Cli, Commands, WatchInput, WatchView};
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["keep-ram-clean", "status", "--watch", "--view", "config"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Status {
                watch: true,
                view: Some(WatchView::Config),
                ..
            }
        ));
        assert!(Cli::try_parse_from(["keep-ram-clean", "status", "--view", "config"]).is_err());
        assert!(Cli::try_parse_from(["keep-ram-clean", "status", "--watch", "--json"]).is_ok());
        assert!(Cli::try_parse_from([
            "keep-ram-clean",
            "status",
            "--watch",
            "--json",
            "--view",
            "config"
        ])
        .is_err());
        assert_eq!(
            watch_input(WatchView::Dashboard, b's'),
            WatchInput::View(WatchView::Config)
        );
        assert_eq!(
            watch_input(WatchView::Config, b's'),
            WatchInput::View(WatchView::Dashboard)
        );
        assert_eq!(
            watch_input(WatchView::Config, 27),
            WatchInput::View(WatchView::Dashboard)
        );
        for view in [WatchView::Dashboard, WatchView::Config] {
            assert_eq!(watch_input(view, b'q'), WatchInput::Quit);
        }
        assert_eq!(watch_input(WatchView::Config, b'.'), WatchInput::Ignore);
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
