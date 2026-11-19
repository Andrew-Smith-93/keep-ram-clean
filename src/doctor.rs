//! System memory health audit & Linux VM tuning advisor (`keep-ram-clean doctor`).

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::config::Config;
use crate::metrics::{collect_snapshot, is_path_on_tmpfs, top_memory_processes};

#[derive(Debug, Clone, Serialize)]
pub struct AuditFinding {
    pub check: String,
    pub status: String, // "ok", "info", "warn"
    pub detail: String,
    pub recommendation: String,
}

impl AuditFinding {
    fn new(check: impl Into<String>, status: &str, detail: impl Into<String>, rec: &str) -> Self {
        Self {
            check: check.into(),
            status: status.to_string(),
            detail: detail.into(),
            recommendation: rec.to_string(),
        }
    }
}

fn read_sysctl(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn run_doctor(config: Option<&Config>) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let default_paths = vec!["/tmp".to_string()];
    let tmpfs_paths = config
        .map(|c| c.tmpfs.paths.as_slice())
        .unwrap_or(&default_paths);
    let snap = collect_snapshot(Some(tmpfs_paths), true, true, true, true);

    // 1. PSI support
    if snap.psi.available {
        findings.push(AuditFinding::new(
            "Pressure Stall Information (PSI)",
            "ok",
            format!(
                "Enabled (memory some avg10={:.2}, full avg10={:.2})",
                snap.psi.some_avg10, snap.psi.full_avg10
            ),
            "",
        ));
    } else {
        findings.push(AuditFinding::new(
            "Pressure Stall Information (PSI)",
            "warn",
            "/proc/pressure/memory not available",
            "Boot with kernel parameter 'psi=1' on older kernels for accurate contention-based triggers.",
        ));
    }

    // 2. cgroup v2 root memory.reclaim (Linux 6.1+)
    if Path::new("/sys/fs/cgroup/memory.reclaim").exists() {
        findings.push(AuditFinding::new(
            "cgroup v2 memory.reclaim",
            "ok",
            "Supported (proactive cold-page reclamation available without cold-cache shock)",
            "",
        ));
    } else {
        findings.push(AuditFinding::new(
            "cgroup v2 memory.reclaim",
            "info",
            "Not available (root /sys/fs/cgroup/memory.reclaim requires Linux 6.1+ with unified cgroup v2)",
            "Daemon will fall back to drop_caches_level when running as root.",
        ));
    }

    // 3. MGLRU
    if let Some(mglru) = read_sysctl("/sys/kernel/mm/lru_gen/enabled") {
        let enabled = u64::from_str_radix(mglru.trim_start_matches("0x"), if mglru.starts_with("0x") { 16 } else { 10 })
            .map(|v| v != 0)
            .unwrap_or(mglru != "0" && mglru != "0x0000");
        findings.push(AuditFinding::new(
            "Multi-Gen LRU (MGLRU)",
            if enabled { "ok" } else { "info" },
            format!("Kernel value: {mglru}"),
            if enabled {
                ""
            } else {
                "Enabling MGLRU (CONFIG_LRU_GEN) significantly improves desktop responsiveness under memory pressure."
            },
        ));
    }

    // 4. Swap / zram / zswap
    let has_zram = snap.swaps.iter().any(|s| s.is_zram());
    let has_disk_swap = snap.swaps.iter().any(|s| !s.is_zram());
    let zswap_enabled = read_sysctl("/sys/module/zswap/parameters/enabled").as_deref() == Some("Y");

    if has_zram {
        let zram_mb: u64 = snap
            .swaps
            .iter()
            .filter(|s| s.is_zram())
            .map(|s| s.size_mb)
            .sum();
        findings.push(AuditFinding::new(
            "Compressed Swap (zram)",
            "ok",
            format!("Active ({zram_mb} MiB total zram); skip_zram=true will avoid wasteful zram flushing."),
            "",
        ));
    } else if zswap_enabled {
        findings.push(AuditFinding::new(
            "Compressed Swap (zswap)",
            "ok",
            "zswap is enabled in kernel.",
            "",
        ));
    } else if snap.memory.swap_total_mb == 0 {
        findings.push(AuditFinding::new(
            "Swap Configuration",
            "info",
            "No swap configured.",
            "Consider enabling zram (e.g. zram-generator) to cushion RAM spikes without disk thrashing.",
        ));
    } else {
        findings.push(AuditFinding::new(
            "Swap Configuration",
            "info",
            format!(
                "Disk/file swap active ({} MiB total, no zram/zswap detected).",
                snap.memory.swap_total_mb
            ),
            "Consider zram or zswap for faster, lower-latency swapping under load.",
        ));
    }

    // 5. vm.swappiness
    if let Some(raw) = read_sysctl("/proc/sys/vm/swappiness") {
        if let Ok(sw) = raw.parse::<u32>() {
            if snap.memory.swap_total_mb == 0 {
                findings.push(AuditFinding::new(
                    "vm.swappiness",
                    "ok",
                    format!("Currently {sw} (no swap active)"),
                    "",
                ));
            } else if has_zram && !has_disk_swap && sw < 60 {
                findings.push(AuditFinding::new(
                    "vm.swappiness",
                    "info",
                    format!("Currently {sw} (zram-only swap active)"),
                    "With zram-only swap, higher swappiness (100-180) is often recommended because zram is RAM-speed.",
                ));
            } else if has_zram && has_disk_swap {
                findings.push(AuditFinding::new(
                    "vm.swappiness",
                    "ok",
                    format!("Currently {sw} (mixed zram + disk swap active)"),
                    "",
                ));
            } else if !has_zram && !zswap_enabled && sw > 60 {
                findings.push(AuditFinding::new(
                    "vm.swappiness",
                    "warn",
                    format!("Currently {sw} (disk swap only)"),
                    "Consider lowering vm.swappiness to 10-30 for desktop workloads on disk swap.",
                ));
            } else {
                findings.push(AuditFinding::new(
                    "vm.swappiness",
                    "ok",
                    format!("Currently {sw}"),
                    "",
                ));
            }
        }
    }

    // 6. vm.vfs_cache_pressure
    if let Some(raw) = read_sysctl("/proc/sys/vm/vfs_cache_pressure") {
        match raw.parse::<u32>() {
            Ok(0) => findings.push(AuditFinding::new(
                "vm.vfs_cache_pressure",
                "warn",
                "Currently 0 (kernel will never reclaim dentry/inode slab caches)",
                "Set vm.vfs_cache_pressure to 50-100 to prevent unbounded slab growth.",
            )),
            Ok(v) => findings.push(AuditFinding::new(
                "vm.vfs_cache_pressure",
                "ok",
                format!("Currently {v}"),
                "",
            )),
            Err(_) => findings.push(AuditFinding::new(
                "vm.vfs_cache_pressure",
                "info",
                format!("Currently {raw}"),
                "",
            )),
        }
    }

    // 7. Monitored tmpfs paths
    for p in tmpfs_paths {
        if is_path_on_tmpfs(p) {
            findings.push(AuditFinding::new(
                format!("{p} Filesystem"),
                "ok",
                format!("Mounted on tmpfs (RAM-backed); stale files in {p} directly consume RAM and will be cleaned."),
                "",
            ));
        } else {
            findings.push(AuditFinding::new(
                format!("{p} Filesystem"),
                "info",
                format!("{p} is on disk (not tmpfs)."),
                &format!("tmpfs cleanup will safely skip {p} while require_tmpfs=true."),
            ));
        }
    }

    // 8. Top consumers
    let top = top_memory_processes(3);
    if !top.is_empty() {
        let summary = top
            .iter()
            .map(|p| format!("{} ({:.0} MiB)", p.comm, p.rss_mb))
            .collect::<Vec<_>>()
            .join(", ");
        findings.push(AuditFinding::new("Top Memory Consumers", "info", summary, ""));
    }

    findings
}
