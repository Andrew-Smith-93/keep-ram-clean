//! Pure-Rust terminal visuals: rounded headers, color-coded gauges, and status badges.

use std::env;
use std::io::IsTerminal;

pub const BOX_WIDTH: usize = 68;

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

pub fn gauge_bar(pct: f64, width: usize, warn_at: f64, crit_at: f64, p: &Palette) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let col = p.severity_color(clamped, warn_at, crit_at);
    format!(
        "{col}{}{}{}{}{}",
        "━".repeat(filled),
        p.dim,
        "─".repeat(empty),
        p.nc,
        ""
    )
}

pub fn mini_bar(fraction: f64, width: usize, p: &Palette) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let col = if clamped >= 0.20 {
        p.yellow
    } else {
        p.cyan
    };
    format!(
        "{col}{}{}{}{}",
        "█".repeat(filled),
        p.dim,
        "░".repeat(empty),
        p.nc
    )
}

pub fn print_banner(title: &str, badge_text: &str, badge_color: &str, p: &Palette) {
    let inner = BOX_WIDTH - 2;
    let left_len = title.chars().count();
    let right_len = badge_text.chars().count();
    let pad = inner.saturating_sub(left_len + right_len + 4);
    println!("{}╭{}╮{}", p.cyan, "─".repeat(inner), p.nc);
    println!(
        "{}│{}  {}{title}{}{}{badge_color}{badge_text}{}  {}│{}",
        p.cyan,
        p.nc,
        p.bold,
        p.nc,
        " ".repeat(pad),
        p.nc,
        p.cyan,
        p.nc
    );
    println!("{}╰{}╯{}", p.cyan, "─".repeat(inner), p.nc);
}

pub fn print_section(title: &str, p: &Palette) {
    let prefix = format!("── {title} ");
    let fill = BOX_WIDTH.saturating_sub(prefix.chars().count());
    println!(
        "\n{}{prefix}{}{}{}",
        p.bold,
        p.dim,
        "─".repeat(fill),
        p.nc
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_helpers_work() {
        assert_eq!(fmt_mib(512), "512 MiB");
        assert_eq!(fmt_mib(2048), "2.0 GiB");
        assert_eq!(fmt_mib_f64(850.4), "850 MiB");
        assert_eq!(fmt_mib_f64(1536.0), "1.50 GiB");
        assert_eq!(truncate_str("language_server", 10), "language_…");
    }
}
