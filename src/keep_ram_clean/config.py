"""Configuration schema, TOML loading, validation, and default generation."""

from __future__ import annotations

import math
import os
import re
import stat
from dataclasses import dataclass, field, fields
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None  # type: ignore[assignment]


DEFAULT_SYSTEM_CONFIG = Path("/etc/keep-ram-clean/config.toml")
MAX_WATCHDOG_PATTERN_LEN = 256
KNOWN_SECTIONS = frozenset(
    {
        "triggers",
        "tmpfs",
        "watchdog",
        "user_caches",
        "kernel",
        "swap",
        "notifications",
        "logging",
    }
)


def default_user_config() -> Path:
    xdg = os.environ.get("XDG_CONFIG_HOME")
    base = Path(xdg) if (xdg and os.path.isabs(xdg)) else Path.home() / ".config"
    return base / "keep-ram-clean" / "config.toml"


def default_runtime_dir() -> Path:
    if os.geteuid() == 0:
        return Path("/run/keep-ram-clean")
    xdg_runtime = os.environ.get("XDG_RUNTIME_DIR")
    if xdg_runtime and os.path.isabs(xdg_runtime):
        return Path(xdg_runtime) / "keep-ram-clean"
    xdg_state = os.environ.get("XDG_STATE_HOME")
    state_base = (
        Path(xdg_state)
        if (xdg_state and os.path.isabs(xdg_state))
        else Path.home() / ".local" / "state"
    )
    return state_base / "keep-ram-clean"


@dataclass
class TriggerConfig:
    """User-configurable triggers determining *when* cleanup runs."""

    poll_interval_seconds: int = 15
    cooldown_seconds: int = 300
    hysteresis_percent: float = 5.0

    # Reactive memory thresholds (set to 0 to disable an individual trigger)
    ram_used_percent: float = 80.0
    min_available_mb: int = 1536
    swap_used_percent: float = 50.0
    swap_used_mb: int = 0
    tmpfs_used_percent: float = 70.0

    # Linux Pressure Stall Information (PSI) thresholds (0 to disable)
    psi_some_avg10: float = 10.0
    psi_full_avg10: float = 2.0

    # Proactive time-based triggers
    interval_minutes: int = 0
    schedules: List[str] = field(default_factory=list)

    # Guardrails
    max_load_avg_ratio: float = 0.0


@dataclass
class TmpfsConfig:
    """Configuration for cleaning stale files on RAM-backed tmpfs mounts."""

    enabled: bool = True
    paths: List[str] = field(default_factory=lambda: ["/tmp"])
    require_tmpfs: bool = True
    patterns: List[str] = field(default_factory=lambda: [
        "tmp*",
        "pytest-of-*",
        "npm-*",
        "pip-*",
        "cargo-*",
        "rustc*",
        "go-build*",
        "webpack-*",
        "vite-*",
    ])
    exclude_patterns: List[str] = field(default_factory=lambda: [
        ".X11-unix",
        ".ICE-unix",
        ".XIM-unix",
        ".font-unix",
        "systemd-private-*",
        "ssh-*",
        "tmux-*",
        "keep-ram-clean-*",
    ])
    min_age_minutes: int = 30
    owner_only: bool = True
    skip_open_files: bool = True


@dataclass
class WatchdogRule:
    """Rule for handling a specific runaway process."""

    name: str
    pattern: str
    max_rss_mb: int = 1024
    action: str = "graceful"
    grace_seconds: int = 10
    allow_sigkill: bool = False


@dataclass
class WatchdogConfig:
    """Configuration for runaway process monitoring."""

    enabled: bool = False
    rules: List[WatchdogRule] = field(default_factory=list)


@dataclass
class UserCacheConfig:
    """Optional disk/user-cache hygiene (separated from RAM reclamation)."""

    enabled: bool = False
    clean_thumbnails: bool = True
    thumbnail_max_age_days: int = 14
    clean_pip_cache: bool = False
    extra_cache_dirs: List[str] = field(default_factory=list)
    extra_cache_max_age_days: int = 14
    target_users: List[str] = field(default_factory=list)


@dataclass
class KernelConfig:
    """Kernel-level memory reclamation (requires root)."""

    enabled: bool = True
    use_cgroup_reclaim: bool = True
    cgroup_reclaim_mb: int = 512
    # 0 = off, 1 = pagecache, 2 = dentries & inodes (slab), 3 = both
    drop_caches_level: int = 2
    drop_caches_on_critical_only: bool = True
    compact_memory: bool = False


@dataclass
class SwapConfig:
    """Smart disk-swap flushing (requires root)."""

    enabled: bool = True
    min_swap_used_mb: int = 128
    safety_margin_mb: int = 1536
    skip_zram: bool = True
    max_load_avg_ratio: float = 1.5


@dataclass
class NotificationConfig:
    """Optional desktop notifications via notify-send."""

    enabled: bool = False
    min_freed_mb: int = 100
    on_critical_only: bool = False


@dataclass
class LoggingConfig:
    """Daemon & CLI logging configuration."""

    level: str = "INFO"
    file: str = ""
    max_bytes: int = 5 * 1024 * 1024
    backup_count: int = 3


def _in_finite_range(val: float, lo: float, hi: float) -> bool:
    return math.isfinite(val) and lo <= val <= hi


@dataclass
class Config:
    """Root configuration object for keep-ram-clean."""

    triggers: TriggerConfig = field(default_factory=TriggerConfig)
    tmpfs: TmpfsConfig = field(default_factory=TmpfsConfig)
    watchdog: WatchdogConfig = field(default_factory=WatchdogConfig)
    user_caches: UserCacheConfig = field(default_factory=UserCacheConfig)
    kernel: KernelConfig = field(default_factory=KernelConfig)
    swap: SwapConfig = field(default_factory=SwapConfig)
    notifications: NotificationConfig = field(default_factory=NotificationConfig)
    logging: LoggingConfig = field(default_factory=LoggingConfig)
    source_path: Optional[Path] = None

    def validate(self) -> List[str]:
        """Return a list of validation error messages (empty if valid)."""
        errors: List[str] = []
        t = self.triggers
        if t.poll_interval_seconds < 1:
            errors.append("triggers.poll_interval_seconds must be >= 1")
        if t.cooldown_seconds < 0:
            errors.append("triggers.cooldown_seconds must be >= 0")
        if not _in_finite_range(t.hysteresis_percent, 0.0, 50.0):
            errors.append("triggers.hysteresis_percent must be a finite number between 0 and 50")
        if not _in_finite_range(t.ram_used_percent, 0.0, 100.0):
            errors.append("triggers.ram_used_percent must be a finite number between 0 and 100")
        if t.min_available_mb < 0:
            errors.append("triggers.min_available_mb must be >= 0")
        if not _in_finite_range(t.swap_used_percent, 0.0, 100.0):
            errors.append("triggers.swap_used_percent must be a finite number between 0 and 100")
        if t.swap_used_mb < 0:
            errors.append("triggers.swap_used_mb must be >= 0")
        if not _in_finite_range(t.tmpfs_used_percent, 0.0, 100.0):
            errors.append("triggers.tmpfs_used_percent must be a finite number between 0 and 100")
        if not _in_finite_range(t.psi_some_avg10, 0.0, 100.0):
            errors.append("triggers.psi_some_avg10 must be a finite number between 0 and 100")
        if not _in_finite_range(t.psi_full_avg10, 0.0, 100.0):
            errors.append("triggers.psi_full_avg10 must be a finite number between 0 and 100")
        if t.interval_minutes < 0:
            errors.append("triggers.interval_minutes must be >= 0")
        if not (math.isfinite(t.max_load_avg_ratio) and t.max_load_avg_ratio >= 0.0):
            errors.append("triggers.max_load_avg_ratio must be a finite number >= 0")

        if self.kernel.drop_caches_level not in (0, 1, 2, 3):
            errors.append("kernel.drop_caches_level must be 0, 1, 2, or 3")
        if self.kernel.cgroup_reclaim_mb < 0:
            errors.append("kernel.cgroup_reclaim_mb must be >= 0")

        if self.swap.min_swap_used_mb < 0:
            errors.append("swap.min_swap_used_mb must be >= 0")
        if self.swap.safety_margin_mb < 0:
            errors.append("swap.safety_margin_mb must be >= 0")
        if not (math.isfinite(self.swap.max_load_avg_ratio) and self.swap.max_load_avg_ratio >= 0.0):
            errors.append("swap.max_load_avg_ratio must be a finite number >= 0")

        if self.tmpfs.min_age_minutes < 0:
            errors.append("tmpfs.min_age_minutes must be >= 0")
        for p in self.tmpfs.paths:
            norm = os.path.normpath(p)
            if not os.path.isabs(p) or norm == "/":
                errors.append(f"tmpfs.paths: unsafe or non-absolute path '{p}'")

        if self.user_caches.thumbnail_max_age_days < 0:
            errors.append("user_caches.thumbnail_max_age_days must be >= 0")
        if self.user_caches.extra_cache_max_age_days < 0:
            errors.append("user_caches.extra_cache_max_age_days must be >= 0")
        for extra in self.user_caches.extra_cache_dirs:
            stripped = extra.strip()
            rel = stripped[2:] if stripped.startswith("~/") else stripped
            parts = [pt for pt in Path(rel).parts if pt not in ("", ".")]
            if (
                not stripped
                or stripped in ("~", "~/", ".", "/", "..")
                or not parts
                or ".." in parts
            ):
                errors.append(
                    f"user_caches.extra_cache_dirs: unsafe cache directory '{extra}'"
                )

        if self.logging.max_bytes < 1024:
            errors.append("logging.max_bytes must be >= 1024")
        if self.logging.backup_count < 0:
            errors.append("logging.backup_count must be >= 0")

        for rule in self.watchdog.rules:
            if rule.action not in ("notify", "sigterm", "graceful", "sigkill"):
                errors.append(
                    f"watchdog rule '{rule.name}': invalid action '{rule.action}'"
                )
            if rule.max_rss_mb < 1:
                errors.append(f"watchdog rule '{rule.name}': max_rss_mb must be >= 1")
            if rule.grace_seconds < 0:
                errors.append(f"watchdog rule '{rule.name}': grace_seconds must be >= 0")
            if not rule.pattern or len(rule.pattern) > MAX_WATCHDOG_PATTERN_LEN:
                errors.append(
                    f"watchdog rule '{rule.name}': pattern length must be 1..{MAX_WATCHDOG_PATTERN_LEN}"
                )
            else:
                try:
                    re.compile(rule.pattern)
                except re.error as exc:
                    errors.append(
                        f"watchdog rule '{rule.name}': invalid regex '{rule.pattern}': {exc}"
                    )

        for sched in t.schedules:
            if not _is_valid_schedule(sched):
                errors.append(
                    f"triggers.schedules: invalid entry '{sched}' "
                    "(expected 'HH:MM' or 5-field cron 'M H DOM MON DOW')"
                )
        return errors


_CRON_BOUNDS: Tuple[Tuple[int, int], ...] = (
    (0, 59),  # minute
    (0, 23),  # hour
    (1, 31),  # day of month
    (1, 12),  # month
    (0, 7),   # day of week (0 and 7 both mean Sunday)
)


def _is_valid_cron_field(expr: str, lo: int, hi: int) -> bool:
    if not expr:
        return False
    for part in expr.split(","):
        part = part.strip()
        if not part:
            return False
        if "/" in part:
            pieces = part.split("/")
            if len(pieces) != 2 or not pieces[1].isdigit() or int(pieces[1]) < 1:
                return False
            base = pieces[0]
        else:
            base = part

        if base == "*":
            continue
        if "-" in base:
            bounds = base.split("-")
            if len(bounds) != 2 or not bounds[0].isdigit() or not bounds[1].isdigit():
                return False
            a, b = int(bounds[0]), int(bounds[1])
            if not (lo <= a <= hi and lo <= b <= hi and a <= b):
                return False
        elif base.isdigit():
            v = int(base)
            if not (lo <= v <= hi):
                return False
        else:
            return False
    return True


def _is_valid_schedule(expr: str) -> bool:
    expr = expr.strip()
    if re.fullmatch(r"([01]?\d|2[0-3]):[0-5]\d", expr):
        return True
    parts = expr.split()
    if len(parts) == 5:
        return all(
            _is_valid_cron_field(p, lo, hi)
            for p, (lo, hi) in zip(parts, _CRON_BOUNDS)
        )
    return False


def _strip_toml_comment(line: str) -> str:
    in_str = False
    quote_char = ""
    escaped = False
    for idx, ch in enumerate(line):
        if escaped:
            escaped = False
            continue
        if ch == "\\" and in_str and quote_char == '"':
            escaped = True
            continue
        if ch in ('"', "'"):
            if not in_str:
                in_str = True
                quote_char = ch
            elif ch == quote_char:
                in_str = False
        elif ch == "#" and not in_str:
            return line[:idx]
    return line


_TOML_SIMPLE_ESCAPES = {
    "b": "\b",
    "t": "\t",
    "n": "\n",
    "f": "\f",
    "r": "\r",
    '"': '"',
    "\\": "\\",
}


def _decode_toml_basic_string(raw: str) -> str:
    """Decode a TOML basic (double-quoted) string without Python unicode_escape quirks."""
    out: List[str] = []
    i = 0
    n = len(raw)
    while i < n:
        ch = raw[i]
        if ch == "\\":
            if i + 1 >= n:
                raise ValueError("Unterminated escape sequence in TOML string")
            nxt = raw[i + 1]
            if nxt in _TOML_SIMPLE_ESCAPES:
                out.append(_TOML_SIMPLE_ESCAPES[nxt])
                i += 2
            elif nxt == "u" and i + 6 <= n:
                cp = int(raw[i + 2 : i + 6], 16)
                if 0xD800 <= cp <= 0xDFFF:
                    raise ValueError("Invalid surrogate code point in TOML string")
                out.append(chr(cp))
                i += 6
            elif nxt == "U" and i + 10 <= n:
                cp = int(raw[i + 2 : i + 10], 16)
                if cp > 0x10FFFF or (0xD800 <= cp <= 0xDFFF):
                    raise ValueError("Invalid Unicode code point in TOML string")
                out.append(chr(cp))
                i += 10
            else:
                raise ValueError(f"Invalid TOML escape sequence '\\{nxt}'")
        elif ch == '"':
            raise ValueError("Unescaped quote inside TOML basic string")
        else:
            out.append(ch)
            i += 1
    return "".join(out)


def _parse_toml_value(raw: str) -> Any:
    raw = raw.strip()
    if raw == "true":
        return True
    if raw == "false":
        return False
    if raw.startswith('"') and raw.endswith('"') and len(raw) >= 2:
        return _decode_toml_basic_string(raw[1:-1])
    if raw.startswith("'") and raw.endswith("'") and len(raw) >= 2:
        return raw[1:-1]
    if raw.startswith("[") and raw.endswith("]"):
        inner = raw[1:-1].strip()
        if not inner:
            return []
        items: List[Any] = []
        buf: List[str] = []
        in_str = False
        q = ""
        escaped = False
        for ch in inner:
            if escaped:
                buf.append(ch)
                escaped = False
                continue
            if ch == "\\" and in_str and q == '"':
                buf.append(ch)
                escaped = True
                continue
            if ch in ('"', "'"):
                if not in_str:
                    in_str = True
                    q = ch
                elif ch == q:
                    in_str = False
            if ch == "," and not in_str:
                token = "".join(buf).strip()
                if token:
                    items.append(_parse_toml_value(token))
                buf = []
            else:
                buf.append(ch)
        tail = "".join(buf).strip()
        if tail:
            items.append(_parse_toml_value(tail))
        return items
    try:
        return int(raw.replace("_", ""))
    except ValueError:
        pass
    try:
        return float(raw.replace("_", ""))
    except ValueError:
        pass
    raise ValueError(f"Invalid TOML value: {raw!r}")


def _fallback_parse_toml(text: str) -> Dict[str, Any]:
    """Minimal pure-Python TOML parser for Python 3.9/3.10 compatibility."""
    root: Dict[str, Any] = {}
    current: Dict[str, Any] = root
    lines = text.splitlines()
    idx = 0
    while idx < len(lines):
        line = _strip_toml_comment(lines[idx]).strip()
        idx += 1
        if not line:
            continue
        if line.startswith("[[") and line.endswith("]]"):
            keys = [k.strip() for k in line[2:-2].split(".")]
            target = root
            for k in keys[:-1]:
                nxt = target.setdefault(k, {})
                if not isinstance(nxt, dict):
                    raise ValueError(f"TOML table conflict at '{k}'")
                target = nxt
            arr = target.setdefault(keys[-1], [])
            if not isinstance(arr, list):
                raise ValueError(f"TOML array-of-tables conflict at '{keys[-1]}'")
            new_table: Dict[str, Any] = {}
            arr.append(new_table)
            current = new_table
            continue
        if line.startswith("[") and line.endswith("]"):
            keys = [k.strip() for k in line[1:-1].split(".")]
            target = root
            for k in keys:
                nxt = target.setdefault(k, {})
                if not isinstance(nxt, dict):
                    raise ValueError(f"TOML table conflict at '{k}'")
                target = nxt
            current = target
            continue
        if "=" in line:
            key, val = line.split("=", 1)
            key = key.strip()
            val = val.strip()
            while val.startswith("[") and not val.endswith("]") and idx < len(lines):
                val += " " + _strip_toml_comment(lines[idx]).strip()
                idx += 1
            current[key] = _parse_toml_value(val)
            continue
        raise ValueError(f"Invalid TOML syntax: {line!r}")
    return root


def parse_toml(text: str) -> Dict[str, Any]:
    if tomllib is not None:
        return tomllib.loads(text)
    return _fallback_parse_toml(text)


def _coerce_and_check_type(section: str, key: str, current: Any, value: Any) -> Any:
    if isinstance(current, bool):
        if not isinstance(value, bool):
            raise ValueError(f"{section}.{key} must be a boolean")
        return value
    if isinstance(current, int):
        if isinstance(value, bool) or not isinstance(value, int):
            raise ValueError(f"{section}.{key} must be an integer")
        return value
    if isinstance(current, float):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError(f"{section}.{key} must be a number")
        return float(value)
    if isinstance(current, str):
        if not isinstance(value, str):
            raise ValueError(f"{section}.{key} must be a string")
        return value
    if isinstance(current, list):
        if not isinstance(value, list) or not all(isinstance(x, str) for x in value):
            raise ValueError(f"{section}.{key} must be a list of strings")
        return value
    return value


def _apply_dict(section: str, obj: Any, data: Dict[str, Any]) -> None:
    valid_fields = {f.name for f in fields(obj)}
    for key, value in data.items():
        if key not in valid_fields:
            raise ValueError(f"Unknown configuration key: {section}.{key}")
        current = getattr(obj, key)
        setattr(obj, key, _coerce_and_check_type(section, key, current, value))


def _build_watchdog_rule(raw_rule: Any) -> WatchdogRule:
    if not isinstance(raw_rule, dict):
        raise ValueError("Each [[watchdog.rules]] entry must be a table")
    allowed = {"name", "pattern", "max_rss_mb", "action", "grace_seconds", "allow_sigkill"}
    unknown = set(raw_rule.keys()) - allowed
    if unknown:
        raise ValueError(f"Unknown watchdog rule key(s): {', '.join(sorted(unknown))}")
    if "name" not in raw_rule or "pattern" not in raw_rule:
        raise ValueError("Each [[watchdog.rules]] entry requires 'name' and 'pattern'")
    if not isinstance(raw_rule["name"], str) or not isinstance(raw_rule["pattern"], str):
        raise ValueError("watchdog.rules 'name' and 'pattern' must be strings")
    rule = WatchdogRule(name=raw_rule["name"], pattern=raw_rule["pattern"])
    for k, v in raw_rule.items():
        if k in ("name", "pattern"):
            continue
        setattr(rule, k, _coerce_and_check_type("watchdog.rules", k, getattr(rule, k), v))
    return rule


def _verify_root_config_security(path: Path) -> None:
    """When running as root, reject symlinked or non-root/writable config files and ancestors."""
    abs_path = Path(os.path.abspath(str(path)))
    st = os.lstat(abs_path)
    if not stat.S_ISREG(st.st_mode):
        raise PermissionError(f"Refusing to load non-regular config file as root: {path}")
    if st.st_uid != 0:
        raise PermissionError(
            f"Refusing to load config file not owned by root (uid={st.st_uid}): {path}"
        )
    if st.st_mode & 0o022:
        raise PermissionError(
            f"Refusing to load group/world-writable config file as root: {path}"
        )
    # Verify every ancestor directory up to /
    for ancestor in abs_path.parents:
        ast = os.lstat(ancestor)
        if not stat.S_ISDIR(ast.st_mode):
            raise PermissionError(
                f"Refusing to load config through non-directory ancestor: {ancestor}"
            )
        if ast.st_uid != 0 or (ast.st_mode & 0o022):
            raise PermissionError(
                f"Refusing to load config from non-root or group/world-writable ancestor: {ancestor}"
            )


def _read_config_text_secure(path: Path, is_root: bool) -> str:
    """Read config via O_NOFOLLOW FD (verifying fstat when root) to prevent TOCTOU."""
    if is_root:
        _verify_root_config_security(path)
    fd = os.open(str(path), os.O_RDONLY | os.O_NOFOLLOW)
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode):
            raise PermissionError(f"Refusing to load non-regular config file: {path}")
        if is_root and (st.st_uid != 0 or (st.st_mode & 0o022)):
            raise PermissionError(f"Config file permissions changed unsafely: {path}")
        with os.fdopen(fd, "r", encoding="utf-8") as fh:
            fd = -1
            return fh.read()
    finally:
        if fd >= 0:
            os.close(fd)


def load_config(path: Optional[Path] = None) -> Config:
    """Load configuration from explicit path, user config (non-root), or system config."""
    cfg = Config()
    chosen: Optional[Path] = None
    is_root = os.geteuid() == 0

    if path is not None:
        chosen = path.expanduser()
        if not chosen.exists() and not chosen.is_symlink():
            raise FileNotFoundError(f"Configuration file not found: {chosen}")
    elif is_root:
        if DEFAULT_SYSTEM_CONFIG.exists() or DEFAULT_SYSTEM_CONFIG.is_symlink():
            chosen = DEFAULT_SYSTEM_CONFIG
    else:
        for candidate in (default_user_config(), DEFAULT_SYSTEM_CONFIG):
            if candidate.exists():
                chosen = candidate
                break

    if chosen is None:
        return cfg

    raw = parse_toml(_read_config_text_secure(chosen, is_root=is_root))
    cfg.source_path = chosen

    unknown_sections = set(raw.keys()) - KNOWN_SECTIONS
    if unknown_sections:
        raise ValueError(
            f"Unknown configuration section(s): {', '.join(sorted(unknown_sections))}"
        )

    for sec_name, target_obj in (
        ("triggers", cfg.triggers),
        ("tmpfs", cfg.tmpfs),
        ("user_caches", cfg.user_caches),
        ("kernel", cfg.kernel),
        ("swap", cfg.swap),
        ("notifications", cfg.notifications),
        ("logging", cfg.logging),
    ):
        if sec_name in raw:
            if not isinstance(raw[sec_name], dict):
                raise ValueError(f"Section [{sec_name}] must be a table")
            _apply_dict(sec_name, target_obj, raw[sec_name])

    if "watchdog" in raw:
        if not isinstance(raw["watchdog"], dict):
            raise ValueError("Section [watchdog] must be a table")
        wd_raw = dict(raw["watchdog"])
        rules_raw = wd_raw.pop("rules", [])
        _apply_dict("watchdog", cfg.watchdog, wd_raw)
        if not isinstance(rules_raw, list):
            raise ValueError("watchdog.rules must be an array of tables")
        cfg.watchdog.rules = [_build_watchdog_rule(r) for r in rules_raw]

    errors = cfg.validate()
    if errors:
        raise ValueError("Invalid configuration:\n  - " + "\n  - ".join(errors))

    return cfg


EXAMPLE_CONFIG_TOML = """\
# ==============================================================================
# keep-ram-clean configuration
# Locations searched automatically:
#   - Explicit:  --config /path/to/config.toml
#   - Root mode: /etc/keep-ram-clean/config.toml ONLY (must be root-owned, 0644/0600,
#                with root-owned non-writable parent directories; also enforced for --config)
#   - User mode: ~/.config/keep-ram-clean/config.toml, then /etc/keep-ram-clean/config.toml
# ==============================================================================

[triggers]
# How often the daemon samples /proc metrics (seconds)
poll_interval_seconds = 15

# Minimum wait between consecutive cleanups to prevent thrashing (seconds)
cooldown_seconds = 300

# Percentage drop required below threshold before a threshold trigger re-arms
hysteresis_percent = 5.0

# --- Reactive Thresholds (set any to 0 to disable) ---
# Trigger when (MemTotal - MemAvailable) / MemTotal >= ram_used_percent
ram_used_percent = 80.0

# Trigger when MemAvailable drops below this many MiB
min_available_mb = 1536

# Trigger when swap usage reaches this percentage or absolute MiB
swap_used_percent = 50.0
swap_used_mb = 0

# Trigger when any monitored tmpfs mount (e.g., /tmp) exceeds this % full
tmpfs_used_percent = 70.0

# Linux Pressure Stall Information (/proc/pressure/memory) triggers.
# Far more accurate than raw RAM % because Linux intentionally uses free RAM
# for page cache. 'some' = at least one task stalled; 'full' = all tasks stalled.
psi_some_avg10 = 10.0
psi_full_avg10 = 2.0

# --- Proactive Time-Based Triggers ---
# Run cleanup every N minutes regardless of pressure (0 = disabled)
interval_minutes = 0

# Specific wall-clock schedules: "HH:MM" (daily) or 5-field cron "M H DOM MON DOW"
# Examples: ["03:00"], ["0 */4 * * *"], ["30 2 * * 0"]
schedules = []

# Defer non-critical cleanups when 1-min load average exceeds ratio * CPU cores (0 = off)
max_load_avg_ratio = 0.0


[tmpfs]
# Clean stale ephemeral build/temp directories from RAM-backed tmpfs mounts
enabled = true
paths = ["/tmp"]

# Safety guard: only clean paths that are genuinely mounted as tmpfs
require_tmpfs = true

# Glob patterns inside each path eligible for cleanup
patterns = [
    "tmp*",
    "pytest-of-*",
    "npm-*",
    "pip-*",
    "cargo-*",
    "rustc*",
    "go-build*",
    "webpack-*",
    "vite-*",
]

# Patterns that are NEVER deleted even if they match above
exclude_patterns = [
    ".X11-unix",
    ".ICE-unix",
    ".XIM-unix",
    ".font-unix",
    "systemd-private-*",
    "ssh-*",
    "tmux-*",
    "keep-ram-clean-*",
]

# Only remove items not modified in at least this many minutes
min_age_minutes = 30

# When running non-root, only remove items owned by the current user
owner_only = true

# Never delete files/directories currently held open by any running process
skip_open_files = true


[watchdog]
# Optional runaway-process watchdog (disabled by default for safety).
# Actions: "notify", "sigterm", "graceful" (SIGTERM then optional SIGKILL), "sigkill"
enabled = false

# Example: gracefully stop KDE Baloo if it balloons past 1.5 GiB RSS
# [[watchdog.rules]]
# name = "baloo-indexer"
# pattern = "baloo_file(_extractor)?"
# max_rss_mb = 1536
# action = "graceful"
# grace_seconds = 10
# allow_sigkill = false

# Example: alert on GNOME tracker miner exceeding 1 GiB
# [[watchdog.rules]]
# name = "tracker-miner"
# pattern = "tracker-miner-fs"
# max_rss_mb = 1024
# action = "notify"


[user_caches]
# Optional disk cache hygiene (disabled by default; disk caches do not use RAM
# unless ~/.cache is mounted on tmpfs)
enabled = false
clean_thumbnails = true
thumbnail_max_age_days = 14
clean_pip_cache = false
extra_cache_dirs = []
extra_cache_max_age_days = 14
# When running as root, specify usernames whose caches may be cleaned.
# If empty under systemd (no SUDO_USER), user_caches is safely skipped (never cleans /root).
target_users = []


[kernel]
# Kernel-level memory reclamation (requires root; skipped cleanly in user mode)
enabled = true

# Prefer modern Linux 6.1+ root cgroup v2 proactive reclaim (/sys/fs/cgroup/memory.reclaim)
# which gently ages out cold pages without causing cold-cache I/O storms
use_cgroup_reclaim = true
cgroup_reclaim_mb = 512

# /proc/sys/vm/drop_caches level:
#   0 = disabled
#   1 = pagecache only
#   2 = reclaimable slab objects (dentries & inodes) [recommended default]
#   3 = pagecache + slab (aggressive; causes brief cold-cache slowdown)
drop_caches_level = 2

# Only invoke drop_caches when a critical pressure trigger fires (recommended with cgroup reclaim)
drop_caches_on_critical_only = true

# Trigger /proc/sys/vm/compact_memory (off by default; Linux kcompactd runs asynchronously)
compact_memory = false


[swap]
# Smart swap flushing (requires root; skipped cleanly in user mode)
enabled = true

# Only flush a disk swap device if it holds at least this many MiB
min_swap_used_mb = 128

# Require Available RAM >= SwapUsed + safety_margin_mb before flushing
safety_margin_mb = 1536

# Skip zram devices (flushing compressed RAM back into RAM is counterproductive)
skip_zram = true

# Skip swap flush if 1-min load average exceeds ratio * CPU cores
max_load_avg_ratio = 1.5


[notifications]
# Desktop notifications via notify-send (user mode only; skipped when running as root)
enabled = false
min_freed_mb = 100
on_critical_only = false


[logging]
level = "INFO"
# Optional rotating log file path (empty = log to stderr / systemd journal).
# Under systemd system mode, /var/log/keep-ram-clean/ is pre-provisioned via LogsDirectory=.
file = ""
max_bytes = 5242880
backup_count = 3
"""
