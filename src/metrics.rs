//! Zero-subprocess Linux memory, PSI, swap, tmpfs, and process telemetry.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Serialize;

const MAX_CMDLINE_BYTES: usize = 4096;

pub fn unescape_proc_path(raw: &str) -> String {
    raw.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MemoryInfo {
    pub total_mb: u64,
    pub available_mb: u64,
    pub used_mb: u64,
    pub buffers_mb: u64,
    pub cached_mb: u64,
    pub slab_reclaimable_mb: u64,
    pub dirty_mb: u64,
    pub swap_total_mb: u64,
    pub swap_free_mb: u64,
    pub swap_used_mb: u64,
}

impl MemoryInfo {
    pub fn used_percent(&self) -> f64 {
        if self.total_mb == 0 {
            0.0
        } else {
            ((self.used_mb as f64 / self.total_mb as f64) * 10000.0).round() / 100.0
        }
    }

    pub fn swap_used_percent(&self) -> f64 {
        if self.swap_total_mb == 0 {
            0.0
        } else {
            ((self.swap_used_mb as f64 / self.swap_total_mb as f64) * 10000.0).round() / 100.0
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PsiMemory {
    pub available: bool,
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub full_avg300: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwapDevice {
    pub path: String,
    pub swap_type: String,
    pub size_mb: u64,
    pub used_mb: u64,
    pub priority: i32,
}

impl SwapDevice {
    pub fn is_zram(&self) -> bool {
        Path::new(&self.path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase().contains("zram"))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TmpfsMount {
    pub mountpoint: String,
    pub total_mb: u64,
    pub used_mb: u64,
    pub free_mb: u64,
}

impl TmpfsMount {
    pub fn used_percent(&self) -> f64 {
        if self.total_mb == 0 {
            0.0
        } else {
            ((self.used_mb as f64 / self.total_mb as f64) * 10000.0).round() / 100.0
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: i32,
    pub uid: u32,
    pub comm: String,
    #[serde(skip_serializing)]
    pub cmdline: String,
    pub rss_mb: f64,
    #[serde(skip_serializing)]
    pub starttime: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SystemSnapshot {
    pub memory: MemoryInfo,
    pub psi: PsiMemory,
    pub swaps: Vec<SwapDevice>,
    pub tmpfs_mounts: Vec<TmpfsMount>,
    pub load_avg_1m: f64,
    pub load_avg_5m: f64,
    pub load_avg_15m: f64,
    pub cpu_count: usize,
}

impl SystemSnapshot {
    pub fn load_ratio(&self) -> f64 {
        let cpus = self.cpu_count.max(1) as f64;
        ((self.load_avg_1m / cpus) * 100.0).round() / 100.0
    }

    pub fn max_tmpfs_used_percent(&self, paths: Option<&[String]>) -> (f64, Option<String>) {
        let norm_targets: Option<Vec<String>> = paths.map(|ps| {
            ps.iter()
                .map(|p| {
                    fs::canonicalize(p)
                        .ok()
                        .and_then(|c| c.to_str().map(String::from))
                        .unwrap_or_else(|| p.clone())
                })
                .collect()
        });
        let mut best_pct = 0.0;
        let mut best_mp: Option<String> = None;
        for m in &self.tmpfs_mounts {
            if let Some(targets) = &norm_targets {
                let prefix = format!("{}/", m.mountpoint.trim_end_matches('/'));
                if !targets
                    .iter()
                    .any(|t| t == &m.mountpoint || t.starts_with(&prefix))
                {
                    continue;
                }
            }
            let pct = m.used_percent();
            if pct > best_pct {
                best_pct = pct;
                best_mp = Some(m.mountpoint.clone());
            }
        }
        (best_pct, best_mp)
    }
}

pub fn parse_meminfo(text: &str) -> MemoryInfo {
    let mut kb: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        if let Some((k, rest)) = line.split_once(':') {
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(v) = tok.parse::<u64>() {
                    kb.insert(k.trim(), v);
                }
            }
        }
    }
    let mb = |k: &str| kb.get(k).copied().unwrap_or(0) / 1024;
    let total = mb("MemTotal");
    let avail = if kb.contains_key("MemAvailable") {
        mb("MemAvailable")
    } else if kb.contains_key("MemFree") {
        mb("MemFree") + mb("Buffers") + mb("Cached")
    } else {
        0
    };
    let used = total.saturating_sub(avail);
    let stotal = mb("SwapTotal");
    let sfree = mb("SwapFree");
    let sused = stotal.saturating_sub(sfree);

    MemoryInfo {
        total_mb: total,
        available_mb: avail,
        used_mb: used,
        buffers_mb: mb("Buffers"),
        cached_mb: mb("Cached"),
        slab_reclaimable_mb: mb("SReclaimable"),
        dirty_mb: mb("Dirty"),
        swap_total_mb: stotal,
        swap_free_mb: sfree,
        swap_used_mb: sused,
    }
}

pub fn read_meminfo() -> MemoryInfo {
    fs::read_to_string("/proc/meminfo")
        .map(|s| parse_meminfo(&s))
        .unwrap_or_default()
}

pub fn parse_psi(text: &str) -> PsiMemory {
    let mut psi = PsiMemory::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(kind) = parts.next() else { continue };
        for tok in parts {
            let Some((k, v)) = tok.split_once('=') else {
                continue;
            };
            let Ok(num) = v.parse::<f64>() else { continue };
            match (kind, k) {
                ("some", "avg10") => {
                    psi.available = true;
                    psi.some_avg10 = num;
                }
                ("some", "avg60") => psi.some_avg60 = num,
                ("some", "avg300") => psi.some_avg300 = num,
                ("full", "avg10") => {
                    psi.available = true;
                    psi.full_avg10 = num;
                }
                ("full", "avg60") => psi.full_avg60 = num,
                ("full", "avg300") => psi.full_avg300 = num,
                _ => {}
            }
        }
    }
    psi
}

pub fn read_psi() -> PsiMemory {
    fs::read_to_string("/proc/pressure/memory")
        .map(|s| parse_psi(&s))
        .unwrap_or_default()
}

pub fn parse_swaps(text: &str) -> Vec<SwapDevice> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        let dev_path = unescape_proc_path(parts[0]);
        if !dev_path.starts_with('/') {
            continue;
        }
        if let (Ok(sz), Ok(used), Ok(prio)) = (
            parts[2].parse::<u64>(),
            parts[3].parse::<u64>(),
            parts[4].parse::<i32>(),
        ) {
            out.push(SwapDevice {
                path: dev_path,
                swap_type: parts[1].to_string(),
                size_mb: sz / 1024,
                used_mb: used / 1024,
                priority: prio,
            });
        }
    }
    out
}

pub fn read_swaps() -> Vec<SwapDevice> {
    fs::read_to_string("/proc/swaps")
        .map(|s| parse_swaps(&s))
        .unwrap_or_default()
}

pub fn parse_tmpfs_mountpoints(mounts_text: &str) -> Vec<String> {
    let mut pts = Vec::new();
    for line in mounts_text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[2] == "tmpfs" {
            pts.push(unescape_proc_path(parts[1]));
        }
    }
    pts
}

pub fn covering_tmpfs_mountpoint(target: &str, mounts_text: &str) -> Option<String> {
    let mut best_mp = String::new();
    let mut best_fs = String::new();
    for line in mounts_text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let mp = unescape_proc_path(parts[1]);
        let fstype = parts[2];
        let prefix = format!("{}/", mp.trim_end_matches('/'));
        if (target == mp || target.starts_with(&prefix)) && mp.len() >= best_mp.len() {
            best_mp = mp;
            best_fs = fstype.to_string();
        }
    }
    if best_fs == "tmpfs" {
        Some(best_mp)
    } else {
        None
    }
}

pub fn is_mount_tmpfs(target: &str, mounts_text: &str) -> bool {
    covering_tmpfs_mountpoint(target, mounts_text).is_some()
}

pub fn is_path_on_tmpfs(path: &str) -> bool {
    let target = fs::canonicalize(path)
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| path.to_string());
    fs::read_to_string("/proc/mounts")
        .map(|m| is_mount_tmpfs(&target, &m))
        .unwrap_or(false)
}

fn statvfs_mb(path: &str) -> Option<(u64, u64, u64)> {
    let cpath = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u128;
    let total = ((st.f_blocks as u128) * frsize / (1024 * 1024)) as u64;
    let free = ((st.f_bavail as u128) * frsize / (1024 * 1024)) as u64;
    let used = total.saturating_sub(free);
    Some((total, used, free))
}

pub fn read_tmpfs_mounts(filter_paths: Option<&[String]>) -> Vec<TmpfsMount> {
    let Ok(mounts_text) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mountpoints: Vec<String> = if let Some(fps) = filter_paths {
        fps.iter()
            .filter_map(|p| {
                let real = fs::canonicalize(p)
                    .ok()
                    .and_then(|c| c.to_str().map(String::from))
                    .unwrap_or_else(|| p.clone());
                covering_tmpfs_mountpoint(&real, &mounts_text)
            })
            .collect()
    } else {
        parse_tmpfs_mountpoints(&mounts_text)
    };

    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for mp in mountpoints {
        if !seen.insert(mp.clone()) {
            continue;
        }
        if let Some((total_mb, used_mb, free_mb)) = statvfs_mb(&mp) {
            results.push(TmpfsMount {
                mountpoint: mp,
                total_mb,
                used_mb,
                free_mb,
            });
        }
    }
    results
}

pub fn read_load_avg() -> (f64, f64, f64) {
    let mut loads = [0.0f64; 3];
    if unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } == 3 {
        (
            (loads[0] * 100.0).round() / 100.0,
            (loads[1] * 100.0).round() / 100.0,
            (loads[2] * 100.0).round() / 100.0,
        )
    } else {
        (0.0, 0.0, 0.0)
    }
}

pub fn read_proc_starttime(pid: i32) -> Option<u64> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = raw.rfind(')')?;
    let after = raw.get(rparen + 2..)?;
    after.split_whitespace().nth(19)?.parse::<u64>().ok()
}

pub fn read_proc_cmdline(pid: i32) -> String {
    let Ok(mut f) = File::open(format!("/proc/{pid}/cmdline")) else {
        return String::new();
    };
    let mut buf = vec![0u8; MAX_CMDLINE_BYTES];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    for b in &mut buf {
        if *b == 0 {
            *b = b' ';
        }
    }
    String::from_utf8_lossy(&buf).trim().to_string()
}

pub fn list_processes(
    min_rss_mb: f64,
    uid_filter: Option<u32>,
    require_starttime: bool,
) -> Vec<ProcessInfo> {
    let mut procs = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return procs;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_s) = name.to_str() else { continue };
        let Ok(pid) = name_s.parse::<i32>() else {
            continue;
        };
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };

        let mut comm = String::new();
        let mut rss_kb: u64 = 0;
        let mut uid: u32 = u32::MAX;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Name:") {
                comm = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(tok) = rest.split_whitespace().next() {
                    rss_kb = tok.parse().unwrap_or(0);
                }
            } else if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(tok) = rest.split_whitespace().next() {
                    uid = tok.parse().unwrap_or(u32::MAX);
                }
            }
        }

        let rss_mb = ((rss_kb as f64 / 1024.0) * 10.0).round() / 10.0;
        if rss_mb < min_rss_mb {
            continue;
        }
        if let Some(want_uid) = uid_filter {
            if uid != want_uid {
                continue;
            }
        }

        let starttime = read_proc_starttime(pid);
        if require_starttime && starttime.is_none() {
            continue;
        }
        let cmdline = {
            let c = read_proc_cmdline(pid);
            if c.is_empty() {
                comm.clone()
            } else {
                c
            }
        };

        procs.push(ProcessInfo {
            pid,
            uid,
            comm,
            cmdline,
            rss_mb,
            starttime,
        });
    }
    procs
}

pub fn top_memory_processes(limit: usize) -> Vec<ProcessInfo> {
    if limit == 0 {
        return Vec::new();
    }
    let mut procs = list_processes(0.0, None, false);
    procs.sort_by(|a, b| {
        b.rss_mb
            .partial_cmp(&a.rss_mb)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    procs.truncate(limit);
    procs
}

pub fn collect_snapshot(
    tmpfs_paths: Option<&[String]>,
    include_swaps: bool,
    include_tmpfs: bool,
    include_psi: bool,
    include_load: bool,
) -> SystemSnapshot {
    let (l1, l5, l15) = if include_load {
        read_load_avg()
    } else {
        (0.0, 0.0, 0.0)
    };
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    SystemSnapshot {
        memory: read_meminfo(),
        psi: if include_psi {
            read_psi()
        } else {
            PsiMemory::default()
        },
        swaps: if include_swaps {
            read_swaps()
        } else {
            Vec::new()
        },
        tmpfs_mounts: if include_tmpfs {
            read_tmpfs_mounts(tmpfs_paths)
        } else {
            Vec::new()
        },
        load_avg_1m: l1,
        load_avg_5m: l5,
        load_avg_15m: l15,
        cpu_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_zero_available_does_not_inflate() {
        let text = "MemTotal: 16384000 kB\nMemFree: 512000 kB\nMemAvailable: 512 kB\nCached: 4096000 kB\n";
        let m = parse_meminfo(text);
        assert_eq!(m.total_mb, 16000);
        assert_eq!(m.available_mb, 0);
    }

    #[test]
    fn shadowed_mountpoint_handled() {
        let mounts = "tmpfs /tmp tmpfs rw 0 0\n/dev/sda1 /tmp ext4 rw 0 0\n";
        assert!(!is_mount_tmpfs("/tmp", mounts));
        let rev = "/dev/sda1 /tmp ext4 rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n";
        assert!(is_mount_tmpfs("/tmp", rev));
    }

    #[test]
    fn swaps_zram_detection() {
        let text = "Filename\tType\tSize\tUsed\tPriority\n/dev/zram0 partition 4194304 524288 100\n/swapfile file 8388608 1048576 -2\n";
        let s = parse_swaps(text);
        assert_eq!(s.len(), 2);
        assert!(s[0].is_zram());
        assert_eq!(s[0].used_mb, 512);
        assert!(!s[1].is_zram());
        assert_eq!(s[1].priority, -2);
    }
}
