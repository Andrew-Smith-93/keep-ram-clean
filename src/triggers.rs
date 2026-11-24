//! Trigger evaluation engine: reactive thresholds, PSI, hysteresis, cooldown, and cron schedules.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::metrics::SystemSnapshot;

#[derive(Debug, Clone, Copy)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,   // 1..=12
    pub day: u32,     // 1..=31
    pub hour: u32,    // 0..=23
    pub minute: u32,  // 0..=59
    pub weekday: u32, // 0=Sunday..=6=Saturday (cron convention)
}

impl LocalTime {
    pub fn now() -> Self {
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            Self {
                year: tm.tm_year + 1900,
                month: (tm.tm_mon + 1) as u32,
                day: tm.tm_mday as u32,
                hour: tm.tm_hour as u32,
                minute: tm.tm_min as u32,
                weekday: tm.tm_wday as u32,
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct TriggerDecision {
    pub should_run: bool,
    pub reasons: Vec<String>,
    pub critical: bool,
    pub deferred: bool,
    pub defer_reason: String,
}

fn match_cron_field(expr: &str, value: u32, min_val: u32, max_val: u32) -> bool {
    for part in expr.split(',') {
        let part = part.trim();
        let (base, step, has_step) = if let Some((b, s)) = part.split_once('/') {
            (b, s.parse::<u32>().unwrap_or(1).max(1), true)
        } else {
            (part, 1, false)
        };

        if base == "*" {
            if value >= min_val && (value - min_val) % step == 0 {
                return true;
            }
        } else if let Some((lo_s, hi_s)) = base.split_once('-') {
            if let (Ok(lo), Ok(hi)) = (lo_s.parse::<u32>(), hi_s.parse::<u32>()) {
                let mut candidates = vec![value];
                if max_val == 6 && value == 0 && hi == 7 {
                    candidates.push(7);
                }
                for cand in candidates {
                    if cand >= lo && cand <= hi && (cand - lo) % step == 0 {
                        return true;
                    }
                }
            }
        } else if let Ok(mut num) = base.parse::<u32>() {
            if max_val == 6 && num == 7 {
                num = 0;
            }
            if has_step {
                if value >= num && (value - num) % step == 0 {
                    return true;
                }
            } else if value == num {
                return true;
            }
        }
    }
    false
}

pub fn schedule_matches(expr: &str, now: LocalTime) -> bool {
    let expr = expr.trim();
    if let Some((hh_s, mm_s)) = expr.split_once(':') {
        if !expr.contains(' ') {
            if let (Ok(hh), Ok(mm)) = (hh_s.parse::<u32>(), mm_s.parse::<u32>()) {
                return now.hour == hh && now.minute == mm;
            }
        }
        return false;
    }
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    match_cron_field(parts[0], now.minute, 0, 59)
        && match_cron_field(parts[1], now.hour, 0, 23)
        && match_cron_field(parts[2], now.day, 1, 31)
        && match_cron_field(parts[3], now.month, 1, 12)
        && match_cron_field(parts[4], now.weekday, 0, 6)
}

type SchedKey = (String, i32, u32, u32, u32, u32);

pub struct TriggerEngine {
    pub config: Config,
    last_cleanup: Option<Instant>,
    last_interval: Instant,
    fired_schedule_minutes: HashSet<SchedKey>,
    latched_triggers: HashSet<&'static str>,
}

impl TriggerEngine {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            last_cleanup: None,
            last_interval: Instant::now(),
            fired_schedule_minutes: HashSet::new(),
            latched_triggers: HashSet::new(),
        }
    }

    pub fn update_config(&mut self, config: Config) {
        self.config = config;
        self.latched_triggers.clear();
    }

    pub fn record_cleanup(&mut self, now: Instant) {
        self.last_cleanup = Some(now);
    }

    pub fn evaluate(
        &mut self,
        snapshot: &SystemSnapshot,
        now_local: LocalTime,
        now_mono: Instant,
    ) -> TriggerDecision {
        let t = &self.config.triggers;
        let mem = &snapshot.memory;
        let psi = &snapshot.psi;

        let mut reasons: Vec<String> = Vec::new();
        let mut critical = false;
        let mut pending_latches: Vec<&'static str> = Vec::new();
        let mut pending_interval = false;
        let mut pending_scheds: Vec<SchedKey> = Vec::new();

        // Hysteresis re-arm checks
        let hyst = t.hysteresis_percent.max(0.0);
        let ram_pct = mem.used_percent();
        let swap_pct = mem.swap_used_percent();
        if self.latched_triggers.contains("ram_pct") && ram_pct <= (t.ram_used_percent - hyst) {
            self.latched_triggers.remove("ram_pct");
        }
        if self.latched_triggers.contains("swap_pct") && swap_pct <= (t.swap_used_percent - hyst) {
            self.latched_triggers.remove("swap_pct");
        }
        if self.latched_triggers.contains("tmpfs_pct") {
            let (max_tmp, _) = snapshot.max_tmpfs_used_percent(Some(&self.config.tmpfs.paths));
            if max_tmp <= (t.tmpfs_used_percent - hyst) {
                self.latched_triggers.remove("tmpfs_pct");
            }
        }

        // Cooldown checks
        let (in_cooldown, in_emergency_cooldown) = match self.last_cleanup {
            Some(prev) => {
                let elapsed = now_mono.saturating_duration_since(prev);
                let cd = Duration::from_secs(t.cooldown_seconds);
                let emerg_cd = Duration::from_secs(t.cooldown_seconds.min(60));
                (elapsed < cd, elapsed < emerg_cd)
            }
            None => (false, false),
        };

        // 1. Reactive Memory Thresholds
        if t.ram_used_percent > 0.0 && ram_pct >= t.ram_used_percent {
            let is_crit = ram_pct >= (t.ram_used_percent + 10.0).min(95.0);
            let latched = hyst > 0.0 && self.latched_triggers.contains("ram_pct");
            if (is_crit && !in_emergency_cooldown) || (!latched && !in_cooldown) {
                reasons.push(format!(
                    "RAM used {ram_pct:.1}% >= {:.1}%",
                    t.ram_used_percent
                ));
                pending_latches.push("ram_pct");
                if is_crit {
                    critical = true;
                }
            }
        }

        if let Some(floor) = t.effective_available_floor(mem.total_mb) {
            if floor > 0 && mem.available_mb <= floor {
                let is_emerg = mem.available_mb <= (floor / 2);
                if (is_emerg && !in_emergency_cooldown) || !in_cooldown {
                    reasons.push(format!(
                        "Available RAM {} MiB <= {floor} MiB",
                        mem.available_mb
                    ));
                    if is_emerg {
                        critical = true;
                    }
                }
            }
        }

        if t.swap_used_percent > 0.0 && mem.swap_total_mb > 0 && swap_pct >= t.swap_used_percent {
            let latched = hyst > 0.0 && self.latched_triggers.contains("swap_pct");
            if !latched && !in_cooldown {
                reasons.push(format!(
                    "Swap used {swap_pct:.1}% >= {:.1}%",
                    t.swap_used_percent
                ));
                pending_latches.push("swap_pct");
            }
        }

        if t.swap_used_mb > 0 && mem.swap_used_mb >= t.swap_used_mb && !in_cooldown {
            reasons.push(format!(
                "Swap used {} MiB >= {} MiB",
                mem.swap_used_mb, t.swap_used_mb
            ));
        }

        // 2. Linux PSI Triggers
        if psi.available {
            if t.psi_full_avg10 > 0.0 && psi.full_avg10 >= t.psi_full_avg10 {
                if !in_emergency_cooldown {
                    reasons.push(format!(
                        "Memory PSI full avg10={:.2} >= {:.2}",
                        psi.full_avg10, t.psi_full_avg10
                    ));
                    critical = true;
                }
            } else if t.psi_some_avg10 > 0.0 && psi.some_avg10 >= t.psi_some_avg10 && !in_cooldown {
                reasons.push(format!(
                    "Memory PSI some avg10={:.2} >= {:.2}",
                    psi.some_avg10, t.psi_some_avg10
                ));
            }
        }

        // 3. tmpfs Usage Trigger
        if t.tmpfs_used_percent > 0.0 && self.config.tmpfs.enabled {
            let (max_tmp, mount) = snapshot.max_tmpfs_used_percent(Some(&self.config.tmpfs.paths));
            let latched = hyst > 0.0 && self.latched_triggers.contains("tmpfs_pct");
            if let Some(mp) = mount {
                if max_tmp >= t.tmpfs_used_percent && !latched && !in_cooldown {
                    reasons.push(format!(
                        "tmpfs {mp} used {max_tmp:.1}% >= {:.1}%",
                        t.tmpfs_used_percent
                    ));
                    pending_latches.push("tmpfs_pct");
                }
            }
        }

        // 4. Proactive Interval Trigger
        if t.interval_minutes > 0 {
            let interval = Duration::from_secs(t.interval_minutes * 60);
            if now_mono.saturating_duration_since(self.last_interval) >= interval {
                pending_interval = true;
                reasons.push(format!("Periodic interval ({}m)", t.interval_minutes));
            }
        }

        // 5. Wall-clock / Cron Schedule Trigger
        if self.fired_schedule_minutes.len() > 64 {
            self.fired_schedule_minutes.retain(|k| {
                k.1 == now_local.year
                    && k.2 == now_local.month
                    && k.3 == now_local.day
                    && k.4 == now_local.hour
                    && k.5 == now_local.minute
            });
        }
        for expr in &t.schedules {
            let key: SchedKey = (
                expr.clone(),
                now_local.year,
                now_local.month,
                now_local.day,
                now_local.hour,
                now_local.minute,
            );
            if !self.fired_schedule_minutes.contains(&key) && schedule_matches(expr, now_local) {
                pending_scheds.push(key);
                reasons.push(format!("Schedule matched ({expr})"));
            }
        }

        if reasons.is_empty() {
            return TriggerDecision::default();
        }

        // CPU Load Guardrail (defer BEFORE committing state)
        let load_ratio = snapshot.load_ratio();
        if !critical && t.max_load_avg_ratio > 0.0 && load_ratio > t.max_load_avg_ratio {
            return TriggerDecision {
                should_run: false,
                reasons,
                critical: false,
                deferred: true,
                defer_reason: format!(
                    "CPU load ratio {load_ratio:.2} > max_load_avg_ratio {:.2}",
                    t.max_load_avg_ratio
                ),
            };
        }

        // Commit state mutations
        for l in pending_latches {
            self.latched_triggers.insert(l);
        }
        if pending_interval {
            self.last_interval = now_mono;
        }
        for k in pending_scheds {
            self.fired_schedule_minutes.insert(k);
        }

        TriggerDecision {
            should_run: true,
            reasons,
            critical,
            deferred: false,
            defer_reason: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MemoryInfo, PsiMemory, TmpfsMount};

    fn make_snap(avail_mb: u64, psi_full: f64, load_1m: f64) -> SystemSnapshot {
        let total = 16000;
        SystemSnapshot {
            memory: MemoryInfo {
                total_mb: total,
                available_mb: avail_mb,
                used_mb: total - avail_mb,
                ..Default::default()
            },
            psi: PsiMemory {
                available: true,
                full_avg10: psi_full,
                ..Default::default()
            },
            swaps: Vec::new(),
            tmpfs_mounts: vec![TmpfsMount {
                mountpoint: "/tmp".into(),
                total_mb: 8000,
                used_mb: 100,
                free_mb: 7900,
            }],
            load_avg_1m: load_1m,
            load_avg_5m: load_1m,
            load_avg_15m: load_1m,
            cpu_count: 4,
        }
    }

    #[test]
    fn available_floor_does_not_fire_at_idle_on_small_hosts_but_still_handles_pressure() {
        let local = LocalTime {
            year: 2026,
            month: 11,
            day: 24,
            hour: 12,
            minute: 0,
            weekday: 2,
        };
        let now = Instant::now();
        let cases = [
            (384, 305, None, false, false),
            (384, 48, None, true, false),
            (384, 24, None, true, true),
            (384, 305, Some(1536), true, true),
            (384, 1, Some(0), false, false),
            (0, 0, None, false, false),
            (u64::MAX, 1536, None, true, false),
        ];
        for (total, available, floor, expected, critical) in cases {
            let mut cfg = Config::default();
            // Isolate this trigger; the ordinary RAM percentage has its own policy.
            cfg.triggers.ram_used_percent = 0.0;
            cfg.triggers.min_available_mb = floor;
            let mut snap = make_snap(0, 0.0, 0.0);
            snap.memory.total_mb = total;
            snap.memory.available_mb = available;
            snap.memory.used_mb = total.saturating_sub(available);
            let result = TriggerEngine::new(cfg).evaluate(&snap, local, now);
            assert_eq!(
                (result.should_run, result.critical),
                (expected, critical),
                "{total}, {available}, {floor:?}"
            );
            assert_eq!(
                result
                    .reasons
                    .iter()
                    .any(|r| r.starts_with("Available RAM")),
                expected
            );
        }

        // A real default-policy idle reproduction, including its other triggers.
        let mut snap = make_snap(305, 0.0, 0.0);
        snap.memory.total_mb = 384;
        snap.memory.used_mb = 79;
        assert!(
            !TriggerEngine::new(Config::default())
                .evaluate(&snap, local, now)
                .should_run
        );
    }

    #[test]
    fn hysteresis_and_cooldown_work() {
        let mut cfg = Config::default();
        cfg.triggers.ram_used_percent = 80.0;
        cfg.triggers.hysteresis_percent = 5.0;
        cfg.triggers.cooldown_seconds = 300;
        let mut eng = TriggerEngine::new(cfg);

        let base = Instant::now();
        let lt = LocalTime {
            year: 2026,
            month: 11,
            day: 19,
            hour: 12,
            minute: 0,
            weekday: 4,
        };

        let high = make_snap(2500, 0.0, 0.5); // 84.4%
        assert!(eng.evaluate(&high, lt, base).should_run);
        eng.record_cleanup(base);

        // Cooldown blocks
        assert!(!eng.evaluate(&high, lt, base + Duration::from_secs(100)).should_run);
        // After cooldown, hysteresis still holds latch while >= 75%
        assert!(!eng.evaluate(&high, lt, base + Duration::from_secs(400)).should_run);
        // Drop to 74% re-arms
        let rearm = make_snap(4160, 0.0, 0.5);
        assert!(!eng.evaluate(&rearm, lt, base + Duration::from_secs(410)).should_run);
        // Crossing >= 80% fires again
        assert!(eng.evaluate(&high, lt, base + Duration::from_secs(420)).should_run);
    }

    #[test]
    fn cron_sunday_range_and_step_base() {
        let sun = LocalTime {
            year: 2026,
            month: 11,
            day: 22,
            hour: 3,
            minute: 20,
            weekday: 0,
        };
        assert!(schedule_matches("5/15 3 * * 5-7", sun));
        let sun2 = LocalTime { minute: 21, ..sun };
        assert!(!schedule_matches("5/15 3 * * 5-7", sun2));
    }

    #[test]
    fn load_guardrail_defers_without_committing_state() {
        let mut cfg = Config::default();
        cfg.triggers.ram_used_percent = 80.0;
        cfg.triggers.max_load_avg_ratio = 1.0;
        let mut eng = TriggerEngine::new(cfg);
        let now = Instant::now();
        let lt = LocalTime::now();
        let busy = make_snap(2500, 0.0, 8.0); // ratio 2.0
        let dec = eng.evaluate(&busy, lt, now);
        assert!(!dec.should_run);
        assert!(dec.deferred);
        // Once load drops, trigger is NOT latched and fires immediately
        let calm = make_snap(2500, 0.0, 1.0);
        assert!(eng.evaluate(&calm, lt, now + Duration::from_secs(5)).should_run);
    }
}
