"""Unit tests for keep-ram-clean."""

from __future__ import annotations

import datetime as dt
import os
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from keep_ram_clean.cleaners import clean_tmpfs, clean_user_caches, flush_swap
from keep_ram_clean.config import (
    Config,
    EXAMPLE_CONFIG_TOML,
    _fallback_parse_toml,
    _verify_root_config_security,
    load_config,
)
from keep_ram_clean.daemon import _send_notification, read_daemon_state
from keep_ram_clean.metrics import (
    MemoryInfo,
    PsiMemory,
    SwapDevice,
    SystemSnapshot,
    TmpfsMount,
    is_mount_tmpfs,
    parse_meminfo,
    parse_psi,
    parse_swaps,
    parse_tmpfs_mountpoints,
)
from keep_ram_clean.triggers import TriggerEngine, schedule_matches


class TestConfig(unittest.TestCase):
    def test_example_config_loads_and_validates(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "config.toml"
            p.write_text(EXAMPLE_CONFIG_TOML, encoding="utf-8")
            cfg = load_config(p)
            self.assertEqual(cfg.validate(), [])
            self.assertEqual(cfg.triggers.ram_used_percent, 80.0)
            self.assertEqual(cfg.kernel.drop_caches_level, 2)
            self.assertTrue(cfg.swap.skip_zram)

    def test_fallback_toml_parser_handles_arrays_and_rules(self) -> None:
        sample = """
[triggers]
ram_used_percent = 75.5
schedules = ["03:00", "*/30 * * * *"]

[watchdog]
enabled = true

[[watchdog.rules]]
name = "indexer"
pattern = "baloo_file"
max_rss_mb = 512
action = "graceful"
allow_sigkill = false
"""
        parsed = _fallback_parse_toml(sample)
        self.assertEqual(parsed["triggers"]["ram_used_percent"], 75.5)
        self.assertEqual(parsed["triggers"]["schedules"], ["03:00", "*/30 * * * *"])
        self.assertTrue(parsed["watchdog"]["enabled"])
        self.assertEqual(len(parsed["watchdog"]["rules"]), 1)
        self.assertEqual(parsed["watchdog"]["rules"][0]["name"], "indexer")
        self.assertFalse(parsed["watchdog"]["rules"][0]["allow_sigkill"])

    def test_invalid_config_rejected(self) -> None:
        cfg = Config()
        cfg.kernel.drop_caches_level = 9
        cfg.triggers.schedules = ["99:99"]
        errors = cfg.validate()
        self.assertTrue(any("drop_caches_level" in e for e in errors))
        self.assertTrue(any("schedules" in e for e in errors))


class TestMetrics(unittest.TestCase):
    def test_parse_meminfo(self) -> None:
        text = """
MemTotal:       16384000 kB
MemFree:         2048000 kB
MemAvailable:    4096000 kB
Buffers:          512000 kB
Cached:          2048000 kB
SReclaimable:     256000 kB
Dirty:             10240 kB
SwapTotal:       4096000 kB
SwapFree:        3072000 kB
"""
        m = parse_meminfo(text)
        self.assertEqual(m.total_mb, 16000)
        self.assertEqual(m.available_mb, 4000)
        self.assertEqual(m.used_mb, 12000)
        self.assertAlmostEqual(m.used_percent, 75.0)
        self.assertEqual(m.swap_used_mb, 1000)
        self.assertAlmostEqual(m.swap_used_percent, 25.0)

    def test_parse_psi(self) -> None:
        text = (
            "some avg10=12.50 avg60=5.10 avg300=1.00 total=12345\n"
            "full avg10=3.25 avg60=1.20 avg300=0.40 total=6789\n"
        )
        psi = parse_psi(text)
        self.assertTrue(psi.available)
        self.assertEqual(psi.some_avg10, 12.50)
        self.assertEqual(psi.full_avg10, 3.25)

    def test_parse_swaps_zram_detection(self) -> None:
        text = (
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n"
            "/dev/zram0                              partition\t4194304\t\t524288\t\t100\n"
            "/swapfile                               file\t\t8388608\t\t1048576\t\t-2\n"
        )
        swaps = parse_swaps(text)
        self.assertEqual(len(swaps), 2)
        self.assertTrue(swaps[0].is_zram)
        self.assertEqual(swaps[0].used_mb, 512)
        self.assertFalse(swaps[1].is_zram)
        self.assertEqual(swaps[1].used_mb, 1024)

    def test_parse_tmpfs_mountpoints(self) -> None:
        mounts = (
            "/dev/nvme0n1p2 / ext4 rw,relatime 0 0\n"
            "tmpfs /tmp tmpfs rw,nosuid,nodev 0 0\n"
            "tmpfs /dev/shm tmpfs rw,nosuid,nodev 0 0\n"
        )
        pts = parse_tmpfs_mountpoints(mounts)
        self.assertEqual(pts, ["/tmp", "/dev/shm"])


class TestTriggers(unittest.TestCase):
    def _make_snapshot(
        self,
        total_mb: int = 16000,
        avail_mb: int = 8000,
        swap_total: int = 4000,
        swap_used: int = 0,
        psi_some: float = 0.0,
        psi_full: float = 0.0,
        tmp_used_mb: int = 100,
        tmp_total_mb: int = 8000,
        load_1m: float = 0.5,
        cpus: int = 4,
    ) -> SystemSnapshot:
        mem = MemoryInfo(
            total_mb=total_mb,
            available_mb=avail_mb,
            used_mb=total_mb - avail_mb,
            buffers_mb=0,
            cached_mb=0,
            slab_reclaimable_mb=0,
            dirty_mb=0,
            swap_total_mb=swap_total,
            swap_free_mb=swap_total - swap_used,
            swap_used_mb=swap_used,
        )
        psi = PsiMemory(available=True, some_avg10=psi_some, full_avg10=psi_full)
        mounts = [
            TmpfsMount(
                mountpoint="/tmp",
                total_mb=tmp_total_mb,
                used_mb=tmp_used_mb,
                free_mb=tmp_total_mb - tmp_used_mb,
            )
        ]
        return SystemSnapshot(
            memory=mem,
            psi=psi,
            tmpfs_mounts=mounts,
            load_avg_1m=load_1m,
            cpu_count=cpus,
        )

    def test_ram_threshold_hysteresis_and_cooldown(self) -> None:
        cfg = Config()
        cfg.triggers.ram_used_percent = 80.0
        cfg.triggers.hysteresis_percent = 5.0
        cfg.triggers.cooldown_seconds = 300
        eng = TriggerEngine(cfg)

        low = self._make_snapshot(avail_mb=8000)  # 50% used
        self.assertFalse(eng.evaluate(low, now_mono=1000.0).should_run)

        high = self._make_snapshot(avail_mb=2500)  # 84.4% used
        dec = eng.evaluate(high, now_mono=1010.0)
        self.assertTrue(dec.should_run)
        self.assertTrue(any("RAM used" in r for r in dec.reasons))

        # Record cleanup and verify cooldown suppresses immediate re-run
        eng.record_cleanup(now_mono=1010.0)
        self.assertFalse(eng.evaluate(high, now_mono=1100.0).should_run)

        # Even after cooldown elapses, hysteresis keeps it latched while still >= 75%
        self.assertFalse(eng.evaluate(high, now_mono=1350.0).should_run)

        # Dropping to 74% (<= 80 - 5) re-arms the trigger
        rearm = self._make_snapshot(avail_mb=4160)  # 74.0% used
        self.assertFalse(eng.evaluate(rearm, now_mono=1360.0).should_run)

        # Crossing >= 80% again now fires
        self.assertTrue(eng.evaluate(high, now_mono=1370.0).should_run)

    def test_psi_full_marks_critical(self) -> None:
        cfg = Config()
        cfg.triggers.psi_full_avg10 = 2.0
        eng = TriggerEngine(cfg)
        snap = self._make_snapshot(psi_full=5.5)
        dec = eng.evaluate(snap, now_mono=100.0)
        self.assertTrue(dec.should_run)
        self.assertTrue(dec.critical)

    def test_load_guardrail_defers_non_critical(self) -> None:
        cfg = Config()
        cfg.triggers.ram_used_percent = 80.0
        cfg.triggers.max_load_avg_ratio = 1.0
        eng = TriggerEngine(cfg)
        snap = self._make_snapshot(avail_mb=2500, load_1m=8.0, cpus=4)  # ratio 2.0
        dec = eng.evaluate(snap, now_mono=100.0)
        self.assertFalse(dec.should_run)
        self.assertTrue(dec.deferred)

    def test_schedule_matching_and_dedup(self) -> None:
        self.assertTrue(schedule_matches("03:30", dt.datetime(2026, 11, 19, 3, 30)))
        self.assertFalse(schedule_matches("03:30", dt.datetime(2026, 11, 19, 3, 31)))
        self.assertTrue(schedule_matches("*/15 * * * *", dt.datetime(2026, 11, 19, 10, 45)))
        self.assertFalse(schedule_matches("*/15 * * * *", dt.datetime(2026, 11, 19, 10, 46)))

        cfg = Config()
        cfg.triggers.schedules = ["03:30"]
        eng = TriggerEngine(cfg)
        snap = self._make_snapshot()
        t = dt.datetime(2026, 11, 19, 3, 30, 5)
        self.assertTrue(eng.evaluate(snap, now_dt=t, now_mono=10.0).should_run)
        # Same minute should not fire a second time
        t2 = dt.datetime(2026, 11, 19, 3, 30, 20)
        self.assertFalse(eng.evaluate(snap, now_dt=t2, now_mono=25.0).should_run)


class TestCleaners(unittest.TestCase):
    def test_tmpfs_cleaner_age_exclude_and_open_file_safety(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            old_match = root / "npm-stale-123"
            old_match.mkdir()
            (old_match / "blob.bin").write_bytes(b"x" * 4096)

            fresh_match = root / "npm-fresh-456"
            fresh_match.mkdir()

            excluded = root / "ssh-agent-1"
            excluded.mkdir()

            open_match = root / "tmp-open-file"
            open_match.write_bytes(b"active")

            # Backdate old_match (and its nested file), excluded, and open_match by 2 hours
            old_ts = time.time() - 7200
            for p in (old_match, old_match / "blob.bin", excluded, open_match):
                os.utime(p, (old_ts, old_ts))

            cfg = Config()
            cfg.tmpfs.paths = [str(root)]
            cfg.tmpfs.require_tmpfs = False
            cfg.tmpfs.patterns = ["npm-*", "ssh-*", "tmp-*"]
            cfg.tmpfs.exclude_patterns = ["ssh-*"]
            cfg.tmpfs.min_age_minutes = 30
            cfg.tmpfs.skip_open_files = True

            # Hold open_match open so /proc/self/fd references it
            with open(open_match, "rb"):
                dry = clean_tmpfs(cfg, dry_run=True)
                self.assertEqual(dry.status, "dry-run")
                self.assertTrue(old_match.exists())

                real = clean_tmpfs(cfg, dry_run=False)
                self.assertEqual(real.status, "ok")
                self.assertFalse(old_match.exists())
                self.assertTrue(fresh_match.exists())
                self.assertTrue(excluded.exists())
                self.assertTrue(open_match.exists())

    def test_swap_flusher_skips_zram(self) -> None:
        cfg = Config()
        cfg.swap.skip_zram = True
        cfg.swap.min_swap_used_mb = 100
        zram_only = [SwapDevice("/dev/zram0", "partition", 4096, 512, 100)]
        with mock.patch("os.geteuid", return_value=0), mock.patch(
            "keep_ram_clean.cleaners.read_swaps", return_value=zram_only
        ):
            res = flush_swap(cfg, dry_run=True)
            self.assertEqual(res.status, "skipped")
            self.assertIn("zram", res.summary)


class TestSecurityHardening(unittest.TestCase):
    def test_root_config_security_rejects_symlink_and_non_root(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            real_cfg = Path(td) / "real.toml"
            real_cfg.write_text("[triggers]\nram_used_percent = 80.0\n", encoding="utf-8")
            link_cfg = Path(td) / "link.toml"
            link_cfg.symlink_to(real_cfg)

            with self.assertRaises(PermissionError):
                _verify_root_config_security(link_cfg)

            if os.geteuid() != 0:
                with self.assertRaises(PermissionError):
                    _verify_root_config_security(real_cfg)

    def test_root_load_config_never_reads_user_config(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            user_cfg = Path(td) / "config.toml"
            user_cfg.write_text("[triggers]\nram_used_percent = 12.0\n", encoding="utf-8")
            with mock.patch("os.geteuid", return_value=0), mock.patch(
                "keep_ram_clean.config.default_user_config", return_value=user_cfg
            ), mock.patch(
                "keep_ram_clean.config.DEFAULT_SYSTEM_CONFIG",
                Path(td) / "nonexistent-system.toml",
            ):
                cfg = load_config(None)
                # Must stay at default 80.0, not user's 12.0
                self.assertEqual(cfg.triggers.ram_used_percent, 80.0)

    def test_user_caches_refuses_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            home = Path(td) / "home"
            victim = Path(td) / "victim"
            victim.mkdir(parents=True)
            victim_file = victim / "important.txt"
            victim_file.write_text("keep me", encoding="utf-8")
            old_ts = time.time() - 86400 * 30
            os.utime(victim_file, (old_ts, old_ts))

            cache = home / ".cache"
            cache.mkdir(parents=True)
            (cache / "thumbnails").symlink_to(victim)
            (cache / "pip").symlink_to(victim)

            cfg = Config()
            cfg.user_caches.enabled = True
            cfg.user_caches.clean_thumbnails = True
            cfg.user_caches.clean_pip_cache = True
            with mock.patch(
                "keep_ram_clean.cleaners._resolve_target_users",
                return_value=[(os.geteuid(), home)],
            ):
                clean_user_caches(cfg, dry_run=False)

            self.assertTrue(victim.exists())
            self.assertTrue(victim_file.exists())

    def test_shadowed_mountpoint_not_misclassified_as_tmpfs(self) -> None:
        mounts = (
            "tmpfs /tmp tmpfs rw 0 0\n"
            "/dev/sda1 /tmp ext4 rw 0 0\n"
        )
        self.assertFalse(is_mount_tmpfs("/tmp", mounts))

        mounts_reversed = (
            "/dev/sda1 /tmp ext4 rw 0 0\n"
            "tmpfs /tmp tmpfs rw 0 0\n"
        )
        self.assertTrue(is_mount_tmpfs("/tmp", mounts_reversed))

    def test_cron_bounds_and_root_path_rejected(self) -> None:
        cfg = Config()
        cfg.triggers.schedules = ["99 99 99 99 99", "*/0 * * * *"]
        cfg.tmpfs.paths = ["/"]
        errors = cfg.validate()
        self.assertTrue(any("99 99 99 99 99" in e for e in errors))
        self.assertTrue(any("*/0" in e for e in errors))
        self.assertTrue(any("unsafe" in e for e in errors))

    def test_fallback_toml_escaped_quotes_and_utf8(self) -> None:
        sample = 'patterns = ["a,\\"b", "café"]\n'
        parsed = _fallback_parse_toml(sample)
        self.assertEqual(parsed["patterns"], ['a,"b', "café"])

    def test_read_daemon_state_rejects_stopped_and_pid_reuse(self) -> None:
        import json

        with tempfile.TemporaryDirectory() as td:
            rdir = Path(td)
            os.chmod(rdir, 0o700)
            state_p = rdir / "state.json"
            # Stopped state must never report alive=True even for our live PID
            state_p.write_text(
                json.dumps({"pid": os.getpid(), "status": "stopped", "starttime": 1}),
                encoding="utf-8",
            )
            os.chmod(state_p, 0o600)
            st = read_daemon_state(runtime_dir=rdir)
            self.assertIsNotNone(st)
            assert st is not None
            self.assertFalse(st["alive"])

            # Running state with wrong starttime (PID reuse) must report alive=False
            state_p.write_text(
                json.dumps({"pid": os.getpid(), "status": "running", "starttime": 999999999}),
                encoding="utf-8",
            )
            os.chmod(state_p, 0o600)
            st2 = read_daemon_state(runtime_dir=rdir)
            self.assertIsNotNone(st2)
            assert st2 is not None
            self.assertFalse(st2["alive"])

    def test_notify_send_handles_timeout_and_skips_root(self) -> None:
        import subprocess
        from keep_ram_clean.cleaners import CleanupReport

        cfg = Config()
        cfg.notifications.enabled = True
        cfg.notifications.min_freed_mb = 0
        report = CleanupReport(0.0, 1.0, False, ["<b>test</b>"], 1000, 1200, 0, 0)

        with mock.patch("os.geteuid", return_value=0), mock.patch(
            "subprocess.run"
        ) as m_run:
            _send_notification(cfg, report, critical=False)
            m_run.assert_not_called()

        with mock.patch("os.geteuid", return_value=1000), mock.patch(
            "shutil.which", return_value="/usr/bin/notify-send"
        ), mock.patch(
            "subprocess.run", side_effect=subprocess.TimeoutExpired("notify-send", 5)
        ):
            # Must not raise
            _send_notification(cfg, report, critical=False)

    def test_extra_cache_dirs_rejects_home_and_negative_age(self) -> None:
        cfg = Config()
        cfg.user_caches.extra_cache_dirs = ["~/", "", "."]
        cfg.user_caches.thumbnail_max_age_days = -1
        errors = cfg.validate()
        self.assertTrue(any("extra_cache_dirs" in e for e in errors))
        self.assertTrue(any("thumbnail_max_age_days" in e for e in errors))

    def test_unknown_top_level_section_and_nan_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "bad.toml"
            p.write_text("[trigger]\nram_used_percent = 80.0\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                load_config(p)

        cfg = Config()
        cfg.triggers.max_load_avg_ratio = float("nan")
        self.assertTrue(any("max_load_avg_ratio" in e for e in cfg.validate()))

    def test_cron_sunday_range_and_step_base(self) -> None:
        # 2026-11-22 is a Sunday
        sun = dt.datetime(2026, 11, 22, 3, 20)
        self.assertTrue(schedule_matches("5/15 3 * * 5-7", sun))
        self.assertFalse(schedule_matches("5/15 3 * * 5-7", dt.datetime(2026, 11, 22, 3, 21)))

    def test_parse_meminfo_zero_available_does_not_inflate(self) -> None:
        text = "MemTotal: 16384000 kB\nMemFree: 512000 kB\nMemAvailable: 512 kB\nCached: 4096000 kB\n"
        m = parse_meminfo(text)
        self.assertEqual(m.available_mb, 0)

    def test_config_example_parity(self) -> None:
        repo_root = Path(__file__).resolve().parent.parent
        on_disk = (repo_root / "config" / "config.example.toml").read_text(encoding="utf-8")
        self.assertEqual(on_disk, EXAMPLE_CONFIG_TOML)

    def test_cli_subcommands_and_doctor_json(self) -> None:
        import io
        import json
        from contextlib import redirect_stdout
        from keep_ram_clean.cli import main

        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = main(["doctor", "--json"])
        self.assertEqual(rc, 0)
        data = json.loads(buf.getvalue())
        self.assertIn("findings", data)
        self.assertTrue(len(data["findings"]) >= 4)

        buf2 = io.StringIO()
        with redirect_stdout(buf2):
            rc2 = main(["run", "--dry-run", "--json"])
        self.assertEqual(rc2, 0)
        run_data = json.loads(buf2.getvalue())
        self.assertTrue(run_data["dry_run"])
        self.assertIn("version", run_data)

        # Test -c/--config accepted after subcommand
        with tempfile.TemporaryDirectory() as td:
            cfg_p = Path(td) / "custom.toml"
            rc_init = main(["init-config", "-o", str(cfg_p)])
            self.assertEqual(rc_init, 0)
            self.assertTrue(cfg_p.exists())
            buf3 = io.StringIO()
            with redirect_stdout(buf3):
                rc3 = main(["status", "-c", str(cfg_p), "--json", "--top", "2"])
            self.assertEqual(rc3, 0)
            st_data = json.loads(buf3.getvalue())
            self.assertLessEqual(len(st_data["top_processes"]), 2)

    def test_flush_swap_returns_error_on_failure(self) -> None:
        import subprocess

        cfg = Config()
        cfg.swap.min_swap_used_mb = 100
        cfg.swap.safety_margin_mb = 100
        disk_swap = [SwapDevice("/swapfile", "file", 4096, 512, -2)]
        mem = MemoryInfo(16000, 8000, 8000, 0, 0, 0, 0, 4096, 3584, 512)
        with mock.patch("os.geteuid", return_value=0), mock.patch(
            "keep_ram_clean.cleaners.read_swaps", return_value=disk_swap
        ), mock.patch(
            "keep_ram_clean.cleaners._is_swapfile_writable", return_value=True
        ), mock.patch(
            "keep_ram_clean.cleaners.read_meminfo", return_value=mem
        ), mock.patch(
            "keep_ram_clean.cleaners._resolve_trusted_binary",
            side_effect=["/sbin/swapoff", "/sbin/swapon"],
        ), mock.patch(
            "subprocess.run",
            side_effect=subprocess.CalledProcessError(1, "swapoff"),
        ):
            res = flush_swap(cfg, dry_run=False)
            self.assertEqual(res.status, "error")

    def test_reclaim_kernel_eagain_vs_eacces(self) -> None:
        import errno as _errno
        from keep_ram_clean.cleaners import reclaim_kernel_memory

        cfg = Config()
        cfg.kernel.use_cgroup_reclaim = True
        cfg.kernel.drop_caches_level = 0
        cfg.kernel.compact_memory = False

        with mock.patch("os.geteuid", return_value=0), mock.patch.object(
            Path, "exists", return_value=True
        ), mock.patch.object(
            Path, "write_text", side_effect=OSError(_errno.EAGAIN, "Resource temporarily unavailable")
        ):
            res_ok = reclaim_kernel_memory(cfg, dry_run=False)
            self.assertEqual(res_ok.status, "ok")

        with mock.patch("os.geteuid", return_value=0), mock.patch.object(
            Path, "exists", return_value=True
        ), mock.patch.object(
            Path, "write_text", side_effect=OSError(_errno.EACCES, "Permission denied")
        ):
            res_err = reclaim_kernel_memory(cfg, dry_run=False)
            self.assertEqual(res_err.status, "error")


if __name__ == "__main__":
    unittest.main()
