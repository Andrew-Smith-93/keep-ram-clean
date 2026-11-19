//! Background daemon loop with self-pipe signal handling, hardened IPC, and hot config reload.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cleaners::{pidfd_open, pidfd_send_signal, run_cleanup, CleanupReport};
use crate::config::{default_runtime_dir, is_root, load_config, Config};
use crate::metrics::{collect_snapshot, read_proc_cmdline, read_proc_starttime};
use crate::triggers::{LocalTime, TriggerEngine};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const TRUSTED_BIN_PATH: &str = "/usr/bin:/bin";

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static STOP_SIGNAL: AtomicI32 = AtomicI32::new(0);
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);
static TRIGGER_REQUESTED: AtomicBool = AtomicBool::new(false);
static WAKE_PIPE_WR: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_signal(sig: libc::c_int) {
    match sig {
        libc::SIGTERM | libc::SIGINT => {
            STOP_SIGNAL.store(sig, Ordering::SeqCst);
            STOP_REQUESTED.store(true, Ordering::SeqCst);
        }
        libc::SIGHUP => RELOAD_REQUESTED.store(true, Ordering::SeqCst),
        libc::SIGUSR1 => TRIGGER_REQUESTED.store(true, Ordering::SeqCst),
        _ => {}
    }
    let fd = WAKE_PIPE_WR.load(Ordering::SeqCst);
    if fd >= 0 {
        let b: u8 = 1;
        unsafe {
            let _ = libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastCleanupStep {
    pub name: String,
    pub status: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastCleanupState {
    pub finished_at: f64,
    pub duration_seconds: f64,
    pub reasons: Vec<String>,
    pub ram_freed_mb: i64,
    pub swap_freed_mb: i64,
    pub steps: Vec<LastCleanupStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub version: String,
    pub pid: i32,
    pub starttime: Option<u64>,
    pub status: String,
    pub uid: u32,
    pub dry_run: bool,
    pub config_path: Option<String>,
    pub started_at: f64,
    pub updated_at: f64,
    pub cleanups_run: u64,
    pub total_ram_freed_mb: i64,
    pub total_swap_freed_mb: i64,
    pub last_cleanup: Option<LastCleanupState>,
    #[serde(default)]
    pub alive: bool,
    #[serde(default)]
    pub state_file: String,
}

fn ensure_secure_runtime_dir(path: &Path) -> io::Result<()> {
    let root = is_root();
    let mode = if root { 0o755 } else { 0o700 };
    fs::create_dir_all(path)?;
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let euid = unsafe { libc::geteuid() };
    if st.st_uid != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Runtime dir {} not owned by uid {euid}", path.display()),
        ));
    }
    if (st.st_mode & 0o022) != 0 || (!root && (st.st_mode & 0o077) != 0) {
        unsafe { libc::fchmod(owned.as_raw_fd(), mode) };
    }
    Ok(())
}

fn atomic_write_text(dir: &Path, filename: &str, content: &str, mode: u32) -> io::Result<()> {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp_path = dir.join(format!(".{filename}.{pid}.{nanos}.tmp"));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp_path)?;
    if let Err(e) = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))
        .and_then(|()| file.write_all(content.as_bytes()))
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    drop(file);
    if let Err(e) = fs::rename(&tmp_path, dir.join(filename)) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

fn send_notification(config: &Config, report: &CleanupReport, critical: bool) {
    let ncfg = &config.notifications;
    if !ncfg.enabled || is_root() {
        return;
    }
    if ncfg.on_critical_only && !critical {
        return;
    }
    if report.ram_freed_mb < ncfg.min_freed_mb && !critical {
        return;
    }
    let notify_bin = TRUSTED_BIN_PATH
        .split(':')
        .map(|d| Path::new(d).join("notify-send"))
        .find(|p| p.is_file());
    let Some(bin) = notify_bin else {
        return;
    };
    let urgency = if critical { "critical" } else { "normal" };
    let title = "keep-ram-clean: Memory Cleanup";
    let body = escape_html(&format!(
        "Trigger: {}\nAvailable RAM: {} MiB ({:+} MiB)\nSwap Used: {} MiB ({:+} MiB)",
        report.reasons.join(", "),
        report.post_available_mb,
        report.ram_freed_mb,
        report.post_swap_used_mb,
        -report.swap_freed_mb
    ));
    let _ = Command::new(bin)
        .args(["-u", urgency, "-a", "keep-ram-clean", "--", title, &body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub struct RamCleanDaemon {
    config: Config,
    config_path: Option<PathBuf>,
    runtime_dir: PathBuf,
    dry_run: bool,
    engine: TriggerEngine,
    started_at: f64,
    starttime: Option<u64>,
    cleanups_run: u64,
    total_ram_freed_mb: i64,
    total_swap_freed_mb: i64,
    last_cleanup: Option<LastCleanupState>,
}

impl RamCleanDaemon {
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        runtime_dir: Option<PathBuf>,
        dry_run: bool,
    ) -> Self {
        let cfg_path = config_path.or_else(|| config.source_path.clone());
        let engine = TriggerEngine::new(config.clone());
        let pid = std::process::id() as i32;
        Self {
            config,
            config_path: cfg_path,
            runtime_dir: runtime_dir.unwrap_or_else(default_runtime_dir),
            dry_run,
            engine,
            started_at: unix_now(),
            starttime: read_proc_starttime(pid),
            cleanups_run: 0,
            total_ram_freed_mb: 0,
            total_swap_freed_mb: 0,
            last_cleanup: None,
        }
    }

    fn write_state(&self, status: &str) {
        if ensure_secure_runtime_dir(&self.runtime_dir).is_err() {
            return;
        }
        let state = DaemonState {
            version: VERSION.to_string(),
            pid: std::process::id() as i32,
            starttime: self.starttime,
            status: status.to_string(),
            uid: unsafe { libc::geteuid() },
            dry_run: self.dry_run,
            config_path: self
                .config_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            started_at: self.started_at,
            updated_at: unix_now(),
            cleanups_run: self.cleanups_run,
            total_ram_freed_mb: self.total_ram_freed_mb,
            total_swap_freed_mb: self.total_swap_freed_mb,
            last_cleanup: self.last_cleanup.clone(),
            alive: status == "running",
            state_file: String::new(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let mode = if is_root() { 0o644 } else { 0o600 };
            let _ = atomic_write_text(&self.runtime_dir, "state.json", &json, mode);
        }
    }

    fn write_pid(&self) -> io::Result<()> {
        ensure_secure_runtime_dir(&self.runtime_dir)?;
        let mode = if is_root() { 0o644 } else { 0o600 };
        atomic_write_text(
            &self.runtime_dir,
            "daemon.pid",
            &format!("{}\n", std::process::id()),
            mode,
        )
    }

    fn cleanup_runtime_files(&self) {
        let pid_path = self.runtime_dir.join("daemon.pid");
        if let Ok(meta) = fs::symlink_metadata(&pid_path) {
            if meta.file_type().is_file() {
                if let Ok(txt) = fs::read_to_string(&pid_path) {
                    if txt.trim() == std::process::id().to_string() {
                        let _ = fs::remove_file(&pid_path);
                    }
                }
            }
        }
        self.write_state("stopped");
    }

    fn reload_config(&mut self) {
        eprintln!("[INFO] Received SIGHUP, reloading configuration...");
        match load_config(self.config_path.as_deref()) {
            Ok(new_cfg) => {
                let src = new_cfg
                    .source_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "built-in defaults".into());
                self.config = new_cfg.clone();
                self.engine.update_config(new_cfg);
                self.write_state("running");
                eprintln!("[INFO] Reloaded configuration from {src}");
            }
            Err(e) => eprintln!("[ERROR] Config reload failed (keeping previous config): {e}"),
        }
    }

    fn execute_cleanup(&mut self, reasons: Vec<String>, critical: bool) {
        eprintln!(
            "[INFO] Triggering cleanup ({}){}",
            reasons.join("; "),
            if critical { " [CRITICAL]" } else { "" }
        );
        let report = run_cleanup(&self.config, reasons, self.dry_run, critical, None);
        self.engine.record_cleanup(Instant::now());
        self.cleanups_run += 1;
        if report.ram_freed_mb > 0 {
            self.total_ram_freed_mb += report.ram_freed_mb;
        }
        if report.swap_freed_mb > 0 {
            self.total_swap_freed_mb += report.swap_freed_mb;
        }
        for s in &report.steps {
            eprintln!("[INFO]   [{}] ({}) {}", s.name, s.status, s.summary);
        }
        eprintln!(
            "[INFO] Cleanup finished in {:.2}s | Available RAM: {} -> {} MiB ({:+} MiB) | Swap: {} -> {} MiB ({:+} MiB)",
            report.duration_seconds,
            report.pre_available_mb,
            report.post_available_mb,
            report.ram_freed_mb,
            report.pre_swap_used_mb,
            report.post_swap_used_mb,
            -report.swap_freed_mb
        );

        self.last_cleanup = Some(LastCleanupState {
            finished_at: report.finished_at,
            duration_seconds: report.duration_seconds,
            reasons: report.reasons.clone(),
            ram_freed_mb: report.ram_freed_mb,
            swap_freed_mb: report.swap_freed_mb,
            steps: report
                .steps
                .iter()
                .map(|s| LastCleanupStep {
                    name: s.name.clone(),
                    status: s.status.clone(),
                    summary: s.summary.clone(),
                })
                .collect(),
        });
        self.write_state("running");
        send_notification(&self.config, &report, critical);
    }

    fn poll_once(&mut self) {
        let t = &self.config.triggers;
        let snap = collect_snapshot(
            Some(&self.config.tmpfs.paths),
            false,
            self.config.tmpfs.enabled && t.tmpfs_used_percent > 0.0,
            t.psi_some_avg10 > 0.0 || t.psi_full_avg10 > 0.0,
            t.max_load_avg_ratio > 0.0,
        );
        let dec = self.engine.evaluate(&snap, LocalTime::now(), Instant::now());
        if dec.deferred {
            eprintln!(
                "[INFO] Deferred cleanup ({}): {}",
                dec.reasons.join("; "),
                dec.defer_reason
            );
        } else if dec.should_run {
            self.execute_cleanup(dec.reasons, dec.critical);
        }
    }

    pub fn run(mut self) -> i32 {
        // Best-effort mlockall when root so the daemon stays resident under severe thrashing
        if is_root() {
            unsafe {
                let _ = libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
            }
        }

        // Create non-blocking self-pipe for race-free signal wakeup
        let mut fds = [-1i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            eprintln!("[ERROR] Failed to create wake pipe: {}", io::Error::last_os_error());
            return 1;
        }
        let rd_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let wr_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        WAKE_PIPE_WR.store(wr_fd.as_raw_fd(), Ordering::SeqCst);

        STOP_REQUESTED.store(false, Ordering::SeqCst);
        RELOAD_REQUESTED.store(false, Ordering::SeqCst);
        TRIGGER_REQUESTED.store(false, Ordering::SeqCst);

        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handle_signal as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1] {
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }

        if let Err(e) = self.write_pid() {
            eprintln!("[ERROR] Failed writing PID file: {e}");
            return 1;
        }
        self.write_state("running");

        let cfg_label = self
            .config
            .source_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "built-in defaults".into());
        eprintln!(
            "[INFO] keep-ram-clean daemon v{VERSION} started (pid={}, uid={}, poll={}s, cooldown={}s, config={cfg_label})",
            std::process::id(),
            unsafe { libc::geteuid() },
            self.config.triggers.poll_interval_seconds,
            self.config.triggers.cooldown_seconds,
        );
        if !is_root() {
            eprintln!(
                "[INFO] Running in non-root user mode: kernel reclaim and swap flush will be skipped."
            );
        }

        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            drain_pipe(rd_fd.as_raw_fd());

            if RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
                self.reload_config();
            }
            if TRIGGER_REQUESTED.swap(false, Ordering::SeqCst) {
                eprintln!("[INFO] Received SIGUSR1, running on-demand cleanup...");
                self.execute_cleanup(vec!["On-demand signal (SIGUSR1)".into()], false);
            } else if !STOP_REQUESTED.load(Ordering::SeqCst) {
                self.poll_once();
            }

            if STOP_REQUESTED.load(Ordering::SeqCst)
                || RELOAD_REQUESTED.load(Ordering::SeqCst)
                || TRIGGER_REQUESTED.load(Ordering::SeqCst)
            {
                continue;
            }

            let mut pfd = libc::pollfd {
                fd: rd_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = (self.config.triggers.poll_interval_seconds.min(86400) * 1000) as i32;
            unsafe {
                libc::poll(&mut pfd, 1, timeout_ms);
            }
        }

        let sig = STOP_SIGNAL.load(Ordering::SeqCst);
        if sig != 0 {
            eprintln!("[INFO] Received signal {sig}, shutting down gracefully...");
        }
        WAKE_PIPE_WR.store(-1, Ordering::SeqCst);
        self.cleanup_runtime_files();
        eprintln!("[INFO] keep-ram-clean daemon stopped.");
        0
    }
}

fn drain_pipe(fd: RawFd) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

fn verify_daemon_process(pid: i32, expected_starttime: Option<u64>) -> bool {
    let Some(expected) = expected_starttime else {
        return false;
    };
    if read_proc_starttime(pid) != Some(expected) {
        return false;
    }
    let cmd = read_proc_cmdline(pid);
    cmd.is_empty() || cmd.contains("keep-ram-clean") || cmd.contains("keep_ram_clean")
}

fn is_trusted_state_file(rdir: &Path, state_path: &Path) -> bool {
    let euid = unsafe { libc::geteuid() };
    let Ok(dm) = fs::symlink_metadata(rdir) else {
        return false;
    };
    if !dm.file_type().is_dir() || (dm.uid() != 0 && dm.uid() != euid) || (dm.mode() & 0o022) != 0 {
        return false;
    }
    let Ok(fm) = fs::symlink_metadata(state_path) else {
        return false;
    };
    fm.file_type().is_file() && (fm.uid() == 0 || fm.uid() == euid) && (fm.mode() & 0o022) == 0
}

pub fn read_daemon_state(runtime_dir: Option<&Path>) -> Option<DaemonState> {
    let candidates: Vec<PathBuf> = match runtime_dir {
        Some(d) => vec![d.to_path_buf()],
        None => vec![default_runtime_dir(), PathBuf::from("/run/keep-ram-clean")],
    };
    for rdir in candidates {
        let state_path = rdir.join("state.json");
        if !state_path.exists() || !is_trusted_state_file(&rdir, &state_path) {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(mut state) = serde_json::from_str::<DaemonState>(&raw) else {
            continue;
        };
        let mut alive = false;
        if state.status == "running" && state.pid > 1 && state.starttime.is_some() {
            let rc = unsafe { libc::kill(state.pid, 0) };
            let err = io::Error::last_os_error().raw_os_error();
            if rc == 0 || err == Some(libc::EPERM) {
                alive = verify_daemon_process(state.pid, state.starttime);
            }
        }
        state.alive = alive;
        state.state_file = state_path.to_string_lossy().to_string();
        return Some(state);
    }
    None
}

pub fn signal_daemon(sig: i32, sig_name: &str, runtime_dir: Option<&Path>) -> Result<String, String> {
    let Some(state) = read_daemon_state(runtime_dir) else {
        return Err("No running keep-ram-clean daemon found.".into());
    };
    if !state.alive {
        return Err("No running keep-ram-clean daemon found.".into());
    }
    let pid = state.pid;
    let Some(expected_st) = state.starttime else {
        return Err(format!("Daemon state missing starttime for PID {pid}; refusing to signal."));
    };

    match pidfd_open(pid) {
        Ok(pfd) => {
            if !verify_daemon_process(pid, Some(expected_st)) {
                return Err(format!("PID {pid} no longer matches keep-ram-clean daemon."));
            }
            pidfd_send_signal(pfd.as_raw_fd(), sig).map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    format!("Permission denied signalling daemon PID {pid} (try running with sudo).")
                } else {
                    format!("Failed to signal PID {pid}: {e}")
                }
            })?;
            Ok(format!("Sent {sig_name} to daemon (PID {pid})."))
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
            if !verify_daemon_process(pid, Some(expected_st)) {
                return Err(format!("PID {pid} no longer matches keep-ram-clean daemon."));
            }
            if unsafe { libc::kill(pid, sig) } != 0 {
                return Err(format!("Failed to signal PID {pid}: {}", io::Error::last_os_error()));
            }
            Ok(format!("Sent {sig_name} to daemon (PID {pid})."))
        }
        Err(e) => Err(format!("Failed to open pidfd for PID {pid}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_daemon_state_rejects_stopped_and_pid_reuse() {
        let dir = std::env::temp_dir().join(format!("krc-state-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let sp = dir.join("state.json");
        let base = DaemonState {
            version: VERSION.into(),
            pid: std::process::id() as i32,
            starttime: Some(1),
            status: "stopped".into(),
            uid: unsafe { libc::geteuid() },
            dry_run: false,
            config_path: None,
            started_at: 0.0,
            updated_at: 0.0,
            cleanups_run: 0,
            total_ram_freed_mb: 0,
            total_swap_freed_mb: 0,
            last_cleanup: None,
            alive: false,
            state_file: String::new(),
        };
        fs::write(&sp, serde_json::to_string(&base).unwrap()).unwrap();
        fs::set_permissions(&sp, fs::Permissions::from_mode(0o600)).unwrap();
        let st = read_daemon_state(Some(&dir)).unwrap();
        assert!(!st.alive);

        let reused = DaemonState {
            status: "running".into(),
            starttime: Some(999_999_999),
            ..base
        };
        fs::write(&sp, serde_json::to_string(&reused).unwrap()).unwrap();
        let st2 = read_daemon_state(Some(&dir)).unwrap();
        assert!(!st2.alive);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_escape_works() {
        assert_eq!(escape_html("<b>a & b</b>"), "&lt;b&gt;a &amp; b&lt;/b&gt;");
    }
}
