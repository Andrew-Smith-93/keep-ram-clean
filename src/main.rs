//! Command-line interface for keep-ram-clean.

mod cleaners;
mod config;
mod daemon;
mod doctor;
mod metrics;
mod triggers;

use std::env;
use std::fs::{self, File};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;

use crate::cleaners::{run_cleanup, ALL_STEP_NAMES};
use crate::config::{
    default_user_config, expand_tilde, is_root, load_config, DEFAULT_SYSTEM_CONFIG,
    EXAMPLE_CONFIG_TOML,
};
use crate::daemon::{read_daemon_state, signal_daemon, RamCleanDaemon, VERSION};
use crate::doctor::run_doctor;
use crate::metrics::{collect_snapshot, top_memory_processes};

struct Palette {
    bold: &'static str,
    dim: &'static str,
    green: &'static str,
    cyan: &'static str,
    yellow: &'static str,
    red: &'static str,
    nc: &'static str,
}

impl Palette {
    fn detect() -> Self {
        let no_color = env::var_os("NO_COLOR")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let use_color = std::io::stdout().is_terminal() && !no_color;
        if use_color {
            Self {
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
    let c = Palette::detect();
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

    let mode = if report.dry_run {
        format!(" {}[DRY-RUN]{}", c.yellow, c.nc)
    } else {
        String::new()
    };
    println!("{}{}keep-ram-clean v{VERSION}{}{mode}", c.bold, c.cyan, c.nc);
    println!(
        "Config: {}",
        cfg.source_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "built-in defaults".into())
    );
    println!("{}", "-".repeat(58));
    for step in &report.steps {
        let icon = match step.status.as_str() {
            "ok" => format!("{}✔{}", c.green, c.nc),
            "dry-run" => format!("{}◎{}", c.cyan, c.nc),
            "skipped" => format!("{}–{}", c.yellow, c.nc),
            _ => format!("{}✖{}", c.red, c.nc),
        };
        println!(" {icon} {}{:<12}{} {}", c.bold, step.name, c.nc, step.summary);
        if verbose || report.dry_run || step.status == "error" {
            for d in &step.details {
                println!("     {}• {d}{}", c.dim, c.nc);
            }
        }
    }
    println!("{}", "-".repeat(58));
    println!(
        "Available RAM : {}{} MiB{} (was {} MiB, {:+} MiB)",
        c.bold, report.post_available_mb, c.nc, report.pre_available_mb, report.ram_freed_mb
    );
    println!(
        "Swap Used     : {}{} MiB{} (was {} MiB, {:+} MiB)",
        c.bold, report.post_swap_used_mb, c.nc, report.pre_swap_used_mb, -report.swap_freed_mb
    );
    println!("Elapsed       : {:.2}s", report.duration_seconds);
    code
}

fn cmd_status(cfg_path: Option<PathBuf>, as_json: bool, top: usize) -> i32 {
    let c = Palette::detect();
    let cfg = match load_config(cfg_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let snap = collect_snapshot(Some(&cfg.tmpfs.paths), true, true, true, true);
    let top_procs = top_memory_processes(top);
    let daemon_state = read_daemon_state(None);

    if as_json {
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

    let mem = &snap.memory;
    println!("{}{}keep-ram-clean Status{}", c.bold, c.cyan, c.nc);
    println!("{}", "-".repeat(58));
    println!(
        "Physical RAM  : {}{} / {} MiB{} ({:.1}% used, {} MiB available)",
        c.bold,
        mem.used_mb,
        mem.total_mb,
        c.nc,
        mem.used_percent(),
        mem.available_mb
    );
    println!(
        "Caches / Slab : PageCache {} MiB | Reclaimable Slab {} MiB | Dirty {} MiB",
        mem.cached_mb, mem.slab_reclaimable_mb, mem.dirty_mb
    );
    println!(
        "Swap          : {}{} / {} MiB{} ({:.1}% used)",
        c.bold,
        mem.swap_used_mb,
        mem.swap_total_mb,
        c.nc,
        mem.swap_used_percent()
    );
    for s in &snap.swaps {
        let tag = if s.is_zram() { " [zram]" } else { "" };
        println!("                  • {}{tag}: {}/{} MiB", s.path, s.used_mb, s.size_mb);
    }
    if snap.psi.available {
        println!(
            "Memory PSI    : some avg10={:.2} avg60={:.2} | full avg10={:.2} avg60={:.2}",
            snap.psi.some_avg10, snap.psi.some_avg60, snap.psi.full_avg10, snap.psi.full_avg60
        );
    }
    println!(
        "CPU Load      : {:.2} {:.2} {:.2} ({} cores, ratio {:.2})",
        snap.load_avg_1m,
        snap.load_avg_5m,
        snap.load_avg_15m,
        snap.cpu_count,
        snap.load_ratio()
    );
    if !snap.tmpfs_mounts.is_empty() {
        println!("Monitored tmpfs:");
        for m in &snap.tmpfs_mounts {
            println!(
                "  • {}: {}/{} MiB ({:.1}%)",
                m.mountpoint,
                m.used_mb,
                m.total_mb,
                m.used_percent()
            );
        }
    }
    if !top_procs.is_empty() {
        println!("Top {} Memory Consumers:", top_procs.len());
        for p in &top_procs {
            println!("  • PID {:<7} {:>8.1} MiB  {}", p.pid, p.rss_mb, p.comm);
        }
    }
    println!("{}", "-".repeat(58));
    if let Some(ds) = daemon_state.filter(|d| d.alive) {
        println!(
            "Daemon        : {}RUNNING{} (PID {}, since {})",
            c.green,
            c.nc,
            ds.pid,
            fmt_ts(ds.started_at)
        );
        println!(
            "Cleanups Run  : {} (cumulative +{} MiB RAM, -{} MiB swap)",
            ds.cleanups_run, ds.total_ram_freed_mb, ds.total_swap_freed_mb
        );
        if let Some(last) = ds.last_cleanup {
            println!(
                "Last Cleanup  : {} — {}",
                fmt_ts(last.finished_at),
                last.reasons.join("; ")
            );
        }
    } else {
        println!("Daemon        : {}NOT RUNNING{}", c.yellow, c.nc);
    }
    0
}

fn cmd_doctor(cfg_path: Option<PathBuf>, as_json: bool) -> i32 {
    let c = Palette::detect();
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
    println!("{}{}keep-ram-clean Doctor — System Memory Audit{}", c.bold, c.cyan, c.nc);
    println!("{}", "-".repeat(58));
    for f in &findings {
        let icon = match f.status.as_str() {
            "ok" => format!("{}✔{}", c.green, c.nc),
            "warn" => format!("{}⚠{}", c.yellow, c.nc),
            _ => format!("{}ℹ{}", c.cyan, c.nc),
        };
        println!(" {icon} {}{}{}: {}", c.bold, f.check, c.nc, f.detail);
        if !f.recommendation.is_empty() {
            println!("     {}→ {}{}", c.dim, f.recommendation, c.nc);
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
        Commands::Status { json, top } => cmd_status(cli.config, json, top),
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
