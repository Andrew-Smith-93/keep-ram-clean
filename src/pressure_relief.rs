//! Standalone, opt-in intervention for explicitly registered user workloads.
use crate::cleaners::{pidfd_open, pidfd_send_signal};
use std::{
    collections::HashSet,
    fs, io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(2);
const SUSTAIN: Duration = Duration::from_secs(20);
const COOLDOWN: Duration = Duration::from_secs(60);
const MIN_RSS_KIB: u64 = 64 * 1024;
const MAX_WORKLOADS: usize = 16;

fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let mut matches = body.lines().filter_map(|line| line.strip_prefix(key));
    let value = matches.next()?.trim();
    matches.next().is_none().then_some(value)
}

fn mem_kib(body: &str, key: &str) -> Option<u64> {
    let mut words = field(body, key)?.split_whitespace();
    let n = words.next()?.parse().ok()?;
    (words.next()? == "kB" && words.next().is_none()).then_some(n)
}

fn pressured(body: &str) -> Option<bool> {
    let total = mem_kib(body, "MemTotal:")?;
    let avail = mem_kib(body, "MemAvailable:")?;
    let swap = mem_kib(body, "SwapTotal:")?;
    let free = mem_kib(body, "SwapFree:")?;
    if total == 0 || avail > total || free > swap {
        return None;
    }
    let under = |remaining: u64, total: u64, percent: u128| {
        u128::from(remaining) * 100 <= u128::from(total) * percent
    };
    // Machines without swap require the stricter RAM threshold.
    Some(if swap == 0 {
        under(avail, total, 5)
    } else {
        under(avail, total, 10) && under(free, swap, 10)
    })
}

#[derive(Default)]
struct PressureGate {
    since: Option<Duration>,
    previous: Option<Duration>,
    attempted: Option<Duration>,
}

impl PressureGate {
    fn ready(&mut self, now: Duration, pressure: Option<bool>) -> bool {
        if self
            .previous
            .is_none_or(|prev| now.saturating_sub(prev) > INTERVAL * 2)
            || pressure != Some(true)
        {
            self.since = None;
        }
        self.previous = Some(now);
        pressure == Some(true)
            && now.saturating_sub(*self.since.get_or_insert(now)) >= SUSTAIN
            && self
                .attempted
                .is_none_or(|prev| now.saturating_sub(prev) >= COOLDOWN)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    start: u64,
    exe: PathBuf,
    exe_dev: u64,
    exe_ino: u64,
    proc_ino: u64,
}

struct Sample {
    identity: Identity,
    rss: u64,
    score: u32,
}

fn regular_user(status: &str, uid: u32) -> Option<()> {
    let uids: Vec<u32> = field(status, "Uid:")?
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if uid < 1000 || uids != [uid; 4] {
        return None;
    }
    for cap in ["CapPrm:", "CapEff:", "CapAmb:"] {
        if u64::from_str_radix(field(status, cap)?, 16).ok()? != 0 {
            return None;
        }
    }
    Some(())
}

fn sample(procfs: &Path, pid: u32, uid: u32, allowed: &[PathBuf]) -> Option<Sample> {
    if pid < 2 {
        return None;
    }
    let dir = procfs.join(pid.to_string());
    let proc_ino = fs::metadata(&dir).ok()?.ino();
    let stat = fs::read_to_string(dir.join("stat")).ok()?;
    let (lead, rest) = stat.rsplit_once(") ")?;
    if lead.split_once(" (")?.0.parse::<u32>().ok()? != pid {
        return None;
    }
    let mut fields = rest.split_whitespace();
    if !matches!(fields.next()?, "R" | "S" | "D") {
        return None;
    }
    // Field 22 (starttime), after consuming field 3 (state).
    let start = fields.nth(18)?.parse().ok()?;
    let status = fs::read_to_string(dir.join("status")).ok()?;
    regular_user(&status, uid)?;
    if field(&status, "Tgid:")?.parse::<u32>().ok()? != pid {
        return None;
    }
    let exe = fs::read_link(dir.join("exe")).ok()?;
    if !allowed.contains(&exe) {
        return None;
    }
    let meta = fs::metadata(dir.join("exe")).ok()?;
    if !valid_exe(&meta, uid) {
        return None;
    }
    let score = fs::read_to_string(dir.join("oom_score"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let adjustment: i32 = fs::read_to_string(dir.join("oom_score_adj"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if adjustment < 0 || fs::metadata(&dir).ok()?.ino() != proc_ino {
        return None;
    }
    Some(Sample {
        identity: Identity {
            start,
            exe,
            exe_dev: meta.dev(),
            exe_ino: meta.ino(),
            proc_ino,
        },
        rss: mem_kib(&status, "VmRSS:")?,
        score,
    })
}

fn valid_exe(meta: &fs::Metadata, uid: u32) -> bool {
    meta.is_file() && meta.uid() == uid && meta.mode() & 0o6022 == 0 && meta.mode() & 0o100 != 0
}

fn ancestors(procfs: &Path, self_pid: u32) -> Option<HashSet<u32>> {
    let mut seen = HashSet::new();
    let mut pid = self_pid;
    while seen.len() < 128 && seen.insert(pid) {
        if pid == 1 {
            return Some(seen);
        }
        let status = fs::read_to_string(procfs.join(pid.to_string()).join("status")).ok()?;
        pid = field(&status, "PPid:")?.parse().ok()?;
        if pid == 0 {
            return None;
        }
    }
    None
}

struct Workload {
    pid: u32,
    // The same pinned descriptor is kept for the entire watch session.
    fd: OwnedFd,
    identity: Identity,
    attempted: bool,
}

fn fd_matches(procfs: &Path, fd: &OwnedFd, pid: u32) -> bool {
    let info = procfs.join("self/fdinfo").join(fd.as_raw_fd().to_string());
    fs::read_to_string(info)
        .ok()
        .and_then(|s| field(&s, "Pid:")?.parse::<u32>().ok())
        == Some(pid)
}

fn register(procfs: &Path, pid: u32, uid: u32, allowed: &[PathBuf]) -> Option<Workload> {
    let proc_dir = fs::File::open(procfs.join(pid.to_string())).ok()?;
    let before = sample(procfs, pid, uid, allowed)?.identity;
    if before.proc_ino != proc_dir.metadata().ok()?.ino() {
        return None;
    }
    let fd = pidfd_open(i32::try_from(pid).ok()?).ok()?;
    let after = sample(procfs, pid, uid, allowed)?.identity;
    (before == after && fd_matches(procfs, &fd, pid)).then_some(Workload {
        pid,
        fd,
        identity: before,
        attempted: false,
    })
}

fn rank(sample: &Sample, pid: u32) -> (u32, u64, std::cmp::Reverse<u32>) {
    (sample.score, sample.rss, std::cmp::Reverse(pid))
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Preview,
    Refused,
    TermSent,
}

fn act(
    original: &Identity,
    now: Option<&Identity>,
    terminate: bool,
    sink: impl FnOnce() -> io::Result<()>,
) -> io::Result<Action> {
    if now != Some(original) {
        return Ok(Action::Refused);
    }
    if !terminate {
        return Ok(Action::Preview);
    }
    sink()?;
    Ok(Action::TermSent)
}

fn validate_args(pids: &[u32], paths: &[PathBuf]) -> Result<(), &'static str> {
    if pids.is_empty()
        || pids.len() > MAX_WORKLOADS
        || pids.iter().any(|&pid| pid < 2 || pid > i32::MAX as u32)
        || pids.iter().collect::<HashSet<_>>().len() != pids.len()
    {
        return Err("pass 1–16 distinct explicit workload PIDs (PID 1 is forbidden)");
    }
    if paths.is_empty()
        || paths.len() > MAX_WORKLOADS
        || paths.iter().any(|path| {
            !path.is_absolute()
                || path
                    .as_os_str()
                    .as_encoded_bytes()
                    .iter()
                    .any(|c| c.is_ascii_control() || matches!(*c, b'*' | b'?' | b'[' | b']'))
        })
    {
        return Err("pass 1–16 exact, absolute executable paths; no wildcards");
    }
    Ok(())
}

pub fn watch(pids: &[u32], paths: &[PathBuf], terminate: bool) -> Result<(), String> {
    validate_args(pids, paths).map_err(str::to_owned)?;
    let procfs = Path::new("/proc");
    let uid = unsafe { libc::getuid() };
    let self_pid = std::process::id();
    let own_status = fs::read_to_string(procfs.join("self/status")).map_err(|e| e.to_string())?;
    if regular_user(&own_status, uid).is_none()
        || field(&own_status, "Pid:").and_then(|p| p.parse().ok()) != Some(self_pid)
    {
        return Err("run as a regular non-root user (UID >= 1000) without added privileges".into());
    }
    for path in paths {
        if fs::canonicalize(path).as_ref().ok() != Some(path)
            || !fs::metadata(path).is_ok_and(|m| valid_exe(&m, uid))
        {
            return Err(format!(
                "not an exact, canonical, user-owned executable: {path:?}"
            ));
        }
    }
    let protected = ancestors(procfs, self_pid).ok_or("unable to verify parent process chain")?;
    let mut workloads = Vec::new();
    for &pid in pids {
        if protected.contains(&pid) {
            return Err(format!("PID {pid} is self or an ancestor"));
        }
        workloads.push(register(procfs, pid, uid, paths).ok_or_else(|| {
            format!("cannot safely register PID {pid}; check ownership, executable, OOM protection and pidfd support")
        })?);
    }
    eprintln!(
        "[pressure] {}; watching {} registered workload(s); Ctrl-C stops.",
        if terminate {
            "SIGTERM explicitly enabled"
        } else {
            "report only (no signals)"
        },
        workloads.len()
    );
    let clock = Instant::now();
    let mut gate = PressureGate::default();
    loop {
        let now = clock.elapsed();
        let memory = || {
            fs::read_to_string(procfs.join("meminfo"))
                .ok()
                .and_then(|s| pressured(&s))
        };
        if gate.ready(now, memory()) {
            let protected =
                ancestors(procfs, self_pid).ok_or("unable to verify parent process chain")?;
            let candidate = workloads
                .iter_mut()
                .filter(|w| {
                    !w.attempted && !protected.contains(&w.pid) && fd_matches(procfs, &w.fd, w.pid)
                })
                .filter_map(|w| {
                    let s = sample(procfs, w.pid, uid, paths)?;
                    (s.identity == w.identity && s.rss >= MIN_RSS_KIB).then_some((w, s))
                })
                .max_by_key(|(w, s)| rank(s, w.pid));
            if let Some((w, s)) = candidate {
                gate.attempted = Some(now);
                let current = sample(procfs, w.pid, uid, paths);
                let ready = memory() == Some(true)
                    && ancestors(procfs, self_pid).is_some_and(|chain| !chain.contains(&w.pid))
                    && fd_matches(procfs, &w.fd, w.pid)
                    && current.as_ref().is_some_and(|a| a.rss >= MIN_RSS_KIB);
                let signal = || pidfd_send_signal(w.fd.as_raw_fd(), libc::SIGTERM);
                if terminate {
                    w.attempted = true;
                }
                match act(&w.identity, current.as_ref().filter(|_| ready).map(|s| &s.identity), terminate, signal) {
                    Ok(Action::Preview) => eprintln!("[pressure] would SIGTERM PID {} ({:?}, RSS {} MiB, OOM score {}); no signal sent", w.pid, s.identity.exe, s.rss / 1024, s.score),
                    Ok(Action::TermSent) => eprintln!("[pressure] sent SIGTERM to PID {} ({:?}, RSS {} MiB, OOM score {})", w.pid, s.identity.exe, s.rss / 1024, s.score),
                    Ok(Action::Refused) => eprintln!("[pressure] PID {} no longer qualifies; no signal sent", w.pid),
                    Err(e) => eprintln!("[pressure] SIGTERM failed for PID {}: {e}; no retry or escalation", w.pid),
                }
            }
        }
        if !workloads
            .iter()
            .any(|w| !w.attempted && fd_matches(procfs, &w.fd, w.pid))
        {
            eprintln!(
                "[pressure] no registered workload remains eligible for an attempt; stopping"
            );
            return Ok(());
        }
        thread::sleep(INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        os::unix::fs::{symlink, PermissionsExt},
    };

    fn mem(avail: u64, swap: u64, free: u64) -> String {
        format!("MemTotal: 1000 kB\nMemAvailable: {avail} kB\nSwapTotal: {swap} kB\nSwapFree: {free} kB\n")
    }

    #[test]
    fn pressure_requires_both_limits_or_stricter_no_swap_limit() {
        assert_eq!(pressured(&mem(100, 1000, 100)), Some(true));
        assert_eq!(pressured(&mem(100, 1000, 101)), Some(false));
        assert_eq!(pressured(&mem(101, 1000, 100)), Some(false));
        assert_eq!(pressured(&mem(50, 0, 0)), Some(true));
        assert_eq!(pressured(&mem(51, 0, 0)), Some(false));
        assert_eq!(pressured(&mem(50, 0, 1)), None);
        assert_eq!(pressured("MemTotal: 1000 kB\nMemFree: 1 kB"), None);
    }

    #[test]
    fn sustained_pressure_resets_on_unknown_or_gap_and_respects_cooldown() {
        let mut gate = PressureGate::default();
        let s = Duration::from_secs;
        for t in (0..20).step_by(2) {
            assert!(!gate.ready(s(t), Some(true)));
        }
        assert!(gate.ready(s(20), Some(true)));
        gate.attempted = Some(s(20));
        for t in (22..80).step_by(2) {
            assert!(!gate.ready(s(t), Some(true)));
        }
        assert!(gate.ready(s(80), Some(true)));
        assert!(!gate.ready(s(82), None));
        for t in (84..104).step_by(2) {
            assert!(!gate.ready(s(t), Some(true)));
        }
        assert!(!gate.ready(s(110), Some(true)));
    }

    #[test]
    fn unsafe_arguments_rejected() {
        let p = |x: &str| [PathBuf::from(x)];
        assert!(validate_args(&[], &p("/home/workload")).is_err());
        assert!(validate_args(&[0], &p("/home/workload")).is_err());
        assert!(validate_args(&[1], &p("/home/workload")).is_err());
        assert!(validate_args(&[8, 8], &p("/home/workload")).is_err());
        assert!(validate_args(&[8], &p("app")).is_err());
        assert!(validate_args(&[8], &p("/home/*")).is_err());
        assert!(validate_args(&[8], &p("/home/workload")).is_ok());
    }

    fn status(pid: u32, uid: u32, parent: u32) -> String {
        format!("Pid:\t{pid}\nTgid:\t{pid}\nPPid:\t{parent}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nCapPrm: 0\nCapEff: 0\nCapAmb: 0\nVmRSS: 262144 kB\n")
    }

    #[test]
    fn root_foreign_or_elevated_credentials_rejected() {
        let good = status(55, 1000, 1);
        assert!(regular_user(&good, 1000).is_some());
        assert!(regular_user(&good, 1001).is_none());
        assert!(regular_user(&status(55, 0, 1), 0).is_none());
        assert!(regular_user(&good.replace("CapEff: 0", "CapEff: 1"), 1000).is_none());
        assert!(regular_user(&good.replace("1000\t1000", "1000\t0"), 1000).is_none());
    }

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn synthetic_proc_identity_preview_recycled_pid_and_only_one_signal_sink() {
        let uid = unsafe { libc::getuid() };
        if uid < 1000 {
            return;
        }
        let fixture = Fixture(
            std::env::temp_dir().join(format!("krc-pressure-fixture-{}", std::process::id())),
        );
        let procfs = &fixture.0;
        let exe = procfs.join("workload");
        let dir = procfs.join("49003");
        fs::create_dir(procfs).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(&exe, "not an executable fixture").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&exe, dir.join("exe")).unwrap();
        let write_stat = |start| {
            fs::write(
                dir.join("stat"),
                format!("49003 (name ) with paren) S 1 {}{start}\n", "0 ".repeat(17)),
            )
            .unwrap()
        };
        write_stat(50);
        fs::write(dir.join("status"), status(49003, uid, 1)).unwrap();
        fs::write(dir.join("oom_score"), "999\n").unwrap();
        fs::write(dir.join("oom_score_adj"), "0\n").unwrap();
        let allow = [exe];
        let initial = sample(procfs, 49003, uid, &allow).unwrap();
        assert_eq!(initial.identity.start, 50);
        assert_eq!(rank(&initial, 49003).0, 999);
        assert!(sample(procfs, 49003, uid, &[]).is_none());
        assert!(ancestors(procfs, 49003).unwrap().contains(&49003));
        assert!(ancestors(procfs, 49003).unwrap().contains(&1));
        let sent = Cell::new(0);
        let sink = || {
            sent.set(sent.get() + 1);
            Ok(())
        };
        assert_eq!(
            act(&initial.identity, Some(&initial.identity), false, sink).unwrap(),
            Action::Preview
        );
        assert_eq!(sent.get(), 0);
        write_stat(51);
        let recycled = sample(procfs, 49003, uid, &allow).unwrap();
        assert_eq!(
            act(&initial.identity, Some(&recycled.identity), true, sink).unwrap(),
            Action::Refused
        );
        assert_eq!(sent.get(), 0);
        assert_eq!(
            act(&initial.identity, Some(&initial.identity), true, sink).unwrap(),
            Action::TermSent
        );
        assert_eq!(sent.get(), 1);
        fs::write(dir.join("oom_score_adj"), "-1000\n").unwrap();
        assert!(sample(procfs, 49003, uid, &allow).is_none());
    }
}
