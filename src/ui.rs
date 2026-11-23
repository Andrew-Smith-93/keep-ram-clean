//! Pure-Rust full-width terminal visuals: responsive 2-column cards, gauges, and raw-key watch mode.

use std::env;
use std::io::IsTerminal;

pub struct Palette {
    pub color: bool,
    pub bold: &'static str,
    pub dim: &'static str,
    pub green: &'static str,
    pub cyan: &'static str,
    pub yellow: &'static str,
    pub red: &'static str,
    pub nc: &'static str,
}

impl Palette {
    pub fn detect() -> Self {
        let no_color = env::var_os("NO_COLOR")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let color = std::io::stdout().is_terminal() && !no_color;
        if color {
            Self {
                color: true,
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                green: "\x1b[0;32m",
                cyan: "\x1b[0;36m",
                yellow: "\x1b[1;33m",
                red: "\x1b[0;31m",
                nc: "\x1b[0m",
            }
        } else {
            Self {
                color: false,
                bold: "",
                dim: "",
                green: "",
                cyan: "",
                yellow: "",
                red: "",
                nc: "",
            }
        }
    }

    pub fn severity_color(&self, pct: f64, warn_at: f64, crit_at: f64) -> &'static str {
        if pct >= crit_at {
            self.red
        } else if pct >= warn_at {
            self.yellow
        } else {
            self.green
        }
    }
}

/// Detect full terminal width via TIOCGWINSZ (falls back to $COLUMNS or 110).
pub fn terminal_width() -> usize {
    terminal_width_with_minimum(60)
}

pub(crate) fn watch_terminal_width() -> usize {
    terminal_width_with_minimum(10)
}

fn terminal_width_with_minimum(minimum: usize) -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
        && usize::from(ws.ws_col) >= minimum
    {
        return ws.ws_col as usize;
    }
    if let Ok(cols) = env::var("COLUMNS") {
        if let Ok(n) = cols.parse::<usize>() {
            if n >= minimum {
                return n;
            }
        }
    }
    110
}

pub fn fmt_mib(mib: u64) -> String {
    if mib >= 1024 {
        format!("{:.1} GiB", mib as f64 / 1024.0)
    } else {
        format!("{mib} MiB")
    }
}

pub fn fmt_mib_f64(mib: f64) -> String {
    if mib >= 1024.0 {
        format!("{:.2} GiB", mib / 1024.0)
    } else {
        format!("{mib:.0} MiB")
    }
}

/// Count visible characters excluding ANSI CSI escape sequences (`\x1b[...m`).
pub fn visible_len(s: &str) -> usize {
    let mut count = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            count += 1;
        }
    }
    count
}

/// Truncate or pad an ANSI-colored string to exact visible width `width`.
pub fn fit_ansi(s: &str, width: usize, nc: &str) -> String {
    let vis = visible_len(s);
    if vis <= width {
        return format!("{s}{}", " ".repeat(width - vis));
    }
    let mut out = String::with_capacity(s.len());
    let mut seen = 0usize;
    let target = width.saturating_sub(1);
    let mut chars = s.chars().peekable();
    let mut saw_ansi = false;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            saw_ansi = true;
            out.push(ch);
            out.push(chars.next().unwrap());
            for c in chars.by_ref() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if seen >= target {
                out.push('…');
                break;
            }
            out.push(ch);
            seen += 1;
        }
    }
    if saw_ansi && !nc.is_empty() {
        out.push_str(nc);
    }
    out
}

pub fn gauge_bar(pct: f64, width: usize, warn_at: f64, crit_at: f64, p: &Palette) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let col = p.severity_color(clamped, warn_at, crit_at);
    format!(
        "{col}{}{}{}{}",
        "━".repeat(filled),
        p.dim,
        "─".repeat(empty),
        p.nc
    )
}

pub fn mini_bar(fraction: f64, width: usize, p: &Palette) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let col = if clamped >= 0.20 { p.yellow } else { p.cyan };
    format!(
        "{col}{}{}{}{}",
        "█".repeat(filled),
        p.dim,
        "░".repeat(empty),
        p.nc
    )
}

pub fn print_banner(title: &str, center: &str, badge: &str, badge_col: &str, width: usize, p: &Palette) {
    let inner = width.saturating_sub(2);
    let left_len = title.chars().count();
    let mid_len = visible_len(center);
    let right_len = badge.chars().count();
    let content = if !center.is_empty() && inner >= left_len + mid_len + right_len + 8 {
        let total_pad = inner - left_len - mid_len - right_len - 4;
        let pad_l = total_pad / 2;
        let pad_r = total_pad - pad_l;
        format!(
            "  {}{title}{}{}{center}{}{badge_col}{badge}{}  ",
            p.bold,
            p.nc,
            " ".repeat(pad_l),
            " ".repeat(pad_r),
            p.nc
        )
    } else {
        let pad = inner.saturating_sub(left_len + right_len + 4);
        format!(
            "  {}{title}{}{}{badge_col}{badge}{}  ",
            p.bold,
            p.nc,
            " ".repeat(pad),
            p.nc
        )
    };
    println!("{}╭{}╮{}", p.cyan, "─".repeat(inner), p.nc);
    println!("{}│{}{}{}│{}", p.cyan, p.nc, fit_ansi(&content, inner, p.nc), p.cyan, p.nc);
    println!("{}╰{}╯{}", p.cyan, "─".repeat(inner), p.nc);
}

fn panel_top(title: &str, badge: &str, badge_col: &str, width: usize, p: &Palette) -> String {
    let inner = width.saturating_sub(2);
    let title_part = format!(" {title} ");
    let badge_part = if badge.is_empty() {
        String::new()
    } else {
        format!(" {badge} ")
    };
    let used = 1 + title_part.chars().count() + badge_part.chars().count() + if badge.is_empty() { 0 } else { 1 };
    let fill = inner.saturating_sub(used);
    if badge.is_empty() {
        format!(
            "{}╭─{}{}{title_part}{}{}{}╮{}",
            p.dim, p.nc, p.bold, p.nc, p.dim, "─".repeat(fill), p.nc
        )
    } else {
        format!(
            "{}╭─{}{}{title_part}{}{}{}{badge_col}{badge_part}{}{}{}─╮{}",
            p.dim,
            p.nc,
            p.bold,
            p.nc,
            p.dim,
            "─".repeat(fill),
            p.nc,
            p.dim,
            "",
            p.nc
        )
    }
}

fn panel_bottom(width: usize, p: &Palette) -> String {
    let inner = width.saturating_sub(2);
    format!("{}╰{}╯{}", p.dim, "─".repeat(inner), p.nc)
}

fn panel_row(line: &str, width: usize, p: &Palette) -> String {
    let content_w = width.saturating_sub(4);
    format!(
        "{}│{} {} {}│{}",
        p.dim,
        p.nc,
        fit_ansi(line, content_w, p.nc),
        p.dim,
        p.nc
    )
}

pub fn print_panel(
    title: &str,
    badge: &str,
    badge_col: &str,
    lines: &[String],
    width: usize,
    p: &Palette,
) {
    println!("{}", panel_top(title, badge, badge_col, width, p));
    for line in lines {
        println!("{}", panel_row(line, width, p));
    }
    println!("{}", panel_bottom(width, p));
}

pub struct Panel<'a> {
    pub title: &'a str,
    pub badge: &'a str,
    pub badge_col: &'a str,
    pub lines: &'a [String],
}

/// Render two equal-height rounded panels side-by-side spanning `total_width`.
pub fn print_two_panels(left: &Panel<'_>, right: &Panel<'_>, total_width: usize, p: &Palette) {
    let gap = 1usize;
    let left_w = (total_width - gap) / 2;
    let right_w = total_width - gap - left_w;
    let rows = left.lines.len().max(right.lines.len());

    println!(
        "{} {}",
        panel_top(left.title, left.badge, left.badge_col, left_w, p),
        panel_top(right.title, right.badge, right.badge_col, right_w, p)
    );
    for i in 0..rows {
        let l = left.lines.get(i).map(|s| s.as_str()).unwrap_or("");
        let r = right.lines.get(i).map(|s| s.as_str()).unwrap_or("");
        println!("{} {}", panel_row(l, left_w, p), panel_row(r, right_w, p));
    }
    println!("{} {}", panel_bottom(left_w, p), panel_bottom(right_w, p));
}

pub fn psi_badge(
    some: f64,
    full: f64,
    some_thr: f64,
    full_thr: f64,
    p: &Palette,
) -> String {
    if (full_thr > 0.0 && full >= full_thr) || full >= 2.0 {
        format!("{}{}PRESSURE{}", p.bold, p.red, p.nc)
    } else if (some_thr > 0.0 && some >= some_thr) || some >= 5.0 {
        format!("{}{}ELEVATED{}", p.bold, p.yellow, p.nc)
    } else {
        format!("{}{}CALM{}", p.bold, p.green, p.nc)
    }
}

pub fn truncate_str(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else if max_chars > 1 {
        let mut out: String = s.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// RAII guard for non-canonical, no-echo stdin in `--watch` mode (auto-restores on drop).
pub struct RawTerminalGuard {
    orig: Option<libc::termios>,
}

impl RawTerminalGuard {
    pub fn enter() -> Self {
        if !std::io::stdin().is_terminal() {
            return Self { orig: None };
        }
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut orig) } != 0 {
            return Self { orig: None };
        }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) };
        Self { orig: Some(orig) }
    }

    pub fn poll_key(&self, timeout_ms: i32) -> Option<u8> {
        if self.orig.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(0) as u64));
            return None;
        }
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let mut b = 0u8;
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    &mut b as *mut u8 as *mut libc::c_void,
                    1,
                )
            };
            if n == 1 {
                return Some(b);
            }
        }
        None
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        if let Some(orig) = &self.orig {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_and_ansi_width_work() {
        assert_eq!(fmt_mib(512), "512 MiB");
        assert_eq!(fmt_mib(2048), "2.0 GiB");
        assert_eq!(fmt_mib_f64(850.4), "850 MiB");
        assert_eq!(fmt_mib_f64(1536.0), "1.50 GiB");
        assert_eq!(truncate_str("language_server", 10), "language_…");
        assert_eq!(visible_len("\x1b[1m\x1b[0;32mOK\x1b[0m"), 2);
    }
}
