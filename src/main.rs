//! Command-line interface for keep-ram-clean.

mod cleaners;
mod config;
mod daemon;
mod doctor;
mod metrics;
mod triggers;
mod ui;

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::json;

use crate::cleaners::{run_cleanup, ALL_STEP_NAMES};
use crate::config::{
    default_user_config, expand_tilde, is_root, load_config, Config, DEFAULT_SYSTEM_CONFIG,
    EXAMPLE_CONFIG_TOML,
};
use crate::daemon::{read_daemon_state, signal_daemon, RamCleanDaemon, VERSION};
use crate::doctor::run_doctor;
use crate::metrics::{collect_snapshot, top_memory_processes};
use crate::ui::{
    fmt_mib, fmt_mib_f64, gauge_bar, mini_bar, print_banner, print_section, psi_badge,
    truncate_str, Palette, BOX_WIDTH,
};

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
    /// Show live RAM, PSI, swap, tmpfs, and daemon status
    Status {
        /// Output status as JSON
        #[arg(long)]
        json: bool,
        /// Number of top memory processes to display
        #[arg(long, default_value_t = 5)]
        top: usize,
        /// Live-refreshing dashboard mode (Ctrl+C to exit)
        #[arg(short, long)]
        watch: bool,
        /// Refresh interval in seconds for --watch mode
        #[arg(long, default_value_t = 2)]
        interval: u64,
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

    let (badge, badge_col) = if report.dry_run {
        ("◎ DRY-RUN", p.yellow)
    } else if report.has_errors {
        ("✖ ERRORS", p.red)
    } else {
        ("✔ COMPLETE", p.green)
    };
    print_banner(&format!("keep-ram-clean v{VERSION} · Cleanup"), badge, badge_col, &p);
    println!(
        "  {}Config:{} {}",
        p.dim,
        p.nc,
        cfg.source_path
            .as_ref()
            .map(|c| c.display().to_string())
            .unwrap_or_else(|| "built-in defaults".into())
    );

    print_section("STEPS", &p);
    for step in &report.steps {
        let (icon, col) = match step.status.as_str() {
            "ok" => ("✔", p.green),
            "dry-run" => ("◎", p.cyan),
            "skipped" => ("–", p.yellow),
            _ => ("✖", p.red),
        };
        println!(
            "  {col}{icon}{} {}{:<12}{} {}",
            p.nc, p.bold, step.name, p.nc, step.summary
        );
        if verbose || report.dry_run || step.status == "error" {
            for d in &step.details {
                println!("      {}• {d}{}", p.dim, p.nc);
            }
        }
    }

    print_section("SUMMARY", &p);
    let ram_col = if report.ram_freed_mb > 0 {
        p.green
    } else {
        p.dim
    };
    let swap_col = if report.swap_freed_mb > 0 {
        p.green
    } else {
        p.dim
    };
    println!(
        "  {:<14} {}{}{} {} (was {}, {ram_col}{:+} MiB{})",
        "Available RAM",
        p.bold,
        fmt_mib(report.post_available_mb),
        p.nc,
        p.dim,
        fmt_mib(report.pre_available_mb),
        report.ram_freed_mb,
        p.nc
    );
    println!(
        "  {:<14} {}{}{} {} (was {}, {swap_col}{:+} MiB{})",
        "Swap Used",
        p.bold,
        fmt_mib(report.post_swap_used_mb),
        p.nc,
        p.dim,
        fmt_mib(report.pre_swap_used_mb),
        -report.swap_freed_mb,
        p.nc
    );
    println!("  {:<14} {:.2}s", "Elapsed", report.duration_seconds);
    code
}

fn render_status_dashboard(cfg: &Config, top: usize, p: &Palette) {
    let snap = collect_snapshot(Some(&cfg.tmpfs.paths), true, true, true, true);
    let top_procs = top_memory_processes(top);
    let daemon_state = read_daemon_state(None);
    let mem = &snap.memory;
    let t = &cfg.triggers;

    let (badge, badge_col) = match daemon_state.as_ref().filter(|d| d.alive) {
        Some(_) => ("● DAEMON RUNNING", p.green),
        None => ("○ DAEMON STOPPED", p.yellow),
    };
    print_banner(&format!("keep-ram-clean v{VERSION}"), badge, badge_col, p);

    // Memory & Swap
    print_section("MEMORY & SWAP", p);
    let ram_pct = mem.used_percent();
    let ram_crit = if t.ram_used_percent > 0.0 {
        t.ram_used_percent
    } else {
        80.0
    };
    let ram_warn = (ram_crit - 15.0).max(50.0);
    let ram_bar = gauge_bar(ram_pct, 24, ram_warn, ram_crit, p);
    let ram_col = p.severity_color(ram_pct, ram_warn, ram_crit);
    println!(
        "  {:<8} {ram_bar} {ram_col}{:>5.1}%{}  {}{} / {}{}  {}({} free){}",
        "RAM",
        ram_pct,
        p.nc,
        p.bold,
        fmt_mib(mem.used_mb),
        fmt_mib(mem.total_mb),
        p.nc,
        p.dim,
        fmt_mib(mem.available_mb),
        p.nc
    );

    let swap_pct = mem.swap_used_percent();
    let swap_crit = if t.swap_used_percent > 0.0 {
        t.swap_used_percent
    } else {
        50.0
    };
    let swap_warn = (swap_crit - 20.0).max(20.0);
    let swap_bar = gauge_bar(swap_pct, 24, swap_warn, swap_crit, p);
    let swap_col = p.severity_color(swap_pct, swap_warn, swap_crit);
    println!(
        "  {:<8} {swap_bar} {swap_col}{:>5.1}%{}  {}{} / {}{}",
        "Swap",
        swap_pct,
        p.nc,
        p.bold,
        fmt_mib(mem.swap_used_mb),
        fmt_mib(mem.swap_total_mb),
        p.nc
    );

    println!(
        "  {}Caches   PageCache {} · Reclaimable Slab {} · Dirty {}{}",
        p.dim,
        fmt_mib(mem.cached_mb),
        fmt_mib(mem.slab_reclaimable_mb),
        fmt_mib(mem.dirty_mb),
        p.nc
    );

    if !snap.swaps.is_empty() {
        for s in &snap.swaps {
            let tag = if s.is_zram() {
                format!(" {}[zram]{}", p.cyan, p.nc)
            } else {
                String::new()
            };
            println!(
                "  {}  ↳{} {}{tag}  {}{}/{}{}",
                p.dim,
                p.nc,
                s.path,
                p.dim,
                fmt_mib(s.used_mb),
                fmt_mib(s.size_mb),
                p.nc
            );
        }
    }

    // tmpfs mounts
    if !snap.tmpfs_mounts.is_empty() {
        print_section("TMPFS MOUNTS", p);
        let tmp_crit = if t.tmpfs_used_percent > 0.0 {
            t.tmpfs_used_percent
        } else {
            70.0
        };
        let tmp_warn = (tmp_crit - 20.0).max(40.0);
        for m in &snap.tmpfs_mounts {
            let pct = m.used_percent();
            let bar = gauge_bar(pct, 24, tmp_warn, tmp_crit, p);
            let col = p.severity_color(pct, tmp_warn, tmp_crit);
            println!(
                "  {:<8} {bar} {col}{:>5.1}%{}  {} / {}",
                truncate_str(&m.mountpoint, 8),
                pct,
                p.nc,
                fmt_mib(m.used_mb),
                fmt_mib(m.total_mb)
            );
        }
    }

    // Pressure & Load
    print_section("PRESSURE & LOAD", p);
    if snap.psi.available {
        let badge = psi_badge(
            snap.psi.some_avg10,
            snap.psi.full_avg10,
            t.psi_some_avg10,
            t.psi_full_avg10,
            p,
        );
        println!(
            "  {:<8} {badge}  {}some{} avg10={:.2} avg60={:.2}  {}│{}  {}full{} avg10={:.2} avg60={:.2}",
            "PSI",
            p.bold,
            p.nc,
            snap.psi.some_avg10,
            snap.psi.some_avg60,
            p.dim,
            p.nc,
            p.bold,
            p.nc,
            snap.psi.full_avg10,
            snap.psi.full_avg60
        );
    }
    let ratio = snap.load_ratio();
    let load_col = p.severity_color(ratio * 100.0, 80.0, 120.0);
    println!(
        "  {:<8} {:.2}  {:.2}  {:.2}  {}({} cores · ratio {load_col}{:.2}{}{})",
        "Load",
        snap.load_avg_1m,
        snap.load_avg_5m,
        snap.load_avg_15m,
        p.dim,
        snap.cpu_count,
        ratio,
        p.nc,
        p.dim
    );
    print!("{}", p.nc);

    // Top consumers
    if !top_procs.is_empty() {
        print_section(&format!("TOP {} MEMORY CONSUMERS", top_procs.len()), p);
        let total_f = mem.total_mb.max(1) as f64;
        for proc in &top_procs {
            let frac = proc.rss_mb / total_f;
            let bar = mini_bar(frac, 12, p);
            println!(
                "  {}PID {:<7}{} {:<18} {bar} {:>9} {}({:>4.1}%){}",
                p.dim,
                proc.pid,
                p.nc,
                truncate_str(&proc.comm, 18),
                fmt_mib_f64(proc.rss_mb),
                p.dim,
                frac * 100.0,
                p.nc
            );
        }
    }

    // Daemon
    print_section("DAEMON", p);
    if let Some(ds) = daemon_state.filter(|d| d.alive) {
        println!(
            "  {}● RUNNING{}  PID {}{}{}  {}· since {}{}",
            p.green,
            p.nc,
            p.bold,
            ds.pid,
            p.nc,
            p.dim,
            fmt_ts(ds.started_at),
            p.nc
        );
        println!(
            "  Cleanups   {} run  {}· cumulative {}{:+} MiB RAM{}, {}{:+} MiB swap{}",
            ds.cleanups_run,
            p.dim,
            p.green,
            ds.total_ram_freed_mb,
            p.dim,
            p.cyan,
            -ds.total_swap_freed_mb,
            p.nc
        );
        if let Some(last) = ds.last_cleanup {
            println!(
                "  Last Run   {}  {}— {}{}",
                fmt_ts(last.finished_at),
                p.dim,
                last.reasons.join("; "),
                p.nc
            );
        }
    } else {
        println!(
            "  {}○ NOT RUNNING{}  {}Start with: keep-ram-clean daemon (or systemd service){}",
            p.yellow, p.nc, p.dim, p.nc
        );
    }
}

fn cmd_status(
    cfg_path: Option<PathBuf>,
    as_json: bool,
    top: usize,
    watch: bool,
    interval: u64,
) -> i32 {
    let p = Palette::detect();
    let cfg = match load_config(cfg_path.as_deref()) {
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
        render_status_dashboard(&cfg, top, &p);
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

    let mut first = true;
    while !WATCH_STOP.load(Ordering::SeqCst) {
        if p.color {
            if first {
                print!("\x1b[2J\x1b[H");
                first = false;
            } else {
                print!("\x1b[H\x1b[J");
            }
        }
        render_status_dashboard(&cfg, top, &p);
        println!(
            "\n{}{}{}\n{}  Refreshing every {}s · Press Ctrl+C to exit{}",
            p.dim,
            "─".repeat(BOX_WIDTH),
            p.nc,
            p.dim,
            sleep_secs,
            p.nc
        );
        let _ = std::io::stdout().flush();

        // Sleep in small increments so Ctrl+C exits promptly
        for _ in 0..(sleep_secs * 10) {
            if WATCH_STOP.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    0
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

    let warn_count = findings.iter().filter(|f| f.status == "warn").count();
    let (badge, badge_col) = if warn_count == 0 {
        ("✔ HEALTHY", p.green)
    } else {
        ("⚠ WARNINGS", p.yellow)
    };
    print_banner(&format!("keep-ram-clean v{VERSION} · Doctor"), badge, badge_col, &p);

    print_section("KERNEL & VM AUDIT", &p);
    for f in &findings {
        let (icon, col) = match f.status.as_str() {
            "ok" => ("✔", p.green),
            "warn" => ("⚠", p.yellow),
            _ => ("ℹ", p.cyan),
        };
        println!(
            "  {col}{icon}{} {}{}{}: {}",
            p.nc, p.bold, f.check, p.nc, f.detail
        );
        if !f.recommendation.is_empty() {
            println!("      {}→ {}{}", p.dim, f.recommendation, p.nc);
        }
    }
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
        Commands::Status {
            json,
            top,
            watch,
            interval,
        } => cmd_status(cli.config, json, top, watch, interval),
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
