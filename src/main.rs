//! Command-line interface for keep-ram-clean.

mod cleaners;
mod config;
mod daemon;
mod doctor;
mod metrics;
mod pressure_relief;
mod triggers;
mod ui;
mod watch_layout;
mod watch_editor;
mod watch_tables;

use std::fs::{self, File};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde_json::json;

use crate::cleaners::{run_cleanup, ALL_STEP_NAMES};
use crate::config::{
    default_user_config, expand_tilde, is_root, load_config, update_config_setting, Config,
    DEFAULT_SYSTEM_CONFIG, EXAMPLE_CONFIG_TOML,
};
use crate::daemon::{read_daemon_state, signal_daemon, RamCleanDaemon, VERSION};
use crate::doctor::run_doctor;
use crate::metrics::{collect_snapshot, top_memory_processes};
use crate::ui::{
    fmt_mib, fmt_mib_f64, gauge_bar, mini_bar, print_banner, print_panel, print_two_panels,
    psi_badge, terminal_width, truncate_str, Palette, Panel, RawTerminalGuard,
};

#[cfg(test)]
static WATCH_KEY_FIXTURE: AtomicBool = AtomicBool::new(false);
static WATCH_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_watch_sigint(_sig: libc::c_int) {
    WATCH_STOP.store(true, Ordering::SeqCst);
}

fn fmt_ts(ts: f64) -> String {
    let secs = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
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

fn fmt_hms(ts: f64) -> String {
    let secs = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[derive(Parser)]
#[command(
    name = "keep-ram-clean",
    version = VERSION,
    about = "Configurable Linux RAM, tmpfs, swap, and memory-pressure management daemon and CLI"
)]
struct Cli {
    /// Path to TOML configuration file
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a one-shot cleanup immediately
    Run {
        /// Preview actions without modifying anything
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated subset of steps (tmpfs,watchdog,user_caches,kernel,swap)
        #[arg(long)]
        only: Option<String>,
        /// Treat run as critical memory pressure
        #[arg(long)]
        critical: bool,
        /// Output report as JSON
        #[arg(long)]
        json: bool,
        /// Show detailed step output
        #[arg(short, long)]
        verbose: bool,
    },
    /// Start the background monitoring & cleanup daemon
    Daemon {
        /// Log what the daemon would do without modifying anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Watch explicit user workloads for severe memory pressure; reports by default
    PressureWatch {
        /// Exact PID of an already-running workload; repeat for multiple PIDs
        #[arg(long, required_unless_present = "discover")]
        workload_pid: Vec<u32>,
        /// Discover only exact allowlisted user-owned executables when pressure persists
        #[arg(long, conflicts_with = "workload_pid")]
        discover: bool,
        /// Exact canonical path to a user-owned executable; repeat as needed
        #[arg(long, required = true)]
        allow_exe: Vec<PathBuf>,
        /// Explicitly allow one SIGTERM per registered workload; never escalates
        #[arg(long)]
        terminate: bool,
    },
    /// Show live RAM, PSI, swap, tmpfs, auto-clean activity, and daemon status
    Status {
        /// Output status as JSON
        #[arg(long)]
        json: bool,
        /// Number of top memory processes to display
        #[arg(long, default_value_t = 5)]
        top: usize,
        /// Interactive live-refreshing dashboard mode (s for Settings)
        #[arg(short, long)]
        watch: bool,
        /// Start Watch in the dashboard or settings
        #[arg(long, value_enum, requires = "watch", conflicts_with = "json")]
        view: Option<WatchView>,
        /// Refresh interval in seconds for --watch mode
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// View or update auto-clean trigger and notification settings
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Audit kernel VM settings and memory health
    Doctor {
        /// Output audit findings as JSON
        #[arg(long)]
        json: bool,
    },
    /// Generate a documented starter config.toml
    InitConfig {
        /// Destination config file path
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite existing config file
        #[arg(short, long)]
        force: bool,
    },
    /// Send SIGHUP to running daemon to reload config
    Reload,
    /// Send SIGUSR1 to running daemon for immediate cleanup
    Trigger,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current auto-clean trigger & notification settings
    Show,
    /// Update a setting and hot-reload the running daemon
    Set {
        /// Setting name (e.g. ram-percent, min-avail-mb, swap-percent, cooldown, interval, notify)
        key: String,
        /// New value (e.g. 75, 180, true, false)
        value: String,
    },
}

fn cmd_run(
    cfg_path: Option<PathBuf>,
    dry_run: bool,
    only: Option<String>,
    critical: bool,
    as_json: bool,
    verbose: bool,
) -> i32 {
    let p = Palette::detect();
    let cfg = match load_config(cfg_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let only_vec: Option<Vec<String>> = if let Some(raw) = only {
        let items: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let bad: Vec<&str> = items
            .iter()
            .filter(|s| !ALL_STEP_NAMES.contains(&s.as_str()))
            .map(|s| s.as_str())
            .collect();
        if !bad.is_empty() {
            eprintln!(
                "Unknown step(s): {}. Valid choices: {}",
                bad.join(", "),
                ALL_STEP_NAMES.join(", ")
            );
            return 2;
        }
        Some(items)
    } else {
        None
    };

    let report = run_cleanup(
        &cfg,
        vec!["Manual CLI run".into()],
        dry_run,
        critical,
        only_vec.as_deref(),
    );
    let code = if report.has_errors { 1 } else { 0 };

    if as_json {
        let payload = json!({
            "version": VERSION,
            "config_path": cfg.source_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            "dry_run": report.dry_run,
            "critical": report.critical,
            "reasons": report.reasons,
            "started_at": report.started_at,
            "finished_at": report.finished_at,
            "duration_seconds": report.duration_seconds,
            "pre_available_mb": report.pre_available_mb,
            "post_available_mb": report.post_available_mb,
            "ram_freed_mb": report.ram_freed_mb,
            "pre_swap_used_mb": report.pre_swap_used_mb,
            "post_swap_used_mb": report.post_swap_used_mb,
            "swap_freed_mb": report.swap_freed_mb,
            "has_errors": report.has_errors,
            "steps": report.steps,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return code;
    }

    let w = terminal_width();
    let (badge, badge_col) = if report.dry_run {
        ("◎ DRY-RUN", p.yellow)
    } else if report.has_errors {
        ("✖ ERRORS", p.red)
    } else {
        ("✔ COMPLETE", p.green)
    };
    let center = format!(
        "RAM {} ({:+} MiB) │ Swap {} ({:+} MiB) │ {:.2}s",
        fmt_mib(report.post_available_mb),
        report.ram_freed_mb,
        fmt_mib(report.post_swap_used_mb),
        -report.swap_freed_mb,
        report.duration_seconds
    );
    print_banner(&format!("keep-ram-clean v{VERSION} · Cleanup"), &center, badge, badge_col, w, &p);

    let mut lines = Vec::new();
    for step in &report.steps {
        let (icon, col) = match step.status.as_str() {
            "ok" => ("✔", p.green),
            "dry-run" => ("◎", p.cyan),
            "skipped" => ("–", p.yellow),
            _ => ("✖", p.red),
        };
        lines.push(format!(
            "{col}{icon}{} {}{:<12}{} {}",
            p.nc, p.bold, step.name, p.nc, step.summary
        ));
        if verbose || report.dry_run || step.status == "error" {
            for d in &step.details {
                lines.push(format!("    {}• {d}{}", p.dim, p.nc));
            }
        }
    }
    let cfg_label = cfg
        .source_path
        .as_ref()
        .map(|c| c.display().to_string())
        .unwrap_or_else(|| "built-in defaults".into());
    print_panel("CLEANUP PIPELINE", &cfg_label, p.dim, &lines, w, &p);
    code
}

fn render_status_dashboard(
    cfg: &Config,
    top: usize,
    p: &Palette,
    toast: Option<&str>,
    watch_scroll: Option<&mut usize>,
    interval: u64,
) {
    #[cfg(test)]
    if WATCH_KEY_FIXTURE.load(Ordering::Relaxed) {
        println!("FIXTURE DASHBOARD");
        return;
    }

    let w = if watch_scroll.is_some() { ui::watch_terminal_width() } else { terminal_width() };
    let two_col = w >= 96;
    let col_inner = if two_col {
        ((w - 1) / 2).saturating_sub(4)
    } else {
        w.saturating_sub(4)
    };
    let bar_w = col_inner.saturating_sub(34).clamp(10, 26);

    let snap = collect_snapshot(Some(&cfg.tmpfs.paths), true, true, true, true);
    let top_procs = top_memory_processes(top);
    let daemon_state = read_daemon_state(None);
    if let Some(offset) = watch_scroll {
        let report = watch_tables::Report {
            config: cfg, snapshot: &snap, processes: &top_procs,
            daemon: daemon_state.as_ref(), toast, interval,
        };
        let rows = report.render(w, p);
        let (rows, clipped) = if p.color {
            watch_tables::viewport(rows, w, ui::watch_terminal_height(), offset)
        } else {
            (rows, false)
        };
        let last = rows.len().saturating_sub(1);
        for (i, line) in rows.into_iter().enumerate() {
            if clipped && i == last { print!("{line}"); } else { println!("{line}"); }
        }
        return;
    }

    let mem = &snap.memory;
    let t = &cfg.triggers;

    let cleaning_now = daemon_state
        .as_ref()
        .map(|d| d.alive && d.cleaning_now)
        .unwrap_or(false);
    let (badge, badge_col) = if cleaning_now {
        ("⚡ AUTO-CLEANING", p.yellow)
    } else if daemon_state.as_ref().map(|d| d.alive).unwrap_or(false) {
        ("● DAEMON RUNNING", p.green)
    } else {
        ("○ DAEMON STOPPED", p.yellow)
    };

    let center = format!(
        "RAM {:.0}% ({} free) │ Swap {:.0}% │ Load {:.2}",
        mem.used_percent(),
        fmt_mib(mem.available_mb),
        mem.swap_used_percent(),
        snap.load_avg_1m
    );
    print_banner(&format!("keep-ram-clean v{VERSION}"), &center, badge, badge_col, w, p);

    // --- Left Panel: MEMORY, SWAP & TMPFS ---
    let mut left_lines: Vec<String> = Vec::new();
    let ram_pct = mem.used_percent();
    let ram_crit = if t.ram_used_percent > 0.0 {
        t.ram_used_percent
    } else {
        80.0
    };
    let ram_warn = (ram_crit - 15.0).max(50.0);
    let ram_bar = gauge_bar(ram_pct, bar_w, ram_warn, ram_crit, p);
    let ram_col = p.severity_color(ram_pct, ram_warn, ram_crit);
    left_lines.push(format!(
        "{:<5} {ram_bar} {ram_col}{:>5.1}%{} {}{}/{}{}",
        "RAM",
        ram_pct,
        p.nc,
        p.bold,
        fmt_mib(mem.used_mb),
        fmt_mib(mem.total_mb),
        p.nc
    ));

    let swap_pct = mem.swap_used_percent();
    let swap_crit = if t.swap_used_percent > 0.0 {
        t.swap_used_percent
    } else {
        50.0
    };
    let swap_warn = (swap_crit - 20.0).max(20.0);
    let swap_bar = gauge_bar(swap_pct, bar_w, swap_warn, swap_crit, p);
    let swap_col = p.severity_color(swap_pct, swap_warn, swap_crit);
    left_lines.push(format!(
        "{:<5} {swap_bar} {swap_col}{:>5.1}%{} {}{}/{}{}",
        "Swap",
        swap_pct,
        p.nc,
        p.bold,
        fmt_mib(mem.swap_used_mb),
        fmt_mib(mem.swap_total_mb),
        p.nc
    ));

    let tmp_crit = if t.tmpfs_used_percent > 0.0 {
        t.tmpfs_used_percent
    } else {
        70.0
    };
    let tmp_warn = (tmp_crit - 20.0).max(40.0);
    for m in &snap.tmpfs_mounts {
        let pct = m.used_percent();
        let bar = gauge_bar(pct, bar_w, tmp_warn, tmp_crit, p);
        let col = p.severity_color(pct, tmp_warn, tmp_crit);
        left_lines.push(format!(
            "{:<5} {bar} {col}{:>5.1}%{} {}/{}",
            truncate_str(&m.mountpoint, 5),
            pct,
            p.nc,
            fmt_mib(m.used_mb),
            fmt_mib(m.total_mb)
        ));
    }

    left_lines.push(format!(
        "{}Cache{} {} │ {}Slab{} {} │ {}Dirty{} {}",
        p.dim,
        p.nc,
        fmt_mib(mem.cached_mb),
        p.dim,
        p.nc,
        fmt_mib(mem.slab_reclaimable_mb),
        p.dim,
        p.nc,
        fmt_mib(mem.dirty_mb)
    ));

    if !snap.swaps.is_empty() {
        let dev_summary: Vec<String> = snap
            .swaps
            .iter()
            .map(|s| {
                let tag = if s.is_zram() { "[zram]" } else { "" };
                format!(
                    "{}{tag} {}/{}",
                    s.path,
                    fmt_mib(s.used_mb),
                    fmt_mib(s.size_mb)
                )
            })
            .collect();
        left_lines.push(format!("{}Swaps{} {}", p.dim, p.nc, dev_summary.join(" · ")));
    }

    // --- Right Panel: PRESSURE & TOP CONSUMERS ---
    let mut right_lines: Vec<String> = Vec::new();
    if snap.psi.available {
        let pb = psi_badge(
            snap.psi.some_avg10,
            snap.psi.full_avg10,
            t.psi_some_avg10,
            t.psi_full_avg10,
            p,
        );
        right_lines.push(format!(
            "PSI {pb}  some {:.2}/{:.2} │ full {:.2}/{:.2}  {}Load {:.2} ({}c){}",
            snap.psi.some_avg10,
            snap.psi.some_avg60,
            snap.psi.full_avg10,
            snap.psi.full_avg60,
            p.dim,
            snap.load_avg_1m,
            snap.cpu_count,
            p.nc
        ));
    } else {
        right_lines.push(format!(
            "Load {:.2} {:.2} {:.2} ({} cores · ratio {:.2})",
            snap.load_avg_1m,
            snap.load_avg_5m,
            snap.load_avg_15m,
            snap.cpu_count,
            snap.load_ratio()
        ));
    }

    let total_f = mem.total_mb.max(1) as f64;
    let name_w = col_inner.saturating_sub(36).clamp(10, 18);
    for proc in &top_procs {
        let frac = proc.rss_mb / total_f;
        let bar = mini_bar(frac, 8, p);
        right_lines.push(format!(
            "{}{:<7}{} {:<name_w$} {bar} {:>8} {}({:>4.1}%){}",
            p.dim,
            proc.pid,
            p.nc,
            truncate_str(&proc.comm, name_w),
            fmt_mib_f64(proc.rss_mb),
            p.dim,
            frac * 100.0,
            p.nc
        ));
    }

    if two_col {
        let free_badge = format!("{} free", fmt_mib(mem.available_mb));
        let top_badge = format!("Top {}", top_procs.len());
        print_two_panels(
            &Panel {
                title: "MEMORY · SWAP · TMPFS",
                badge: &free_badge,
                badge_col: p.green,
                lines: &left_lines,
            },
            &Panel {
                title: "PRESSURE & TOP CONSUMERS",
                badge: &top_badge,
                badge_col: p.cyan,
                lines: &right_lines,
            },
            w,
            p,
        );
    } else {
        print_panel("MEMORY · SWAP · TMPFS", "", "", &left_lines, w, p);
        print_panel("PRESSURE & TOP CONSUMERS", "", "", &right_lines, w, p);
    }

    // Bottom panel shares the same activity fields and history limit as before.
    let auto_lines = status_activity_lines(cfg, p, daemon_state.as_ref(), str::to_owned);

    let (act_badge, act_col) = if cleaning_now {
        ("CLEANING", p.yellow)
    } else {
        ("STANDBY", p.dim)
    };
    print_panel("AUTO-CLEAN ACTIVITY & TRIGGERS", act_badge, act_col, &auto_lines, w, p);
}

fn status_activity_lines(
    cfg: &Config,
    p: &Palette,
    daemon_state: Option<&daemon::DaemonState>,
    terminal_value: impl Fn(&str) -> String,
) -> Vec<String> {
    const ACTIVITY_MARKER: &str = "*";
    let t = &cfg.triggers;
    let mut auto_lines: Vec<String> = Vec::new();
    let notif_pill = if cfg.notifications.enabled {
        format!("{}ON{}", p.green, p.nc)
    } else {
        format!("{}OFF{}", p.dim, p.nc)
    };
    let sched_str = if t.interval_minutes > 0 {
        format!("Every {}m", t.interval_minutes)
    } else if !t.schedules.is_empty() {
        t.schedules.join(",")
    } else {
        "Reactive".to_string()
    };
    auto_lines.push(format!(
        "{}Triggers{} RAM ≥ {}{:.0}%{} │ Avail ≤ {} │ Swap ≥ {:.0}% │ PSI ≥ {:.0}/{:.0} │ Cooldown {}s │ Mode {} │ Alerts {notif_pill}",
        p.bold,
        p.nc,
        p.cyan,
        t.ram_used_percent,
        p.nc,
        fmt_mib(t.min_available_mb),
        t.swap_used_percent,
        t.psi_some_avg10,
        t.psi_full_avg10,
        t.cooldown_seconds,
        terminal_value(&sched_str)
    ));

    if let Some(ds) = daemon_state.filter(|d| d.alive) {
        if ds.cleaning_now {
            let why = terminal_value(ds.cleaning_reason.as_deref().unwrap_or("Trigger fired"));
            auto_lines.push(format!(
                "{}{}{ACTIVITY_MARKER} AUTO-CLEAN IN PROGRESS — {why}{}",
                p.bold, p.yellow, p.nc
            ));
        } else {
            auto_lines.push(format!(
                "{}● Active{} PID {} (since {}) │ Cleanups: {}{}{} │ Total Freed: {}{:+} MiB RAM{}, {}{:+} MiB swap{}",
                p.green,
                p.nc,
                ds.pid,
                fmt_ts(ds.started_at),
                p.bold,
                ds.cleanups_run,
                p.nc,
                p.green,
                ds.total_ram_freed_mb,
                p.nc,
                p.cyan,
                -ds.total_swap_freed_mb,
                p.nc
            ));
        }

        let history: Vec<&daemon::LastCleanupState> = if !ds.recent_cleanups.is_empty() {
            ds.recent_cleanups.iter().take(3).collect()
        } else {
            ds.last_cleanup.iter().collect()
        };
        if history.is_empty() {
            auto_lines.push(format!(
                "{}History{}  No auto-cleanups triggered yet — standing by (run 'keep-ram-clean trigger' or press 'c' in --watch to test)",
                p.dim, p.nc
            ));
        } else {
            for ev in history {
                let step_icons: String = ev
                    .steps
                    .iter()
                    .map(|s| match s.status.as_str() {
                        "ok" => format!("{}✔{}{}", p.green, terminal_value(&s.name), p.nc),
                        "error" => format!("{}✖{}{}", p.red, terminal_value(&s.name), p.nc),
                        _ => format!("{}–{}{}", p.dim, terminal_value(&s.name), p.nc),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                auto_lines.push(format!(
                    "{}{ACTIVITY_MARKER} {}{}  {}{:+} MiB RAM{}  {}{:+} MiB swap{}  ({:.2}s)  {}  [{}]",
                    p.cyan,
                    fmt_hms(ev.finished_at),
                    p.nc,
                    if ev.ram_freed_mb > 0 { p.green } else { p.dim },
                    ev.ram_freed_mb,
                    p.nc,
                    if ev.swap_freed_mb > 0 { p.cyan } else { p.dim },
                    -ev.swap_freed_mb,
                    p.nc,
                    ev.duration_seconds,
                    terminal_value(&ev.reasons.join("; ")),
                    step_icons
                ));
            }
        }
    } else {
        auto_lines.push(format!(
            "{}○ Daemon not running{} — start with `keep-ram-clean daemon` or systemd service",
            p.yellow, p.nc
        ));
    }

    auto_lines
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WatchView {
    Dashboard,
    Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchInput {
    Quit,
    View(WatchView),
    DashboardKey(u8),
    Ignore,
}

fn watch_input(view: WatchView, key: u8) -> WatchInput {
    match (view, key) {
        (WatchView::Dashboard, b'q' | b'Q' | 3) | (WatchView::Config, b'q') => WatchInput::Quit,
        (WatchView::Dashboard, b's' | b'S') => WatchInput::View(WatchView::Config),
        (WatchView::Config, b's' | 27) => WatchInput::View(WatchView::Dashboard),
        (WatchView::Config, _) => WatchInput::Ignore,
        (_, key) => WatchInput::DashboardKey(key),
    }
}

fn render_watch_settings(
    cfg: &Config,
    p: &Palette,
    editor: Option<&watch_editor::Editor>,
    message: &str,
    page: &mut watch_layout::SettingsPage,
) {
    let layout = watch_layout::Layout::new(p);
    let width = ui::watch_terminal_width();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let height = if interactive { ui::watch_terminal_height() } else { 0 };
    layout.print(layout.settings(cfg, (width, height), interactive, editor, message, page));
}

fn cmd_status(
    cfg_path: Option<PathBuf>,
    as_json: bool,
    top: usize,
    watch: bool,
    interval: u64,
    start_view: Option<WatchView>,
) -> i32 {
    let p = Palette::detect();
    let load = if watch && !as_json { config::load_watch_config } else { load_config };
    let mut cfg = match load(cfg_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    if as_json {
        let snap = collect_snapshot(Some(&cfg.tmpfs.paths), true, true, true, true);
        let top_procs = top_memory_processes(top);
        let daemon_state = read_daemon_state(None);
        let payload = json!({
            "version": VERSION,
            "memory": {
                "total_mb": snap.memory.total_mb,
                "used_mb": snap.memory.used_mb,
                "available_mb": snap.memory.available_mb,
                "used_percent": snap.memory.used_percent(),
                "buffers_mb": snap.memory.buffers_mb,
                "cached_mb": snap.memory.cached_mb,
                "slab_reclaimable_mb": snap.memory.slab_reclaimable_mb,
                "dirty_mb": snap.memory.dirty_mb,
                "swap_total_mb": snap.memory.swap_total_mb,
                "swap_free_mb": snap.memory.swap_free_mb,
                "swap_used_mb": snap.memory.swap_used_mb,
                "swap_used_percent": snap.memory.swap_used_percent(),
            },
            "psi": snap.psi,
            "load_avg": {
                "1m": snap.load_avg_1m,
                "5m": snap.load_avg_5m,
                "15m": snap.load_avg_15m,
                "cpus": snap.cpu_count,
                "ratio": snap.load_ratio(),
            },
            "swaps": snap.swaps.iter().map(|s| json!({
                "path": s.path,
                "type": s.swap_type,
                "size_mb": s.size_mb,
                "used_mb": s.used_mb,
                "priority": s.priority,
                "is_zram": s.is_zram(),
            })).collect::<Vec<_>>(),
            "tmpfs": snap.tmpfs_mounts.iter().map(|m| json!({
                "mountpoint": m.mountpoint,
                "total_mb": m.total_mb,
                "used_mb": m.used_mb,
                "free_mb": m.free_mb,
                "used_percent": m.used_percent(),
            })).collect::<Vec<_>>(),
            "top_processes": top_procs,
            "daemon": daemon_state,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return 0;
    }

    if !watch {
        render_status_dashboard(&cfg, top, &p, None, None, interval);
        return 0;
    }

    let mut view = start_view.unwrap_or(WatchView::Dashboard);
    if view == WatchView::Config
        && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    {
        render_watch_settings(&cfg, &p, None, "", &mut watch_layout::SettingsPage::default());
        return 0;
    }

    let sleep_secs = interval.max(1);
    WATCH_STOP.store(false, Ordering::SeqCst);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_watch_sigint as *const () as usize;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }

    let mut term = RawTerminalGuard::enter();
    term.settings_mode(view == WatchView::Config);
    let mut editor = None;
    let mut watch_scroll = 0usize;
    let mut settings_message = String::new();
    let mut settings_page = watch_layout::SettingsPage::default();
    let mut first = true;
    let mut toast: Option<(String, Instant)> = None;

    while !WATCH_STOP.load(Ordering::SeqCst) {
        if let Some((_, exp)) = &toast {
            if Instant::now() >= *exp {
                toast = None;
            }
        }
        if p.color {
            if first {
                print!("\x1b[2J\x1b[H");
                first = false;
            } else {
                print!("\x1b[H\x1b[J");
            }
        }
        if view == WatchView::Config {
            render_watch_settings(&cfg, &p, editor.as_ref(), &settings_message, &mut settings_page);
        } else {
            render_status_dashboard(
                &cfg, top, &p, toast.as_ref().map(|(m, _)| m.as_str()), Some(&mut watch_scroll), sleep_secs,
            );
        }
        let _ = std::io::stdout().flush();

        let deadline = Instant::now() + Duration::from_secs(sleep_secs);
        while Instant::now() < deadline && !WATCH_STOP.load(Ordering::SeqCst) {
            if let Some(key) = term.poll_key(100) {
                if view == WatchView::Config && matches!(key, b'[' | b']') {
                    settings_page.step(key == b']');
                    break;
                }
                if let Some(active) = editor.as_mut() {
                    use watch_editor::Outcome;
                    let outcome = active.input(key);
                    if let Some(selected) = active.selecting() {
                        settings_page.selection = selected;
                    }
                    if matches!(outcome, Outcome::Ignore) { continue; }
                    settings_page.follow();
                    match outcome {
                        Outcome::Ignore => unreachable!(),
                        Outcome::Continue => {}
                        Outcome::Cancel => {
                            editor = None;
                            settings_message = "Draft discarded; no file written.".into();
                        }
                        Outcome::Quit => { WATCH_STOP.store(true, Ordering::SeqCst); }
                        Outcome::Save => match active.draft.save() {
                            Ok(saved) => {
                                cfg = saved;
                                editor = None;
                                settings_message = "Saved the selected file. No daemon reload was sent.".into();
                            }
                            Err(error) => active.note = error,
                        },
                    }
                    break;
                }
                if view == WatchView::Config {
                    match key {
                        b'j' | b'k' => {
                            settings_page.selection = watch_editor::next_field(settings_page.selection, key == b'j');
                            settings_page.follow();
                            break;
                        }
                        b'e' | b'\r' | b'\n' => {
                            settings_page.follow();
                            match watch_editor::Editor::open_at(&cfg, settings_page.preset(), settings_page.selection) {
                                Ok(mut opened) => {
                                    if key != b'e' {
                                        opened.input(key);
                                    }
                                    editor = Some(opened);
                                    settings_message.clear();
                                }
                                Err(error) => settings_message = format!("Cannot edit: {error}"),
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                let key = match watch_input(view, key) {
                    WatchInput::Quit => {
                        WATCH_STOP.store(true, Ordering::SeqCst);
                        break;
                    }
                    WatchInput::View(next) => {
                        settings_message.clear();
                        settings_page.follow();
                        view = next;
                        term.settings_mode(view == WatchView::Config);
                        break;
                    }
                    WatchInput::Ignore => continue,
                    WatchInput::DashboardKey(key) => key,
                };
                #[cfg(test)]
                if WATCH_KEY_FIXTURE.load(Ordering::Relaxed) && matches!(key, b'c' | b'C' | b'r' | b'R' | b'+' | b'=' | b'-' | b'_' | b'n' | b'N') {
                    println!("FIXTURE ACTION {}", key as char);
                    break;
                }
                let msg = match key {
                    b'j' if p.color => { watch_scroll = watch_scroll.saturating_add(1); break; }
                    b'k' if p.color => { watch_scroll = watch_scroll.saturating_sub(1); break; }
                    b'c' | b'C' => match signal_daemon(libc::SIGUSR1, "SIGUSR1", None) {
                        Ok(m) => Some(format!("⚡ {m}")),
                        Err(e) => Some(format!("✖ {e}")),
                    },
                    b'r' | b'R' => match signal_daemon(libc::SIGHUP, "SIGHUP", None) {
                        Ok(m) => {
                            if let Ok(reloaded) = load(cfg_path.as_deref()) {
                                cfg = reloaded;
                            }
                            Some(format!("✔ {m}"))
                        }
                        Err(e) => Some(format!("✖ {e}")),
                    },
                    b'+' | b'=' => {
                        let next = (cfg.triggers.ram_used_percent + 5.0).min(95.0);
                        match update_config_setting(cfg_path.as_deref(), "ram-percent", &format!("{next:.0}")) {
                            Ok((_, new_cfg, k, v)) => {
                                cfg = new_cfg;
                                let _ = signal_daemon(libc::SIGHUP, "SIGHUP", None);
                                Some(format!("✔ Updated {k} = {v} & reloaded daemon"))
                            }
                            Err(e) => Some(format!("✖ {e}")),
                        }
                    }
                    b'-' | b'_' => {
                        let next = (cfg.triggers.ram_used_percent - 5.0).max(50.0);
                        match update_config_setting(cfg_path.as_deref(), "ram-percent", &format!("{next:.0}")) {
                            Ok((_, new_cfg, k, v)) => {
                                cfg = new_cfg;
                                let _ = signal_daemon(libc::SIGHUP, "SIGHUP", None);
                                Some(format!("✔ Updated {k} = {v} & reloaded daemon"))
                            }
                            Err(e) => Some(format!("✖ {e}")),
                        }
                    }
                    b'n' | b'N' => {
                        let next = !cfg.notifications.enabled;
                        match update_config_setting(cfg_path.as_deref(), "notify", if next { "true" } else { "false" }) {
                            Ok((_, new_cfg, k, v)) => {
                                cfg = new_cfg;
                                let _ = signal_daemon(libc::SIGHUP, "SIGHUP", None);
                                Some(format!("✔ Updated {k} = {v} & reloaded daemon"))
                            }
                            Err(e) => Some(format!("✖ {e}")),
                        }
                    }
                    _ => None,
                };
                if let Some(m) = msg {
                    toast = Some((m, Instant::now() + Duration::from_secs(4)));
                    break; // Re-render immediately
                }
            }
        }
    }
    0
}

fn cmd_config(cfg_path: Option<PathBuf>, action: ConfigAction) -> i32 {
    let p = Palette::detect();
    match action {
        ConfigAction::Show => {
            let cfg = match load_config(cfg_path.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            };
            let w = terminal_width();
            let src = cfg
                .source_path
                .as_ref()
                .map(|c| c.display().to_string())
                .unwrap_or_else(|| "built-in defaults".into());
            print_banner(
                &format!("keep-ram-clean v{VERSION} · Settings"),
                &src,
                "CONFIG",
                p.cyan,
                w,
                &p,
            );
            let t = &cfg.triggers;
            let n = &cfg.notifications;
            let k = &cfg.kernel;
            let lines = vec![
                format!("{}{:<22} {:<16} {}{}", p.bold, "CLI KEY", "CURRENT", "DESCRIPTION", p.nc),
                format!("{:<22} {:<16} {}", "ram-percent", format!("{:.1}%", t.ram_used_percent), "Auto-clean when RAM used >= this %"),
                format!("{:<22} {:<16} {}", "min-avail-mb", format!("{} MiB", t.min_available_mb), "Auto-clean when available RAM <= MiB"),
                format!("{:<22} {:<16} {}", "swap-percent", format!("{:.1}%", t.swap_used_percent), "Auto-clean when swap used >= this %"),
                format!("{:<22} {:<16} {}", "tmpfs-percent", format!("{:.1}%", t.tmpfs_used_percent), "Auto-clean when /tmp tmpfs >= this %"),
                format!("{:<22} {:<16} {}", "psi-some / psi-full", format!("{:.1} / {:.1}", t.psi_some_avg10, t.psi_full_avg10), "Linux PSI memory contention thresholds"),
                format!("{:<22} {:<16} {}", "cooldown", format!("{}s", t.cooldown_seconds), "Minimum wait between auto-cleanups"),
                format!("{:<22} {:<16} {}", "interval", format!("{}m", t.interval_minutes), "Proactive periodic cleanup interval (0=off)"),
                format!("{:<22} {:<16} {}", "notify", if n.enabled { "true" } else { "false" }, "Desktop notification popup on auto-clean"),
                format!("{:<22} {:<16} {}", "notify-min-mb", format!("{} MiB", n.min_freed_mb), "Minimum RAM freed to show notification"),
                format!("{:<22} {:<16} {}", "drop-caches", k.drop_caches_level, "Kernel drop_caches level (0-3)"),
                format!("{}Example: keep-ram-clean config set ram-percent 75  │  keep-ram-clean config set notify true{}", p.dim, p.nc),
            ];
            print_panel("AUTO-CLEAN & NOTIFICATION SETTINGS", "", "", &lines, w, &p);
            0
        }
        ConfigAction::Set { key, value } => {
            match update_config_setting(cfg_path.as_deref(), &key, &value) {
                Ok((dest, _, canonical_key, formatted)) => {
                    println!(
                        "{}✔{} Updated {}{canonical_key}{} = {}{formatted}{} in {}",
                        p.green,
                        p.nc,
                        p.bold,
                        p.nc,
                        p.cyan,
                        p.nc,
                        dest.display()
                    );
                    match signal_daemon(libc::SIGHUP, "SIGHUP", None) {
                        Ok(msg) => println!("{}✔{} {msg}", p.green, p.nc),
                        Err(_) => println!(
                            "{}ℹ{} Daemon not running; setting will apply on next start.",
                            p.dim, p.nc
                        ),
                    }
                    0
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
    }
}

fn cmd_doctor(cfg_path: Option<PathBuf>, as_json: bool) -> i32 {
    let p = Palette::detect();
    let cfg = match load_config(cfg_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let findings = run_doctor(Some(&cfg));
    if as_json {
        let payload = json!({ "version": VERSION, "findings": findings });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return 0;
    }

    let w = terminal_width();
    let warn_count = findings.iter().filter(|f| f.status == "warn").count();
    let (badge, badge_col) = if warn_count == 0 {
        ("✔ HEALTHY", p.green)
    } else {
        ("⚠ WARNINGS", p.yellow)
    };
    print_banner(&format!("keep-ram-clean v{VERSION} · Doctor"), "", badge, badge_col, w, &p);

    let mut lines = Vec::new();
    for f in &findings {
        let (icon, col) = match f.status.as_str() {
            "ok" => ("✔", p.green),
            "warn" => ("⚠", p.yellow),
            _ => ("ℹ", p.cyan),
        };
        lines.push(format!("{col}{icon}{} {}{}{}: {}", p.nc, p.bold, f.check, p.nc, f.detail));
        if !f.recommendation.is_empty() {
            lines.push(format!("    {}→ {}{}", p.dim, f.recommendation, p.nc));
        }
    }
    print_panel("KERNEL & VM AUDIT", "", "", &lines, w, &p);
    0
}

fn cmd_init_config(output: Option<PathBuf>, force: bool) -> i32 {
    let dest = match output {
        Some(p) => expand_tilde(&p),
        None if is_root() => PathBuf::from(DEFAULT_SYSTEM_CONFIG),
        None => default_user_config(),
    };
    if let Ok(meta) = fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            eprintln!("Error: Refusing to write through symlink: {}", dest.display());
            return 1;
        }
        if !force {
            eprintln!(
                "Config already exists at {}. Use --force to overwrite.",
                dest.display()
            );
            return 1;
        }
    }
    if let Some(parent) = dest.parent() {
        let dir_mode = if is_root() { 0o755 } else { 0o700 };
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("Error creating {}: {e}", parent.display());
            return 1;
        }
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(dir_mode));
    }
    let file_mode = if is_root() { 0o644 } else { 0o600 };
    let res = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(file_mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&dest)
        .and_then(|mut f| {
            fs::set_permissions(&dest, fs::Permissions::from_mode(file_mode))?;
            f.write_all(EXAMPLE_CONFIG_TOML.as_bytes())
        });
    match res {
        Ok(()) => {
            println!("Wrote default configuration to {}", dest.display());
            0
        }
        Err(e) => {
            eprintln!("Error writing {}: {e}", dest.display());
            1
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Commands::Run {
            dry_run,
            only,
            critical,
            json,
            verbose,
        } => cmd_run(cli.config, dry_run, only, critical, json, verbose),
        Commands::Daemon { dry_run } => match load_config(cli.config.as_deref()) {
            Ok(cfg) => RamCleanDaemon::new(cfg, cli.config, None, dry_run).run(),
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        Commands::PressureWatch {
            workload_pid,
            discover,
            allow_exe,
            terminate,
        } => {
            if cli.config.is_some() {
                eprintln!("Error: pressure-watch is standalone and does not accept --config.");
                return ExitCode::FAILURE;
            }
            match pressure_relief::watch(&workload_pid, &allow_exe, discover, terminate) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
        Commands::Status {
            json,
            top,
            watch,
            interval,
            view,
        } => cmd_status(cli.config, json, top, watch, interval, view),
        Commands::Config { action } => cmd_config(cli.config, action),
        Commands::Doctor { json } => cmd_doctor(cli.config, json),
        Commands::InitConfig { output, force } => cmd_init_config(output, force),
        Commands::Reload => match signal_daemon(libc::SIGHUP, "SIGHUP", None) {
            Ok(msg) => {
                println!("{msg}");
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                1
            }
        },
        Commands::Trigger => match signal_daemon(libc::SIGUSR1, "SIGUSR1", None) {
            Ok(msg) => {
                println!("{msg}");
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                1
            }
        },
    };
    ExitCode::from(code as u8)
}

#[cfg(test)]
#[test]
fn fixture_watch_keyboard_pty() {
    let Some(config) = std::env::var_os("KRC_WATCH_KEY_FIXTURE") else {
        return;
    };
    WATCH_KEY_FIXTURE.store(true, Ordering::Relaxed);
    let mut args = vec!["keep-ram-clean".into(), "--config".into(), config,
        "status".into(), "--watch".into(), "--top".into(), "0".into(), "--interval".into(), "30".into()];
    if std::env::var_os("KRC_WATCH_KEY_EMBEDDED").is_none() {
        args.extend([std::ffi::OsString::from("--view"), "config".into()]);
    }
    let cli = Cli::parse_from(args);
    let Commands::Status { json, top, watch, interval, view } = cli.command else {
        unreachable!("test fixture must invoke status");
    };
    assert_eq!(cmd_status(cli.config, json, top, watch, interval, view), 0);
}
