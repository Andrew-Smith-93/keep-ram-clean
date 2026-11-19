//! Modular, dry-run-safe, race-hardened Linux memory and cache cleanup actions.

use std::collections::HashSet;
use std::env;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;

use crate::config::{home_dir, is_root, Config};
use crate::metrics::{
    collect_snapshot, is_path_on_tmpfs, list_processes, read_meminfo, read_proc_starttime,
    read_swaps, SystemSnapshot,
};

pub const ALL_STEP_NAMES: &[&str] = &["tmpfs", "watchdog", "user_caches", "kernel", "swap"];
const TRUSTED_SBIN_PATH: &str = "/usr/sbin:/sbin:/usr/bin:/bin";
const MAX_WALK_DEPTH: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub name: String,
    pub status: String, // "ok", "skipped", "dry-run", "error"
    pub summary: String,
    pub bytes_freed: u64,
    pub details: Vec<String>,
}

impl StepResult {
    pub fn new(
        name: &str,
        status: &str,
        summary: impl Into<String>,
        bytes_freed: u64,
        details: Vec<String>,
    ) -> Self {
        Self {
            name: name.to_string(),
            status: status.to_string(),
            summary: summary.into(),
            bytes_freed,
            details,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupReport {
    pub started_at: f64,
    pub finished_at: f64,
    pub duration_seconds: f64,
    pub dry_run: bool,
    pub critical: bool,
    pub reasons: Vec<String>,
    pub pre_available_mb: u64,
    pub post_available_mb: u64,
    pub ram_freed_mb: i64,
    pub pre_swap_used_mb: u64,
    pub post_swap_used_mb: u64,
    pub swap_freed_mb: i64,
    pub has_errors: bool,
    pub steps: Vec<StepResult>,
}

/// Linear-time wildcard glob matcher supporting `*` and `?` (no catastrophic backtracking).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn unix_now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn openat_dir_nofollow(dir_fd: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dir_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn fstat_fd(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(st)
    }
}

fn fstatat_nofollow(dir_fd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir_fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(st)
    }
}

fn unlinkat_entry(dir_fd: RawFd, name: &CStr, is_dir: bool) -> io::Result<()> {
    let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
    if unsafe { libc::unlinkat(dir_fd, name.as_ptr(), flags) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read directory entry names from an open directory FD (duplicating FD so `closedir` never closes `dir_fd`).
fn read_dir_names_fd(dir_fd: RawFd) -> io::Result<Vec<CString>> {
    let dup = unsafe { libc::fcntl(dir_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err);
    }
    unsafe { libc::rewinddir(dirp) };
    let mut names = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            break;
        }
        let cstr = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = cstr.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(cstr.to_owned());
    }
    unsafe { libc::closedir(dirp) };
    Ok(names)
}

fn dir_size_and_max_mtime_fd(dir_fd: RawFd, root_dev: u64, depth: usize) -> (u64, i64) {
    if depth >= MAX_WALK_DEPTH {
        return (0, 0);
    }
    let Ok(names) = read_dir_names_fd(dir_fd) else {
        return (0, 0);
    };
    let mut total: u64 = 0;
    let mut max_mtime: i64 = 0;
    for name in names {
        let Ok(st) = fstatat_nofollow(dir_fd, &name) else {
            continue;
        };
        if st.st_mtime > max_mtime {
            max_mtime = st.st_mtime;
        }
        let ftype = st.st_mode & libc::S_IFMT;
        if ftype == libc::S_IFLNK {
            total = total.saturating_add(st.st_size.max(0) as u64);
        } else if ftype == libc::S_IFDIR {
            if st.st_dev != root_dev {
                continue;
            }
            if let Ok(sub_fd) = openat_dir_nofollow(dir_fd, &name) {
                if let Ok(sub_st) = fstat_fd(sub_fd.as_raw_fd()) {
                    if sub_st.st_dev == root_dev && sub_st.st_ino == st.st_ino {
                        let (sz, mt) =
                            dir_size_and_max_mtime_fd(sub_fd.as_raw_fd(), root_dev, depth + 1);
                        total = total.saturating_add(sz);
                        if mt > max_mtime {
                            max_mtime = mt;
                        }
                    }
                }
            }
        } else {
            total = total.saturating_add(st.st_size.max(0) as u64);
        }
    }
    (total, max_mtime)
}

fn rmtree_fd(
    parent_fd: RawFd,
    name: &CStr,
    root_dev: u64,
    expected_ino: u64,
    depth: usize,
) -> io::Result<()> {
    if depth >= MAX_WALK_DEPTH {
        return Err(io::Error::from_raw_os_error(libc::ELOOP));
    }
    {
        let dir_fd = openat_dir_nofollow(parent_fd, name)?;
        let dir_st = fstat_fd(dir_fd.as_raw_fd())?;
        if dir_st.st_dev != root_dev || dir_st.st_ino != expected_ino {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory replaced or crossed mountpoint",
            ));
        }
        for child in read_dir_names_fd(dir_fd.as_raw_fd())? {
            let Ok(st) = fstatat_nofollow(dir_fd.as_raw_fd(), &child) else {
                continue;
            };
            let ftype = st.st_mode & libc::S_IFMT;
            if ftype == libc::S_IFDIR {
                if st.st_dev != root_dev {
                    continue;
                }
                rmtree_fd(dir_fd.as_raw_fd(), &child, root_dev, st.st_ino, depth + 1)?;
            } else {
                unlinkat_entry(dir_fd.as_raw_fd(), &child, false)?;
            }
        }
    }
    unlinkat_entry(parent_fd, name, true)
}

fn open_subpath_nofollow(
    base_dir: &Path,
    rel_parts: &[&OsStr],
    expected_uid: Option<u32>,
) -> Option<(OwnedFd, u64)> {
    if rel_parts.is_empty() {
        return None;
    }
    let mut cur_fd = open_dir_nofollow(base_dir).ok()?;
    let base_st = fstat_fd(cur_fd.as_raw_fd()).ok()?;
    if let Some(uid) = expected_uid {
        if base_st.st_uid != uid {
            return None;
        }
    }
    let base_dev = base_st.st_dev;

    for part in rel_parts {
        if part.is_empty() || *part == OsStr::new(".") || *part == OsStr::new("..") {
            return None;
        }
        let cname = CString::new(part.as_bytes()).ok()?;
        let nxt_fd = openat_dir_nofollow(cur_fd.as_raw_fd(), &cname).ok()?;
        let st = fstat_fd(nxt_fd.as_raw_fd()).ok()?;
        if st.st_dev != base_dev {
            return None;
        }
        if let Some(uid) = expected_uid {
            if st.st_uid != uid {
                return None;
            }
        }
        cur_fd = nxt_fd;
    }
    Some((cur_fd, base_dev))
}

fn get_open_paths_under(roots: &[PathBuf]) -> HashSet<String> {
    let norm_roots: Vec<String> = roots
        .iter()
        .map(|r| {
            let real = fs::canonicalize(r)
                .ok()
                .and_then(|c| c.to_str().map(String::from))
                .unwrap_or_else(|| r.to_string_lossy().to_string());
            format!("{}/", real.trim_end_matches('/'))
        })
        .collect();

    let mut open_paths = HashSet::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return open_paths;
    };
    let root_user = is_root();
    let mut warned_eacces = false;

    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(s) = fname.to_str() else { continue };
        if !s.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid_path = entry.path();
        for link in ["cwd", "exe"] {
            match fs::read_link(pid_path.join(link)) {
                Ok(target) => {
                    let ts = target.to_string_lossy();
                    if norm_roots
                        .iter()
                        .any(|r| ts.starts_with(r.as_str()) || format!("{ts}/") == *r)
                    {
                        open_paths.insert(ts.into_owned());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied && root_user && !warned_eacces => {
                    eprintln!(
                        "[WARN] Permission denied reading /proc/<pid>/{link} as root (ensure CAP_SYS_PTRACE is permitted)"
                    );
                    warned_eacces = true;
                }
                Err(_) => {}
            }
        }
        match fs::read_dir(pid_path.join("fd")) {
            Ok(fds) => {
                for fd_ent in fds.flatten() {
                    if let Ok(target) = fs::read_link(fd_ent.path()) {
                        let ts = target.to_string_lossy();
                        if norm_roots.iter().any(|r| ts.starts_with(r.as_str())) {
                            open_paths.insert(ts.into_owned());
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && root_user && !warned_eacces => {
                eprintln!(
                    "[WARN] Permission denied reading /proc/<pid>/fd as root (ensure CAP_SYS_PTRACE is permitted)"
                );
                warned_eacces = true;
            }
            Err(_) => {}
        }
    }
    open_paths
}

fn path_has_open_handle(item_path: &str, open_paths: &HashSet<String>) -> bool {
    let norm = item_path.trim_end_matches('/');
    let prefix = format!("{norm}/");
    open_paths
        .iter()
        .any(|op| op == norm || op.starts_with(&prefix))
}

pub fn clean_tmpfs(config: &Config, dry_run: bool) -> StepResult {
    let cfg = &config.tmpfs;
    if !cfg.enabled {
        return StepResult::new("tmpfs", "skipped", "Disabled in configuration", 0, vec![]);
    }

    let mut eligible_roots = Vec::new();
    let mut skipped_reasons = Vec::new();
    for raw in &cfg.paths {
        let p = PathBuf::from(raw);
        if !p.is_dir() {
            skipped_reasons.push(format!("{raw}: does not exist"));
            continue;
        }
        if cfg.require_tmpfs && !is_path_on_tmpfs(raw) {
            skipped_reasons.push(format!("{raw}: not a tmpfs mount (require_tmpfs=true)"));
            continue;
        }
        eligible_roots.push(p);
    }

    if eligible_roots.is_empty() {
        let reason = if skipped_reasons.is_empty() {
            "No eligible tmpfs paths".to_string()
        } else {
            skipped_reasons.join("; ")
        };
        return StepResult::new("tmpfs", "skipped", reason, 0, vec![]);
    }

    let mut open_paths: Option<HashSet<String>> = None;
    let now_secs = unix_now_secs() as i64;
    let min_age_secs = (cfg.min_age_minutes * 60) as i64;
    let euid = unsafe { libc::geteuid() };
    let mut removed_count: u64 = 0;
    let mut bytes_freed: u64 = 0;
    let mut details = Vec::new();

    for root in &eligible_roots {
        let root_fd = match open_dir_nofollow(root) {
            Ok(fd) => fd,
            Err(e) => {
                details.push(format!("Cannot open {}: {e}", root.display()));
                continue;
            }
        };
        let root_dev = match fstat_fd(root_fd.as_raw_fd()) {
            Ok(st) => st.st_dev,
            Err(e) => {
                details.push(format!("Cannot fstat {}: {e}", root.display()));
                continue;
            }
        };
        let entries = match read_dir_names_fd(root_fd.as_raw_fd()) {
            Ok(v) => v,
            Err(e) => {
                details.push(format!("Cannot scan {}: {e}", root.display()));
                continue;
            }
        };
        let root_real = fs::canonicalize(root)
            .ok()
            .and_then(|c| c.to_str().map(String::from))
            .unwrap_or_else(|| root.to_string_lossy().to_string());

        for cname in entries {
            let name = cname.to_string_lossy();
            if cfg.exclude_patterns.iter().any(|ex| glob_match(ex, &name)) {
                continue;
            }
            if !cfg.patterns.iter().any(|pat| glob_match(pat, &name)) {
                continue;
            }
            let Ok(st) = fstatat_nofollow(root_fd.as_raw_fd(), &cname) else {
                continue;
            };
            if cfg.owner_only && euid != 0 && st.st_uid != euid {
                continue;
            }
            if now_secs.saturating_sub(st.st_mtime) < min_age_secs {
                continue;
            }

            if cfg.skip_open_files {
                let op = open_paths.get_or_insert_with(|| get_open_paths_under(&eligible_roots));
                let canonical_item = format!("{}/{name}", root_real.trim_end_matches('/'));
                if path_has_open_handle(&canonical_item, op) {
                    continue;
                }
            }

            let item_display = root.join(name.as_ref());
            let is_dir = (st.st_mode & libc::S_IFMT) == libc::S_IFDIR;
            let size: u64 = if is_dir {
                if st.st_dev != root_dev {
                    continue;
                }
                let Ok(sub_fd) = openat_dir_nofollow(root_fd.as_raw_fd(), &cname) else {
                    continue;
                };
                let Ok(sub_st) = fstat_fd(sub_fd.as_raw_fd()) else {
                    continue;
                };
                if sub_st.st_dev != root_dev || sub_st.st_ino != st.st_ino {
                    continue;
                }
                let (sz, max_child_mtime) =
                    dir_size_and_max_mtime_fd(sub_fd.as_raw_fd(), root_dev, 0);
                if max_child_mtime > 0 && now_secs.saturating_sub(max_child_mtime) < min_age_secs {
                    continue;
                }
                sz
            } else {
                st.st_size.max(0) as u64
            };

            if dry_run {
                removed_count += 1;
                bytes_freed = bytes_freed.saturating_add(size);
                details.push(format!(
                    "[dry-run] Would remove {} ({:.1} MiB)",
                    item_display.display(),
                    size as f64 / (1024.0 * 1024.0)
                ));
                continue;
            }

            let res = if is_dir {
                rmtree_fd(root_fd.as_raw_fd(), &cname, root_dev, st.st_ino, 0)
            } else {
                unlinkat_entry(root_fd.as_raw_fd(), &cname, false)
            };
            match res {
                Ok(()) => {
                    removed_count += 1;
                    bytes_freed = bytes_freed.saturating_add(size);
                    details.push(format!(
                        "Removed {} ({:.1} MiB)",
                        item_display.display(),
                        size as f64 / (1024.0 * 1024.0)
                    ));
                }
                Err(e) => details.push(format!("Failed to remove {}: {e}", item_display.display())),
            }
        }
    }

    let status = if dry_run { "dry-run" } else { "ok" };
    if removed_count == 0 {
        return StepResult::new("tmpfs", status, "No stale tmpfs items matched", 0, details);
    }
    let verb = if dry_run { "Would remove" } else { "Removed" };
    StepResult::new(
        "tmpfs",
        status,
        format!(
            "{verb} {removed_count} stale item(s) ({:.1} MiB)",
            bytes_freed as f64 / (1024.0 * 1024.0)
        ),
        bytes_freed,
        details,
    )
}

pub fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) as i32 };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub fn pidfd_send_signal(pidfd: RawFd, sig: i32) -> io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn signal_via_pidfd(
    pid: i32,
    expected_starttime: Option<u64>,
    sig: i32,
    grace_seconds: u64,
    escalate_sigkill: bool,
) -> io::Result<&'static str> {
    let Some(expected_st) = expected_starttime else {
        return Ok("stale");
    };

    match pidfd_open(pid) {
        Ok(pfd) => {
            if read_proc_starttime(pid) != Some(expected_st) {
                return Ok("stale");
            }
            pidfd_send_signal(pfd.as_raw_fd(), sig)?;
            if grace_seconds == 0 && !escalate_sigkill {
                return Ok("sent");
            }
            let mut fds = [libc::pollfd {
                fd: pfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let timeout_ms = (grace_seconds.min(3600) * 1000) as i32;
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
            if rc > 0 {
                return Ok("exited");
            }
            if escalate_sigkill {
                pidfd_send_signal(pfd.as_raw_fd(), libc::SIGKILL)?;
                return Ok("escalated");
            }
            Ok("still_running")
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
            if read_proc_starttime(pid) != Some(expected_st) {
                return Ok("stale");
            }
            if unsafe { libc::kill(pid, sig) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if grace_seconds == 0 && !escalate_sigkill {
                return Ok("sent");
            }
            let deadline = Instant::now() + Duration::from_secs(grace_seconds);
            while Instant::now() < deadline {
                if read_proc_starttime(pid) != Some(expected_st) {
                    return Ok("exited");
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if read_proc_starttime(pid) != Some(expected_st) {
                return Ok("exited");
            }
            if escalate_sigkill {
                if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok("escalated");
            }
            Ok("still_running")
        }
        Err(e) => Err(e),
    }
}

pub fn run_process_watchdog(config: &Config, dry_run: bool) -> StepResult {
    let cfg = &config.watchdog;
    if !cfg.enabled || cfg.rules.is_empty() {
        return StepResult::new("watchdog", "skipped", "Disabled or no rules configured", 0, vec![]);
    }

    let euid = unsafe { libc::geteuid() };
    let min_rss = cfg.rules.iter().map(|r| r.max_rss_mb).min().unwrap_or(0) as f64;
    let procs = list_processes(min_rss, if euid == 0 { None } else { Some(euid) }, true);
    let my_pid = std::process::id() as i32;
    let mut actions_taken = 0u64;
    let mut details = Vec::new();

    for rule in &cfg.rules {
        let Ok(re) = Regex::new(&rule.pattern) else {
            continue;
        };
        for proc in &procs {
            if proc.pid <= 1 || proc.pid == my_pid {
                continue;
            }
            if proc.rss_mb < rule.max_rss_mb as f64 {
                continue;
            }
            if !re.is_match(&proc.comm) && !re.is_match(&proc.cmdline) {
                continue;
            }
            let msg = format!(
                "Rule '{}' matched PID {} ({}, RSS {:.1} MiB >= {} MiB)",
                rule.name, proc.pid, proc.comm, proc.rss_mb, rule.max_rss_mb
            );
            if rule.action == "notify" {
                details.push(format!("ALERT: {msg}"));
                actions_taken += 1;
                continue;
            }
            if dry_run {
                details.push(format!("[dry-run] Would apply '{}' to {msg}", rule.action));
                actions_taken += 1;
                continue;
            }
            let res = match rule.action.as_str() {
                "sigkill" => signal_via_pidfd(proc.pid, proc.starttime, libc::SIGKILL, 0, false),
                "sigterm" => signal_via_pidfd(proc.pid, proc.starttime, libc::SIGTERM, 0, false),
                "graceful" => signal_via_pidfd(
                    proc.pid,
                    proc.starttime,
                    libc::SIGTERM,
                    rule.grace_seconds,
                    rule.allow_sigkill,
                ),
                _ => continue,
            };
            match res {
                Ok("stale") => {}
                Ok("exited") => {
                    details.push(format!("Gracefully stopped (SIGTERM): {msg}"));
                    actions_taken += 1;
                }
                Ok("escalated") => {
                    details.push(format!(
                        "Escalated to SIGKILL after {}s: {msg}",
                        rule.grace_seconds
                    ));
                    actions_taken += 1;
                }
                Ok("still_running") => {
                    details.push(format!(
                        "Sent SIGTERM (still running after {}s; allow_sigkill=false): {msg}",
                        rule.grace_seconds
                    ));
                    actions_taken += 1;
                }
                Ok("sent") => {
                    details.push(format!("Sent {}: {msg}", rule.action.to_uppercase()));
                    actions_taken += 1;
                }
                Ok(_) => {}
                Err(e) => details.push(format!("Failed signalling PID {} ({}): {e}", proc.pid, proc.comm)),
            }
        }
    }

    let status = if dry_run { "dry-run" } else { "ok" };
    if actions_taken == 0 {
        return StepResult::new(
            "watchdog",
            status,
            "No runaway processes exceeded thresholds",
            0,
            details,
        );
    }
    StepResult::new(
        "watchdog",
        status,
        format!("Handled {actions_taken} process rule match(es)"),
        0,
        details,
    )
}

fn lookup_user(username: &str) -> Option<(u32, PathBuf)> {
    let cname = CString::new(username).ok()?;
    let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    let uid = unsafe { (*pw).pw_uid };
    let dir = unsafe { CStr::from_ptr((*pw).pw_dir) }
        .to_string_lossy()
        .into_owned();
    Some((uid, PathBuf::from(dir)))
}

fn resolve_target_users(target_users: &[String]) -> Vec<(u32, PathBuf)> {
    if !target_users.is_empty() {
        return target_users
            .iter()
            .filter_map(|u| lookup_user(u))
            .collect();
    }
    if is_root() {
        if let Ok(sudo_user) = env::var("SUDO_USER") {
            if let Some(pair) = lookup_user(&sudo_user) {
                return vec![pair];
            }
        }
        return Vec::new();
    }
    vec![(unsafe { libc::geteuid() }, home_dir())]
}

fn prune_old_files_fd(
    dir_fd: RawFd,
    cutoff_secs: i64,
    dry_run: bool,
    root_dev: u64,
    expected_uid: Option<u32>,
    depth: usize,
) -> (u64, u64) {
    if depth >= MAX_WALK_DEPTH {
        return (0, 0);
    }
    let Ok(names) = read_dir_names_fd(dir_fd) else {
        return (0, 0);
    };
    let (mut count, mut freed) = (0u64, 0u64);
    for name in names {
        let Ok(st) = fstatat_nofollow(dir_fd, &name) else {
            continue;
        };
        if let Some(uid) = expected_uid {
            if st.st_uid != uid {
                continue;
            }
        }
        let ftype = st.st_mode & libc::S_IFMT;
        if ftype == libc::S_IFLNK {
            continue;
        }
        if ftype == libc::S_IFDIR {
            if st.st_dev != root_dev {
                continue;
            }
            if let Ok(sub_fd) = openat_dir_nofollow(dir_fd, &name) {
                if let Ok(sub_st) = fstat_fd(sub_fd.as_raw_fd()) {
                    if sub_st.st_dev == root_dev && sub_st.st_ino == st.st_ino {
                        let (c, b) = prune_old_files_fd(
                            sub_fd.as_raw_fd(),
                            cutoff_secs,
                            dry_run,
                            root_dev,
                            expected_uid,
                            depth + 1,
                        );
                        count += c;
                        freed = freed.saturating_add(b);
                    }
                }
            }
        } else if ftype == libc::S_IFREG
            && st.st_atime < cutoff_secs
            && st.st_mtime < cutoff_secs
            && (dry_run || unlinkat_entry(dir_fd, &name, false).is_ok())
        {
            count += 1;
            freed = freed.saturating_add(st.st_size.max(0) as u64);
        }
    }
    (count, freed)
}

pub fn clean_user_caches(config: &Config, dry_run: bool) -> StepResult {
    let cfg = &config.user_caches;
    if !cfg.enabled {
        return StepResult::new("user_caches", "skipped", "Disabled in configuration", 0, vec![]);
    }
    let targets = resolve_target_users(&cfg.target_users);
    if targets.is_empty() {
        return StepResult::new(
            "user_caches",
            "skipped",
            "Running as root with no target_users configured",
            0,
            vec![],
        );
    }

    let now_secs = unix_now_secs() as i64;
    let root_user = is_root();
    let mut removed_files = 0u64;
    let mut bytes_freed = 0u64;
    let mut details = Vec::new();

    for (uid, home) in &targets {
        let uid_guard = if root_user { Some(*uid) } else { None };

        if cfg.clean_thumbnails {
            let parts = [OsStr::new(".cache"), OsStr::new("thumbnails")];
            if let Some((dfd, dev)) = open_subpath_nofollow(home, &parts, uid_guard) {
                let cutoff = now_secs - (cfg.thumbnail_max_age_days as i64 * 86400);
                let (c, b) = prune_old_files_fd(dfd.as_raw_fd(), cutoff, dry_run, dev, uid_guard, 0);
                removed_files += c;
                bytes_freed = bytes_freed.saturating_add(b);
                if c > 0 {
                    details.push(format!(
                        "{}Pruned {c} thumbnail(s) in {} ({:.1} MiB)",
                        if dry_run { "[dry-run] " } else { "" },
                        home.join(".cache/thumbnails").display(),
                        b as f64 / (1024.0 * 1024.0)
                    ));
                }
            }
        }

        if cfg.clean_pip_cache {
            let parts = [OsStr::new(".cache")];
            if let Some((cache_fd, dev)) = open_subpath_nofollow(home, &parts, uid_guard) {
                let pip_c = CString::new("pip").unwrap();
                if let Ok(pip_st) = fstatat_nofollow(cache_fd.as_raw_fd(), &pip_c) {
                    let ok_dir = (pip_st.st_mode & libc::S_IFMT) == libc::S_IFDIR
                        && pip_st.st_dev == dev
                        && uid_guard.map(|u| pip_st.st_uid == u).unwrap_or(true);
                    if ok_dir {
                        if let Ok(pip_fd) = openat_dir_nofollow(cache_fd.as_raw_fd(), &pip_c) {
                            let (size, _) = dir_size_and_max_mtime_fd(pip_fd.as_raw_fd(), dev, 0);
                            drop(pip_fd);
                            if size > 0 {
                                if dry_run {
                                    bytes_freed = bytes_freed.saturating_add(size);
                                    details.push(format!(
                                        "[dry-run] Would purge pip cache {} ({:.1} MiB)",
                                        home.join(".cache/pip").display(),
                                        size as f64 / (1024.0 * 1024.0)
                                    ));
                                } else if rmtree_fd(
                                    cache_fd.as_raw_fd(),
                                    &pip_c,
                                    dev,
                                    pip_st.st_ino,
                                    0,
                                )
                                .is_ok()
                                {
                                    bytes_freed = bytes_freed.saturating_add(size);
                                    details.push(format!(
                                        "Purged pip cache {} ({:.1} MiB)",
                                        home.join(".cache/pip").display(),
                                        size as f64 / (1024.0 * 1024.0)
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        for extra in &cfg.extra_cache_dirs {
            let rel_path: PathBuf = if let Some(rest) = extra.strip_prefix("~/") {
                PathBuf::from(rest)
            } else if !Path::new(extra).is_absolute() {
                PathBuf::from(extra)
            } else {
                match Path::new(extra).strip_prefix(home) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                }
            };
            let parts: Vec<&OsStr> = rel_path
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s),
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                continue;
            }
            if let Some((dfd, dev)) = open_subpath_nofollow(home, &parts, uid_guard) {
                let cutoff = now_secs - (cfg.extra_cache_max_age_days as i64 * 86400);
                let (c, b) = prune_old_files_fd(dfd.as_raw_fd(), cutoff, dry_run, dev, uid_guard, 0);
                removed_files += c;
                bytes_freed = bytes_freed.saturating_add(b);
                if c > 0 {
                    details.push(format!(
                        "{}Pruned {c} file(s) in {} ({:.1} MiB)",
                        if dry_run { "[dry-run] " } else { "" },
                        home.join(&rel_path).display(),
                        b as f64 / (1024.0 * 1024.0)
                    ));
                }
            }
        }
    }

    let status = if dry_run { "dry-run" } else { "ok" };
    if bytes_freed == 0 && removed_files == 0 {
        return StepResult::new("user_caches", status, "User caches already clean", 0, details);
    }
    let verb = if dry_run { "Would free" } else { "Freed" };
    StepResult::new(
        "user_caches",
        status,
        format!("{verb} {:.1} MiB from user caches", bytes_freed as f64 / (1024.0 * 1024.0)),
        bytes_freed,
        details,
    )
}

pub fn reclaim_kernel_memory(config: &Config, dry_run: bool, critical: bool) -> StepResult {
    let cfg = &config.kernel;
    if !cfg.enabled {
        return StepResult::new("kernel", "skipped", "Disabled in configuration", 0, vec![]);
    }
    if !is_root() && !dry_run {
        return StepResult::new(
            "kernel",
            "skipped",
            "Requires root privileges (run via sudo or system systemd service)",
            0,
            vec![],
        );
    }

    let mut details = Vec::new();
    let mut actions = 0u32;
    let mut errors = 0u32;

    let cg_path = Path::new("/sys/fs/cgroup/memory.reclaim");
    if cfg.use_cgroup_reclaim && cfg.cgroup_reclaim_mb > 0 && cg_path.exists() {
        let target_bytes = cfg.cgroup_reclaim_mb * 1024 * 1024;
        if dry_run {
            details.push(format!(
                "[dry-run] Would write {}M to {}",
                cfg.cgroup_reclaim_mb,
                cg_path.display()
            ));
            actions += 1;
        } else {
            match fs::write(cg_path, format!("{target_bytes}\n")) {
                Ok(()) => {
                    details.push(format!(
                        "Proactively reclaimed up to {} MiB via cgroup v2 memory.reclaim",
                        cfg.cgroup_reclaim_mb
                    ));
                    actions += 1;
                }
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {
                    details.push(
                        "cgroup v2 memory.reclaim reclaimed available cold pages (EAGAIN)".into(),
                    );
                    actions += 1;
                }
                Err(e) => {
                    details.push(format!("cgroup v2 memory.reclaim failed: {e}"));
                    errors += 1;
                }
            }
        }
    }

    let should_drop = cfg.drop_caches_level > 0 && (!cfg.drop_caches_on_critical_only || critical);
    if should_drop {
        let drop_path = Path::new("/proc/sys/vm/drop_caches");
        if dry_run {
            details.push(format!(
                "[dry-run] Would sync and write '{}' to {}",
                cfg.drop_caches_level,
                drop_path.display()
            ));
            actions += 1;
        } else {
            unsafe { libc::sync() };
            match fs::write(drop_path, format!("{}\n", cfg.drop_caches_level)) {
                Ok(()) => {
                    details.push(format!(
                        "Flushed kernel caches (drop_caches={})",
                        cfg.drop_caches_level
                    ));
                    actions += 1;
                }
                Err(e) => {
                    details.push(format!("drop_caches failed: {e}"));
                    errors += 1;
                }
            }
        }
    }

    let compact_path = Path::new("/proc/sys/vm/compact_memory");
    if cfg.compact_memory && compact_path.exists() {
        if dry_run {
            details.push(format!("[dry-run] Would write '1' to {}", compact_path.display()));
            actions += 1;
        } else {
            match fs::write(compact_path, "1\n") {
                Ok(()) => {
                    details.push("Triggered kernel memory compaction".into());
                    actions += 1;
                }
                Err(e) => {
                    details.push(format!("Memory compaction failed: {e}"));
                    errors += 1;
                }
            }
        }
    }

    if errors > 0 && actions == 0 {
        return StepResult::new(
            "kernel",
            "error",
            format!("Kernel reclaim failed ({errors} error(s))"),
            0,
            details,
        );
    }
    if actions == 0 {
        return StepResult::new(
            "kernel",
            "skipped",
            "No kernel reclaim actions enabled/applicable",
            0,
            details,
        );
    }
    let status = if dry_run {
        "dry-run"
    } else if errors > 0 {
        "error"
    } else {
        "ok"
    };
    StepResult::new(
        "kernel",
        status,
        format!(
            "Executed {actions} kernel memory optimization(s){}",
            if errors > 0 {
                format!(" ({errors} error(s))")
            } else {
                String::new()
            }
        ),
        0,
        details,
    )
}

fn resolve_trusted_binary(name: &str) -> Option<PathBuf> {
    let root_user = is_root();
    for dir in TRUSTED_SBIN_PATH.split(':') {
        let cand = Path::new(dir).join(name);
        let Ok(meta) = fs::metadata(&cand) else {
            continue;
        };
        if !meta.file_type().is_file() || (meta.mode() & 0o111) == 0 {
            continue;
        }
        if root_user && (meta.uid() != 0 || (meta.mode() & 0o022) != 0) {
            continue;
        }
        return Some(cand);
    }
    None
}

fn is_swapfile_writable(dev_path: &str) -> bool {
    let Ok(cpath) = CString::new(dev_path) else {
        return false;
    };
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut vfs) } != 0 {
        return false;
    }
    if (vfs.f_flag & libc::ST_RDONLY) != 0 {
        return false;
    }
    unsafe { libc::access(cpath.as_ptr(), libc::W_OK) == 0 }
}

pub fn flush_swap(config: &Config, snapshot: Option<&SystemSnapshot>, dry_run: bool) -> StepResult {
    let cfg = &config.swap;
    if !cfg.enabled {
        return StepResult::new("swap", "skipped", "Disabled in configuration", 0, vec![]);
    }
    if !is_root() && !dry_run {
        return StepResult::new(
            "swap",
            "skipped",
            "Requires root privileges (run via sudo or system systemd service)",
            0,
            vec![],
        );
    }

    let owned_snap;
    let snap = match snapshot {
        Some(s) => s,
        None => {
            owned_snap = collect_snapshot(None, false, false, false, true);
            &owned_snap
        }
    };
    if cfg.max_load_avg_ratio > 0.0 && snap.load_ratio() > cfg.max_load_avg_ratio {
        return StepResult::new(
            "swap",
            "skipped",
            format!(
                "Deferred: CPU load ratio {:.2} > {:.2}",
                snap.load_ratio(),
                cfg.max_load_avg_ratio
            ),
            0,
            vec![],
        );
    }

    let swaps = read_swaps();
    let mut eligible = Vec::new();
    let mut skipped_zram = 0u32;
    let mut skipped_ro = 0u32;
    for dev in swaps {
        if cfg.skip_zram && dev.is_zram() {
            skipped_zram += 1;
            continue;
        }
        if dev.used_mb < cfg.min_swap_used_mb {
            continue;
        }
        if dev.swap_type == "file" && !dry_run && !is_swapfile_writable(&dev.path) {
            skipped_ro += 1;
            continue;
        }
        eligible.push(dev);
    }

    if eligible.is_empty() {
        let mut notes = Vec::new();
        if skipped_zram > 0 {
            notes.push(format!("{skipped_zram} zram device(s) skipped"));
        }
        if skipped_ro > 0 {
            notes.push(format!("{skipped_ro} read-only swapfile(s) skipped"));
        }
        let suffix = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        return StepResult::new(
            "swap",
            "skipped",
            format!(
                "No disk swap device exceeds min_swap_used_mb={}{suffix}",
                cfg.min_swap_used_mb
            ),
            0,
            vec![],
        );
    }

    let mem = read_meminfo();
    let total_to_flush: u64 = eligible.iter().map(|d| d.used_mb).sum();
    let required_avail = total_to_flush + cfg.safety_margin_mb;
    if mem.available_mb < required_avail {
        return StepResult::new(
            "swap",
            "skipped",
            format!(
                "Insufficient RAM headroom: {} MiB available < {required_avail} MiB required (swap {total_to_flush} + margin {})",
                mem.available_mb, cfg.safety_margin_mb
            ),
            0,
            vec![],
        );
    }

    let swapoff_bin = resolve_trusted_binary("swapoff");
    let swapon_bin = resolve_trusted_binary("swapon");
    if !dry_run && (swapoff_bin.is_none() || swapon_bin.is_none()) {
        return StepResult::new("swap", "error", "Trusted swapoff/swapon binaries not found", 0, vec![]);
    }

    let mut details = Vec::new();
    let mut flushed_mb = 0u64;
    let mut succeeded = 0u32;
    let mut failed = 0u32;

    for dev in &eligible {
        if dry_run {
            details.push(format!(
                "[dry-run] Would cycle swap device {} (priority {}, {} MiB used)",
                dev.path, dev.priority, dev.used_mb
            ));
            flushed_mb += dev.used_mb;
            succeeded += 1;
            continue;
        }
        let off_bin = swapoff_bin.as_ref().unwrap();
        let on_bin = swapon_bin.as_ref().unwrap();
        let prio_s = dev.priority.to_string();

        let off_ok = Command::new(off_bin)
            .args(["--", &dev.path])
            .env_clear()
            .env("PATH", TRUSTED_SBIN_PATH)
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let on_ok = if off_ok {
            Command::new(on_bin)
                .args(["-p", &prio_s, "--", &dev.path])
                .env_clear()
                .env("PATH", TRUSTED_SBIN_PATH)
                .env("LC_ALL", "C")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            false
        };

        if off_ok && on_ok {
            details.push(format!("Flushed swap device {} ({} MiB)", dev.path, dev.used_mb));
            flushed_mb += dev.used_mb;
            succeeded += 1;
        } else {
            failed += 1;
            details.push(format!("Failed cycling {}", dev.path));
            let _ = Command::new(on_bin)
                .args(["-p", &prio_s, "--", &dev.path])
                .env_clear()
                .env("PATH", TRUSTED_SBIN_PATH)
                .env("LC_ALL", "C")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    if failed > 0 {
        return StepResult::new(
            "swap",
            "error",
            format!("Swap flush encountered {failed} error(s) ({succeeded} succeeded, {flushed_mb} MiB flushed)"),
            flushed_mb * 1024 * 1024,
            details,
        );
    }
    let status = if dry_run { "dry-run" } else { "ok" };
    let verb = if dry_run { "Would flush" } else { "Flushed" };
    StepResult::new(
        "swap",
        status,
        format!("{verb} {flushed_mb} MiB across {succeeded} disk swap device(s)"),
        flushed_mb * 1024 * 1024,
        details,
    )
}

pub fn run_cleanup(
    config: &Config,
    reasons: Vec<String>,
    dry_run: bool,
    critical: bool,
    only_steps: Option<&[String]>,
) -> CleanupReport {
    let started_at = unix_now_secs();
    let start_mono = Instant::now();
    let pre_snap = collect_snapshot(Some(&config.tmpfs.paths), false, false, false, true);

    let want = |name: &str| -> bool {
        match only_steps {
            Some(list) => list.iter().any(|s| s == name),
            None => true,
        }
    };

    let mut steps = Vec::new();
    let mut cgroup_reclaimed = false;

    if want("tmpfs") {
        steps.push(clean_tmpfs(config, dry_run));
    }
    if want("watchdog") {
        steps.push(run_process_watchdog(config, dry_run));
    }
    if want("user_caches") {
        steps.push(clean_user_caches(config, dry_run));
    }
    if want("kernel") {
        let kres = reclaim_kernel_memory(config, dry_run, critical);
        if kres.status == "ok" && kres.details.iter().any(|d| d.contains("memory.reclaim")) {
            cgroup_reclaimed = true;
        }
        steps.push(kres);
    }
    if want("swap") {
        if cgroup_reclaimed && only_steps.is_none() {
            steps.push(StepResult::new(
                "swap",
                "skipped",
                "Skipped to avoid reversing cgroup v2 memory.reclaim in the same pass",
                0,
                vec![],
            ));
        } else {
            steps.push(flush_swap(config, Some(&pre_snap), dry_run));
        }
    }

    let post_mem = read_meminfo();
    let duration_seconds = ((start_mono.elapsed().as_secs_f64()) * 100.0).round() / 100.0;
    let has_errors = steps.iter().any(|s| s.status == "error");

    CleanupReport {
        started_at,
        finished_at: unix_now_secs(),
        duration_seconds,
        dry_run,
        critical,
        reasons: if reasons.is_empty() {
            vec!["Manual invocation".into()]
        } else {
            reasons
        },
        pre_available_mb: pre_snap.memory.available_mb,
        post_available_mb: post_mem.available_mb,
        ram_freed_mb: post_mem.available_mb as i64 - pre_snap.memory.available_mb as i64,
        pre_swap_used_mb: pre_snap.memory.swap_used_mb,
        post_swap_used_mb: post_mem.swap_used_mb,
        swap_freed_mb: pre_snap.memory.swap_used_mb as i64 - post_mem.swap_used_mb as i64,
        has_errors,
        steps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matcher_works() {
        assert!(glob_match("npm-*", "npm-cache-123"));
        assert!(!glob_match("npm-*", "pip-cache"));
        assert!(glob_match("a?c*z", "abchelloz"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn tmpfs_cleaner_respects_age_exclude_and_open_files() {
        let root = std::env::temp_dir().join(format!("krc-tmpfs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let old_dir = root.join("npm-old-1");
        fs::create_dir_all(&old_dir).unwrap();
        let old_file = old_dir.join("blob.bin");
        fs::write(&old_file, vec![b'x'; 4096]).unwrap();

        let fresh_dir = root.join("npm-fresh-2");
        fs::create_dir_all(&fresh_dir).unwrap();

        let excluded = root.join("ssh-agent-1");
        fs::create_dir_all(&excluded).unwrap();

        let open_file = root.join("tmp-open-1");
        fs::write(&open_file, b"active").unwrap();

        // Backdate old_dir, old_file, excluded, open_file by 2 hours
        let old_ts = (unix_now_secs() as i64) - 7200;
        let times = [
            libc::timespec { tv_sec: old_ts, tv_nsec: 0 },
            libc::timespec { tv_sec: old_ts, tv_nsec: 0 },
        ];
        for p in [&old_file, &old_dir, &excluded, &open_file] {
            let cp = CString::new(p.as_os_str().as_bytes()).unwrap();
            unsafe { libc::utimensat(libc::AT_FDCWD, cp.as_ptr(), times.as_ptr(), 0) };
        }

        let mut cfg = Config::default();
        cfg.tmpfs.paths = vec![root.to_string_lossy().to_string()];
        cfg.tmpfs.require_tmpfs = false;
        cfg.tmpfs.patterns = vec!["npm-*".into(), "ssh-*".into(), "tmp-*".into()];
        cfg.tmpfs.exclude_patterns = vec!["ssh-*".into()];
        cfg.tmpfs.min_age_minutes = 30;
        cfg.tmpfs.skip_open_files = true;

        let _held_open = fs::File::open(&open_file).unwrap();
        let dry = clean_tmpfs(&cfg, true);
        assert_eq!(dry.status, "dry-run");
        assert!(old_dir.exists());

        let real = clean_tmpfs(&cfg, false);
        assert_eq!(real.status, "ok");
        assert!(!old_dir.exists());
        assert!(fresh_dir.exists());
        assert!(excluded.exists());
        assert!(open_file.exists());

        drop(_held_open);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn user_caches_refuses_symlinks() {
        let base = std::env::temp_dir().join(format!("krc-uc-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let home = base.join("home");
        let victim = base.join("victim");
        fs::create_dir_all(&victim).unwrap();
        let vfile = victim.join("important.txt");
        fs::write(&vfile, b"keep").unwrap();

        let cache = home.join(".cache");
        fs::create_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(&victim, cache.join("thumbnails")).unwrap();
        std::os::unix::fs::symlink(&victim, cache.join("pip")).unwrap();

        assert!(open_subpath_nofollow(&home, &[OsStr::new(".cache"), OsStr::new("thumbnails")], None).is_none());
        assert!(open_subpath_nofollow(&home, &[], None).is_none());
        assert!(vfile.exists());

        let _ = fs::remove_dir_all(&base);
    }
}
