//! Read-only formatter for information collected by the original Watch loop.
use crate::daemon::DaemonState;
use crate::metrics::{ProcessInfo, SystemSnapshot};
use crate::ui::{fmt_mib, fmt_mib_f64, gauge_bar, mini_bar, psi_badge, Palette};
use crate::watch_layout::{escape_controls, Layout};
use crate::Config;

pub(crate) struct Report<'a> {
    pub(crate) config: &'a Config,
    pub(crate) snapshot: &'a SystemSnapshot,
    pub(crate) processes: &'a [ProcessInfo],
    pub(crate) daemon: Option<&'a DaemonState>,
    pub(crate) toast: Option<&'a str>,
    pub(crate) interval: u64,
}

impl Report<'_> {
    pub(crate) fn render(&self, width: usize, p: &Palette) -> Vec<String> {
        let frame_width = width.min(126);
        let width = frame_width - 4;
        let layout = Layout::board(p);
        // Keep a common right-column anchor on the wide canvas.
        let left_lane = if width >= 112 { 61 } else { (width - 1) / 2 };
        let snap = self.snapshot;
        let mem = &snap.memory;
        let t = &self.config.triggers;
        let active = self.daemon.filter(|d| d.alive);
        let (badge, color) = match active {
            Some(d) if d.cleaning_now => ("⚡ AUTO-CLEANING", p.yellow),
            Some(_) => ("● DAEMON RUNNING", p.green),
            None => ("○ DAEMON STOPPED", p.yellow),
        };
        let mut output = layout.text(
            &format!(
                "{}keep-ram-clean v{}{}  │  WATCH  │  {color}{badge}{}",
                p.bold,
                crate::daemon::VERSION,
                p.nc,
                p.nc
            ),
            width,
        );

        let utilization = |value, warning, critical| {
            let color = p.severity_color(value, warning, critical);
            format!(
                "{}{color} {value:.1}%{}",
                gauge_bar(value, 7, warning, critical, p),
                p.nc
            )
        };
        let ram = if t.ram_used_percent > 0.0 {
            t.ram_used_percent
        } else {
            80.0
        };
        let swap = if t.swap_used_percent > 0.0 {
            t.swap_used_percent
        } else {
            50.0
        };
        let tmpfs = if t.tmpfs_used_percent > 0.0 {
            t.tmpfs_used_percent
        } else {
            70.0
        };
        let mut resources = vec![
            vec![
                "RAM".into(),
                format!("{} / {}", fmt_mib(mem.used_mb), fmt_mib(mem.total_mb)),
                utilization(mem.used_percent(), (ram - 15.0).max(50.0), ram),
            ],
            vec![
                "Swap".into(),
                format!(
                    "{} / {}",
                    fmt_mib(mem.swap_used_mb),
                    fmt_mib(mem.swap_total_mb)
                ),
                utilization(mem.swap_used_percent(), (swap - 20.0).max(20.0), swap),
            ],
        ];
        for mount in &snap.tmpfs_mounts {
            resources.push(vec![
                escape_controls(&mount.mountpoint),
                format!("{} / {}", fmt_mib(mount.used_mb), fmt_mib(mount.total_mb)),
                utilization(mount.used_percent(), (tmpfs - 20.0).max(40.0), tmpfs),
            ]);
        }
        let memory = [
            vec![
                "Available / Cache".into(),
                format!("{} / {}", fmt_mib(mem.available_mb), fmt_mib(mem.cached_mb)),
            ],
            vec![
                "Reclaimable slab / Dirty".into(),
                format!(
                    "{} / {}",
                    fmt_mib(mem.slab_reclaimable_mb),
                    fmt_mib(mem.dirty_mb)
                ),
            ],
        ];
        let psi = &snap.psi;
        let mut pressure = Vec::new();
        if psi.available {
            pressure.push(vec![
                "PSI status".into(),
                psi_badge(
                    psi.some_avg10,
                    psi.full_avg10,
                    t.psi_some_avg10,
                    t.psi_full_avg10,
                    p,
                ),
            ]);
            pressure.push(vec![
                "PSI some 10/60s".into(),
                format!("{:.2}% / {:.2}%", psi.some_avg10, psi.some_avg60),
            ]);
            pressure.push(vec![
                "PSI full 10/60s".into(),
                format!("{:.2}% / {:.2}%", psi.full_avg10, psi.full_avg60),
            ]);
            pressure.push(vec![
                "Load (1m) / CPUs".into(),
                format!("{:.2} / {}", snap.load_avg_1m, snap.cpu_count),
            ]);
        } else {
            pressure.push(vec!["PSI status".into(), "Unavailable".into()]);
            pressure.push(vec![
                "Load (1m / 5m / 15m)".into(),
                format!(
                    "{:.2} / {:.2} / {:.2}",
                    snap.load_avg_1m, snap.load_avg_5m, snap.load_avg_15m
                ),
            ]);
            pressure.push(vec![
                "CPUs / Load per CPU".into(),
                format!("{} / {:.2}", snap.cpu_count, snap.load_ratio()),
            ]);
        }
        let consumers: Vec<_> = self
            .processes
            .iter()
            .map(|proc| {
                let frac = proc.rss_mb / mem.total_mb.max(1) as f64;
                vec![
                    proc.pid.to_string(),
                    escape_controls(&proc.comm),
                    fmt_mib_f64(proc.rss_mb),
                    format!("{} {:.1}%", mini_bar(frac, 5, p), frac * 100.0),
                ]
            })
            .collect();
        let left = |w| {
            layout.table(
                "MEMORY · SWAP · TMPFS",
                &["RESOURCE", "USED / TOTAL", "USAGE"],
                &resources,
                w,
            )
        };
        let detail_table = |w| layout.table("RAM DETAILS", &["METRIC", "AMOUNTS"], &memory, w);
        let pressure_table =
            |w| layout.table("PRESSURE & LOAD", &["METRIC", "READING"], &pressure, w);
        let consumer_table = |w| {
            if consumers.is_empty() {
                layout.table(
                    "TOP CONSUMERS",
                    &["STATE", "VALUE"],
                    &[vec!["Listed".into(), "0 (disabled or unavailable)".into()]],
                    w,
                )
            } else {
                layout.table(
                    &format!("TOP {} CONSUMERS", consumers.len()),
                    &["PID", "PROCESS", "RSS", "RAM SHARE"],
                    &consumers,
                    w,
                )
            }
        };
        let mut live = Vec::new();
        if width >= 96 {
            live.extend(layout.pair_at(width, left_lane, left, pressure_table));
        } else {
            live.extend(left(width));
            live.extend(pressure_table(width));
        }

        // Each band starts on one baseline. Nearby headings don't jump up or down
        // based on how many rows are in a neighbor in the previous band.
        let mut resource_band = Vec::new();
        let storage = |w| {
            let mut out = detail_table(w);
            let swaps = layout.swap_devices(&snap.swaps, w);
            if !swaps.is_empty() {
                out.push(String::new());
                out.extend(swaps);
            }
            out
        };
        if width >= 112 {
            resource_band.extend(layout.pair_at(width, left_lane, storage, consumer_table));
        } else {
            resource_band.extend(storage(width));
            resource_band.push(String::new());
            resource_band.extend(consumer_table(width));
        }

        let when = if t.interval_minutes > 0 {
            format!("Every {}m", t.interval_minutes)
        } else if !t.schedules.is_empty() {
            escape_controls(&t.schedules.join(","))
        } else {
            "Reactive".into()
        };
        let triggers = [
            vec![
                "RAM / Available / Swap".into(),
                format!(
                    "≥ {:.0}% / ≤ {} / ≥ {:.0}%",
                    t.ram_used_percent,
                    fmt_mib(t.min_available_mb),
                    t.swap_used_percent
                ),
            ],
            vec![
                "PSI some / full".into(),
                format!("≥ {:.0} / {:.0}%", t.psi_some_avg10, t.psi_full_avg10),
            ],
            vec![
                "Cooldown / Mode / Alerts".into(),
                format!(
                    "{}s / {when} / {}",
                    t.cooldown_seconds,
                    if self.config.notifications.enabled {
                        "ON"
                    } else {
                        "OFF"
                    }
                ),
            ],
        ];
        let mut status = Vec::new();
        if let Some(d) = active {
            if d.cleaning_now {
                status.push(vec!["State".into(), "Auto-clean in progress".into()]);
                status.push(vec![
                    "Reason".into(),
                    escape_controls(d.cleaning_reason.as_deref().unwrap_or("Trigger fired")),
                ]);
            } else {
                status.push(vec!["State".into(), "Running".into()]);
                status.push(vec![
                    "PID / since".into(),
                    format!("{} / {}", d.pid, crate::fmt_ts(d.started_at)),
                ]);
                status.push(vec![
                    "Cleanups / total freed".into(),
                    format!(
                        "{} / {:+} MiB RAM / {:+} MiB swap",
                        d.cleanups_run, d.total_ram_freed_mb, d.total_swap_freed_mb
                    ),
                ]);
            }
        } else {
            status.push(vec![
                "State".into(),
                "Daemon not running. Start the daemon or systemd service.".into(),
            ]);
        }
        let mut automatic = Vec::new();
        let daemon = |w| layout.table("DAEMON STATE", &["FIELD", "VALUE"], &status, w);
        let policy = |w| layout.table("TRIGGERS", &["SETTING", "VALUE"], &triggers, w);
        if width >= 112 {
            automatic.extend(layout.pair_at(width, left_lane, daemon, policy));
        } else {
            automatic.extend(daemon(width));
            automatic.extend(policy(width));
        }
        if let Some(d) = active {
            let recent: Vec<_> = if d.recent_cleanups.is_empty() {
                d.last_cleanup.iter().collect()
            } else {
                d.recent_cleanups.iter().take(3).collect()
            };
            if recent.is_empty() {
                automatic.extend(layout.table(
                    "RECENT CLEANUPS",
                    &["FIELD", "VALUE"],
                    &[vec![
                        "History".into(),
                        "No auto-cleanups triggered yet".into(),
                    ]],
                    width,
                ));
            } else {
                let rows = recent
                    .iter()
                    .map(|e| {
                        let steps = e
                            .steps
                            .iter()
                            .map(|s| {
                                let (icon, color) = match s.status.as_str() {
                                    "ok" => ("✔", p.green),
                                    "error" => ("✖", p.red),
                                    _ => ("–", p.dim),
                                };
                                format!("{color}{icon}{}{}", escape_controls(&s.name), p.nc)
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        vec![
                            format!(
                                "{} {:.2}s | RAM {:+} / Swap {:+} MiB",
                                crate::fmt_hms(e.finished_at),
                                e.duration_seconds,
                                e.ram_freed_mb,
                                -i128::from(e.swap_freed_mb)
                            ),
                            escape_controls(&e.reasons.join("; ")),
                            steps,
                        ]
                    })
                    .collect::<Vec<_>>();
                automatic.extend(layout.table(
                    "RECENT CLEANUPS",
                    &["WHEN / CHANGE", "REASON", "STEPS"],
                    &rows,
                    width,
                ));
            }
        }
        output.extend(layout.board_groups(
            frame_width,
            [
                ("01  LIVE READINGS", live),
                ("02  RESOURCE DETAILS", resource_band),
                ("03  AUTO-CLEAN", automatic),
            ],
        ));
        if let Some(msg) = self.toast {
            output.extend(layout.text(&escape_controls(msg), frame_width));
        }
        output.extend(layout.text(&format!("[c] Clean now  [+/-] RAM ({:.0}%)  [n] Alerts  [r] Reload  [s] Settings  [q] Quit  {}s refresh", t.ram_used_percent, self.interval), frame_width));
        output
    }
}

/// Limit only colored interactive paints; the full report is reachable with j/k.
pub(crate) fn viewport(
    mut rows: Vec<String>,
    width: usize,
    height: usize,
    offset: &mut usize,
) -> (Vec<String>, bool) {
    if height < 6 || rows.len() < height {
        *offset = 0;
        return (rows, false);
    }
    let room = height - 2;
    // Pad the end so repeated scroll keys stop at a stable last page.
    *offset = (*offset).min(rows.len().saturating_sub(room));
    let remaining = rows.len() - *offset;
    let count = room.min(remaining);
    let footer = if width >= 37 {
        format!(
            "j/k Scroll | rows {}–{} of {}",
            *offset + 1,
            *offset + count,
            rows.len()
        )
    } else {
        "j/k Scroll".into()
    };
    let mut visible = vec!["KEEP-RAM-CLEAN WATCH".chars().take(width).collect()];
    visible.extend(rows.drain(*offset..*offset + count));
    visible.push(footer);
    (visible, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{LastCleanupState, LastCleanupStep};
    use crate::metrics::{MemoryInfo, PsiMemory, SwapDevice, TmpfsMount};

    #[test]
    fn scrolling_reaches_first_and_last_rows_without_dropping_them() {
        let rows = (0..40).map(|i| format!("row {i}")).collect::<Vec<_>>();
        let mut offset = 0;
        let (first, clipped) = viewport(rows.clone(), 80, 10, &mut offset);
        assert!(clipped && first.contains(&"row 0".to_owned()));
        assert_eq!(first.len(), 10);
        offset = usize::MAX;
        let (last, _) = viewport(rows, 80, 10, &mut offset);
        assert!(last.contains(&"row 39".to_owned()));
        assert_eq!(last.len(), 10);
    }

    #[test]
    fn original_report_fields_survive_tables_on_fabricated_snapshots() {
        let p = Palette {
            color: false,
            bold: "",
            dim: "",
            green: "",
            cyan: "",
            yellow: "",
            red: "",
            nc: "",
        };
        let snapshot = SystemSnapshot {
            memory: MemoryInfo {
                total_mb: 8192,
                available_mb: 4096,
                used_mb: 4096,
                cached_mb: 120,
                slab_reclaimable_mb: 64,
                dirty_mb: 4,
                swap_total_mb: 2048,
                swap_used_mb: 96,
                ..MemoryInfo::default()
            },
            psi: PsiMemory {
                available: true,
                some_avg10: 1.25,
                some_avg60: 2.50,
                full_avg10: 0.5,
                full_avg60: 0.6,
                ..PsiMemory::default()
            },
            load_avg_1m: 3.77,
            load_avg_5m: 2.25,
            load_avg_15m: 2.0,
            cpu_count: 4,
            tmpfs_mounts: vec![TmpfsMount {
                mountpoint: format!("/{} TMP-END", "x".repeat(49)),
                total_mb: 256,
                used_mb: 128,
                free_mb: 128,
            }],
            swaps: vec![SwapDevice {
                path: "/dev/zram0".into(),
                swap_type: "partition".into(),
                size_mb: 2048,
                used_mb: 96,
                priority: 100,
            }],
        };
        let processes = [ProcessInfo {
            pid: 733,
            uid: 1000,
            comm: format!("{} PROC-END", "worker".repeat(9)),
            cmdline: String::new(),
            rss_mb: 196.0,
            starttime: None,
        }];
        let d = DaemonState {
            version: "2.3.0".into(),
            pid: 9999,
            starttime: Some(10),
            status: "running".into(),
            uid: 1000,
            dry_run: false,
            config_path: None,
            started_at: 1_700_000_000.0,
            updated_at: 1_700_000_001.0,
            cleanups_run: 5,
            total_ram_freed_mb: 87,
            total_swap_freed_mb: 40,
            cleaning_now: true,
            cleaning_reason: Some(format!("{} CURRENT-END", "threshold ".repeat(18))),
            last_cleanup: None,
            recent_cleanups: (0..4)
                .map(|i| LastCleanupState {
                    finished_at: 1_700_000_000.0,
                    duration_seconds: 0.91,
                    ram_freed_mb: 21,
                    swap_freed_mb: 5,
                    reasons: vec![format!("{} WHY-{i}-END", "threshold ".repeat(16))],
                    steps: vec![LastCleanupStep {
                        name: format!("step-{i}"),
                        status: "ok".into(),
                        summary: String::new(),
                    }],
                })
                .collect(),
            alive: true,
            state_file: String::new(),
        };
        let cfg = Config::default();
        let report = Report {
            config: &cfg,
            snapshot: &snapshot,
            processes: &processes,
            daemon: Some(&d),
            toast: None,
            interval: 2,
        };
        let mut normal_snapshot = snapshot.clone();
        normal_snapshot.tmpfs_mounts[0].mountpoint = "/run/user/1000".into();
        let mut normal_processes = processes.clone();
        normal_processes[0].comm = "build-worker".into();
        let mut normal_daemon = d.clone();
        normal_daemon.cleaning_now = false;
        normal_daemon.cleaning_reason = None;
        normal_daemon.recent_cleanups.truncate(1);
        normal_daemon.recent_cleanups[0].reasons = vec!["RAM above threshold".into()];
        let normal = Report {
            config: &cfg,
            snapshot: &normal_snapshot,
            processes: &normal_processes,
            daemon: Some(&normal_daemon),
            toast: None,
            interval: 2,
        };
        if std::env::var_os("KRC_TEST_WATCH_PTY").is_some() {
            let width = crate::ui::watch_terminal_width();
            println!("PTY-WATCH-{width}-BEGIN");
            for line in normal.render(width, &p) {
                println!("{line}");
            }
            println!("PTY-WATCH-{width}-END");
        }
        for width in [80, 120, 200] {
            let rows = normal.render(width, &p);
            let span = |left: &str, right: &str| {
                let row = rows
                    .iter()
                    .find(|r| r.contains(left) && r.contains(right))
                    .unwrap();
                let after_left = row.split_once(left).unwrap().1;
                after_left.split_once(right).unwrap().0.chars().count()
            };
            assert!(span("/dev/zram0", "partition") <= 8);
            assert!(span(" RAM", "4.0 GiB") <= 22);
        }
        for width in [35, 80, 120, 200, 240] {
            let rows = normal.render(width, &p);
            let row_of = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();
            let live = row_of("01  LIVE READINGS");
            let detail = row_of("02  RESOURCE DETAILS");
            let cleanup = row_of("03  AUTO-CLEAN");
            assert!(live < row_of("PRESSURE & LOAD") && row_of("PRESSURE & LOAD") < detail);
            assert!(detail < row_of("SWAP DEVICES") && row_of("SWAP DEVICES") < cleanup);
            assert!(cleanup < row_of("TRIGGERS") && row_of("TRIGGERS") < row_of("RECENT CLEANUPS"));
            if width >= 120 {
                let anchors = ["PRESSURE & LOAD", "TOP 1 CONSUMERS", "TRIGGERS"].map(|label| {
                    let line = &rows[row_of(label)];
                    line[..line.find(label).unwrap()].chars().count()
                });
                assert_eq!(anchors, [anchors[0]; 3]);
            }
        }
        let colored = Palette {
            color: true,
            bold: "\x1b[1m",
            dim: "\x1b[2m",
            green: "\x1b[0;32m",
            cyan: "\x1b[0;36m",
            yellow: "\x1b[1;33m",
            red: "\x1b[0;31m",
            nc: "\x1b[0m",
        };
        for (width, previous_normal, previous_dense) in [(80, 60, 81), (120, 45, 63)] {
            let normal_plain = normal.render(width, &p);
            let dense_plain = report.render(width, &p);
            println!(
                "density-benchmark width={width} normal={} dense={}",
                normal_plain.len(),
                dense_plain.len()
            );
            assert!(normal_plain.len() < previous_normal);
            assert!(dense_plain.len() < previous_dense);
            for (fixture, plain) in [(&normal, normal_plain), (&report, dense_plain)] {
                let ansi = fixture.render(width, &colored);
                assert_eq!(ansi.len(), plain.len());
                let sans_ansi = ansi
                    .into_iter()
                    .map(|line| {
                        [
                            colored.bold,
                            colored.dim,
                            colored.green,
                            colored.cyan,
                            colored.yellow,
                            colored.red,
                            colored.nc,
                        ]
                        .into_iter()
                        .fold(line, |line, code| line.replace(code, ""))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(sans_ansi, plain);
            }
        }
        for width in [35, 80, 120] {
            let lines = report.render(width, &p);
            let text = lines.join("\n");
            let wrapped = text.split_whitespace().collect::<Vec<_>>().join(" ");
            for item in [
                "TMP-END",
                "PROC-END",
                "CURRENT-END",
                "WHY-0-END",
                "WHY-2-END",
                "step-2",
                "3.77",
                "196 MiB",
                "120 MiB",
                "1.25%",
                "2.50%",
                "96 MiB",
                "0.91s",
                "[s] Settings",
                "[c] Clean now",
            ] {
                assert!(wrapped.contains(item), "{width} missing {item}: {text}");
            }
            assert!(!text.contains('…') && !text.contains("WHY-3-END"));
        }
    }
}
