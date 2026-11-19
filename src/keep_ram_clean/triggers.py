"""Trigger evaluation engine: reactive thresholds, PSI, hysteresis, cooldown, and cron schedules."""

from __future__ import annotations

import datetime as dt
import time
from dataclasses import dataclass, field
from typing import List, Optional, Set, Tuple

from .config import Config
from .metrics import SystemSnapshot


@dataclass
class TriggerDecision:
    should_run: bool
    reasons: List[str] = field(default_factory=list)
    critical: bool = False
    deferred: bool = False
    defer_reason: str = ""


def _match_cron_field(expr: str, value: int, min_val: int, max_val: int) -> bool:
    for part in expr.split(","):
        part = part.strip()
        step = 1
        has_step = "/" in part
        if has_step:
            base, step_str = part.split("/", 1)
            step = max(1, int(step_str))
        else:
            base = part

        if base == "*":
            if (value - min_val) % step == 0:
                return True
        elif "-" in base:
            lo_s, hi_s = base.split("-", 1)
            lo, hi = int(lo_s), int(hi_s)
            # Handle cron Sunday == 7 in day-of-week ranges like 5-7
            candidates = [value]
            if max_val == 6 and value == 0 and hi == 7:
                candidates.append(7)
            for cand in candidates:
                if lo <= cand <= hi and (cand - lo) % step == 0:
                    return True
        else:
            num = int(base)
            if max_val == 6 and num == 7:
                num = 0
            if has_step:
                if value >= num and (value - num) % step == 0:
                    return True
            elif value == num:
                return True
    return False


def schedule_matches(expr: str, now: dt.datetime) -> bool:
    """Check whether a 'HH:MM' or 5-field cron expression matches `now` (to the minute)."""
    expr = expr.strip()
    if ":" in expr and " " not in expr:
        hh_s, mm_s = expr.split(":", 1)
        return now.hour == int(hh_s) and now.minute == int(mm_s)

    parts = expr.split()
    if len(parts) != 5:
        return False

    minute_f, hour_f, dom_f, mon_f, dow_f = parts
    # Python weekday(): Monday=0..Sunday=6 -> cron: Sunday=0, Monday=1..Saturday=6
    cron_dow = (now.weekday() + 1) % 7
    return (
        _match_cron_field(minute_f, now.minute, 0, 59)
        and _match_cron_field(hour_f, now.hour, 0, 23)
        and _match_cron_field(dom_f, now.day, 1, 31)
        and _match_cron_field(mon_f, now.month, 1, 12)
        and _match_cron_field(dow_f, cron_dow, 0, 6)
    )


class TriggerEngine:
    """Stateful trigger evaluator for daemon polling."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self._last_cleanup_mono: Optional[float] = None
        self._last_interval_mono: float = time.monotonic()
        self._fired_schedule_minutes: Set[Tuple[str, int, int, int, int, int]] = set()
        self._latched_triggers: Set[str] = set()

    def update_config(self, config: Config) -> None:
        self.config = config
        self._latched_triggers.clear()

    def record_cleanup(self, now_mono: Optional[float] = None) -> None:
        ts = time.monotonic() if now_mono is None else now_mono
        self._last_cleanup_mono = ts

    def evaluate(
        self,
        snapshot: SystemSnapshot,
        now_dt: Optional[dt.datetime] = None,
        now_mono: Optional[float] = None,
    ) -> TriggerDecision:
        t = self.config.triggers
        now_dt = dt.datetime.now() if now_dt is None else now_dt
        now_mono = time.monotonic() if now_mono is None else now_mono

        reasons: List[str] = []
        critical = False
        pending_latches: Set[str] = set()
        pending_interval_update = False
        pending_sched_keys: List[Tuple[str, int, int, int, int, int]] = []
        mem = snapshot.memory
        psi = snapshot.psi

        # --- Hysteresis re-arm checks ---
        hyst = max(0.0, t.hysteresis_percent)
        if "ram_pct" in self._latched_triggers and mem.used_percent <= (t.ram_used_percent - hyst):
            self._latched_triggers.discard("ram_pct")
        if "swap_pct" in self._latched_triggers and mem.swap_used_percent <= (t.swap_used_percent - hyst):
            self._latched_triggers.discard("swap_pct")
        if "tmpfs_pct" in self._latched_triggers:
            max_tmp_pct, _ = snapshot.max_tmpfs_used_percent(self.config.tmpfs.paths)
            if max_tmp_pct <= (t.tmpfs_used_percent - hyst):
                self._latched_triggers.discard("tmpfs_pct")

        # --- Cooldown check ---
        in_cooldown = False
        in_emergency_cooldown = False
        if self._last_cleanup_mono is not None:
            elapsed = now_mono - self._last_cleanup_mono
            in_cooldown = elapsed < t.cooldown_seconds
            emergency_cd = min(t.cooldown_seconds, 60)
            in_emergency_cooldown = elapsed < emergency_cd

        # --- 1. Reactive Memory Thresholds ---
        if t.ram_used_percent > 0 and mem.used_percent >= t.ram_used_percent:
            is_crit = mem.used_percent >= min(95.0, t.ram_used_percent + 10.0)
            latched = hyst > 0 and "ram_pct" in self._latched_triggers
            if (is_crit and not in_emergency_cooldown) or (not latched and not in_cooldown):
                reasons.append(
                    f"RAM used {mem.used_percent:.1f}% >= {t.ram_used_percent:.1f}%"
                )
                pending_latches.add("ram_pct")
                if is_crit:
                    critical = True

        if t.min_available_mb > 0 and mem.available_mb <= t.min_available_mb:
            is_emerg = mem.available_mb <= (t.min_available_mb // 2)
            if (is_emerg and not in_emergency_cooldown) or not in_cooldown:
                reasons.append(
                    f"Available RAM {mem.available_mb} MiB <= {t.min_available_mb} MiB"
                )
                if is_emerg:
                    critical = True

        if t.swap_used_percent > 0 and mem.swap_total_mb > 0:
            latched = hyst > 0 and "swap_pct" in self._latched_triggers
            if mem.swap_used_percent >= t.swap_used_percent and not latched and not in_cooldown:
                reasons.append(
                    f"Swap used {mem.swap_used_percent:.1f}% >= {t.swap_used_percent:.1f}%"
                )
                pending_latches.add("swap_pct")

        if t.swap_used_mb > 0 and mem.swap_used_mb >= t.swap_used_mb and not in_cooldown:
            reasons.append(
                f"Swap used {mem.swap_used_mb} MiB >= {t.swap_used_mb} MiB"
            )

        # --- 2. Linux PSI Triggers ---
        if psi.available:
            if t.psi_full_avg10 > 0 and psi.full_avg10 >= t.psi_full_avg10:
                if not in_emergency_cooldown:
                    reasons.append(
                        f"Memory PSI full avg10={psi.full_avg10:.2f} >= {t.psi_full_avg10:.2f}"
                    )
                    critical = True
            elif t.psi_some_avg10 > 0 and psi.some_avg10 >= t.psi_some_avg10:
                if not in_cooldown:
                    reasons.append(
                        f"Memory PSI some avg10={psi.some_avg10:.2f} >= {t.psi_some_avg10:.2f}"
                    )

        # --- 3. tmpfs Usage Trigger ---
        if t.tmpfs_used_percent > 0 and self.config.tmpfs.enabled:
            max_tmp_pct, mount = snapshot.max_tmpfs_used_percent(self.config.tmpfs.paths)
            latched = hyst > 0 and "tmpfs_pct" in self._latched_triggers
            if mount and max_tmp_pct >= t.tmpfs_used_percent and not latched and not in_cooldown:
                reasons.append(
                    f"tmpfs {mount} used {max_tmp_pct:.1f}% >= {t.tmpfs_used_percent:.1f}%"
                )
                pending_latches.add("tmpfs_pct")

        # --- 4. Proactive Interval Trigger ---
        if t.interval_minutes > 0:
            if (now_mono - self._last_interval_mono) >= (t.interval_minutes * 60):
                pending_interval_update = True
                reasons.append(f"Periodic interval ({t.interval_minutes}m)")

        # --- 5. Wall-clock / Cron Schedule Trigger ---
        minute_key_base = (now_dt.year, now_dt.month, now_dt.day, now_dt.hour, now_dt.minute)
        if len(self._fired_schedule_minutes) > 64:
            self._fired_schedule_minutes = {
                k for k in self._fired_schedule_minutes if k[1:] == minute_key_base
            }
        for expr in t.schedules:
            key = (expr, *minute_key_base)
            if key not in self._fired_schedule_minutes and schedule_matches(expr, now_dt):
                pending_sched_keys.append(key)
                reasons.append(f"Schedule matched ({expr})")

        if not reasons:
            return TriggerDecision(should_run=False)

        # --- Load Guardrail (defer BEFORE committing trigger state) ---
        if (
            not critical
            and t.max_load_avg_ratio > 0
            and snapshot.load_ratio > t.max_load_avg_ratio
        ):
            return TriggerDecision(
                should_run=False,
                reasons=reasons,
                critical=False,
                deferred=True,
                defer_reason=(
                    f"CPU load ratio {snapshot.load_ratio:.2f} > "
                    f"max_load_avg_ratio {t.max_load_avg_ratio:.2f}"
                ),
            )

        # Commit state mutations only when actually running
        self._latched_triggers.update(pending_latches)
        if pending_interval_update:
            self._last_interval_mono = now_mono
        self._fired_schedule_minutes.update(pending_sched_keys)

        return TriggerDecision(should_run=True, reasons=reasons, critical=critical)
