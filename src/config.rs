//! Configuration schema, TOML loading, strict validation, and root trust checks.

use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

pub const DEFAULT_SYSTEM_CONFIG: &str = "/etc/keep-ram-clean/config.toml";
pub const MAX_WATCHDOG_PATTERN_LEN: usize = 256;
pub const EXAMPLE_CONFIG_TOML: &str = include_str!("../config/config.example.toml");

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

pub fn default_user_config() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("keep-ram-clean").join("config.toml")
}

pub fn default_runtime_dir() -> PathBuf {
    if is_root() {
        return PathBuf::from("/run/keep-ram-clean");
    }
    if let Some(rt) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return rt.join("keep-ram-clean");
    }
    let state_base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".local").join("state"));
    state_base.join("keep-ram-clean")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TriggerConfig {
    pub poll_interval_seconds: u64,
    pub cooldown_seconds: u64,
    pub hysteresis_percent: f64,

    pub ram_used_percent: f64,
    pub min_available_mb: u64,
    pub swap_used_percent: f64,
    pub swap_used_mb: u64,
    pub tmpfs_used_percent: f64,

    pub psi_some_avg10: f64,
    pub psi_full_avg10: f64,

    pub interval_minutes: u64,
    pub schedules: Vec<String>,

    pub max_load_avg_ratio: f64,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 15,
            cooldown_seconds: 300,
            hysteresis_percent: 5.0,
            ram_used_percent: 80.0,
            min_available_mb: 1536,
            swap_used_percent: 50.0,
            swap_used_mb: 0,
            tmpfs_used_percent: 70.0,
            psi_some_avg10: 10.0,
            psi_full_avg10: 2.0,
            interval_minutes: 0,
            schedules: Vec::new(),
            max_load_avg_ratio: 0.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TmpfsConfig {
    pub enabled: bool,
    pub paths: Vec<String>,
    pub require_tmpfs: bool,
    pub patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub min_age_minutes: u64,
    pub owner_only: bool,
    pub skip_open_files: bool,
}

impl Default for TmpfsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: vec!["/tmp".to_string()],
            require_tmpfs: true,
            patterns: vec![
                "tmp*".into(),
                "pytest-of-*".into(),
                "npm-*".into(),
                "pip-*".into(),
                "cargo-*".into(),
                "rustc*".into(),
                "go-build*".into(),
                "webpack-*".into(),
                "vite-*".into(),
            ],
            exclude_patterns: vec![
                ".X11-unix".into(),
                ".ICE-unix".into(),
                ".XIM-unix".into(),
                ".font-unix".into(),
                "systemd-private-*".into(),
                "ssh-*".into(),
                "tmux-*".into(),
                "keep-ram-clean-*".into(),
            ],
            min_age_minutes: 30,
            owner_only: true,
            skip_open_files: true,
        }
    }
}

fn default_max_rss_mb() -> u64 {
    1024
}
fn default_action() -> String {
    "graceful".to_string()
}
fn default_grace_seconds() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchdogRule {
    pub name: String,
    pub pattern: String,
    #[serde(default = "default_max_rss_mb")]
    pub max_rss_mb: u64,
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default = "default_grace_seconds")]
    pub grace_seconds: u64,
    #[serde(default)]
    pub allow_sigkill: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchdogConfig {
    pub enabled: bool,
    pub rules: Vec<WatchdogRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserCacheConfig {
    pub enabled: bool,
    pub clean_thumbnails: bool,
    pub thumbnail_max_age_days: u64,
    pub clean_pip_cache: bool,
    pub extra_cache_dirs: Vec<String>,
    pub extra_cache_max_age_days: u64,
    pub target_users: Vec<String>,
}

impl Default for UserCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            clean_thumbnails: true,
            thumbnail_max_age_days: 14,
            clean_pip_cache: false,
            extra_cache_dirs: Vec::new(),
            extra_cache_max_age_days: 14,
            target_users: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig {
    pub enabled: bool,
    pub use_cgroup_reclaim: bool,
    pub cgroup_reclaim_mb: u64,
    pub drop_caches_level: u8,
    pub drop_caches_on_critical_only: bool,
    pub compact_memory: bool,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            use_cgroup_reclaim: true,
            cgroup_reclaim_mb: 512,
            drop_caches_level: 2,
            drop_caches_on_critical_only: true,
            compact_memory: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwapConfig {
    pub enabled: bool,
    pub min_swap_used_mb: u64,
    pub safety_margin_mb: u64,
    pub skip_zram: bool,
    pub max_load_avg_ratio: f64,
}

impl Default for SwapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_swap_used_mb: 128,
            safety_margin_mb: 1536,
            skip_zram: true,
            max_load_avg_ratio: 1.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub min_freed_mb: i64,
    pub on_critical_only: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_freed_mb: 100,
            on_critical_only: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub file: String,
    pub max_bytes: u64,
    pub backup_count: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "INFO".to_string(),
            file: String::new(),
            max_bytes: 5 * 1024 * 1024,
            backup_count: 3,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub triggers: TriggerConfig,
    pub tmpfs: TmpfsConfig,
    pub watchdog: WatchdogConfig,
    pub user_caches: UserCacheConfig,
    pub kernel: KernelConfig,
    pub swap: SwapConfig,
    pub notifications: NotificationConfig,
    pub logging: LoggingConfig,
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

fn in_finite_range(val: f64, lo: f64, hi: f64) -> bool {
    val.is_finite() && val >= lo && val <= hi
}

const CRON_BOUNDS: [(u32, u32); 5] = [
    (0, 59), // minute
    (0, 23), // hour
    (1, 31), // day of month
    (1, 12), // month
    (0, 7),  // day of week (0 and 7 both mean Sunday)
];

fn is_valid_cron_field(expr: &str, lo: u32, hi: u32) -> bool {
    if expr.is_empty() {
        return false;
    }
    for part in expr.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        let base = if let Some((b, step_s)) = part.split_once('/') {
            if step_s.contains('/') {
                return false;
            }
            match step_s.parse::<u32>() {
                Ok(step) if step >= 1 => b,
                _ => return false,
            }
        } else {
            part
        };

        if base == "*" {
            continue;
        }
        if let Some((a_s, b_s)) = base.split_once('-') {
            if b_s.contains('-') {
                return false;
            }
            match (a_s.parse::<u32>(), b_s.parse::<u32>()) {
                (Ok(a), Ok(b)) if a >= lo && a <= hi && b >= lo && b <= hi && a <= b => {}
                _ => return false,
            }
        } else {
            match base.parse::<u32>() {
                Ok(v) if v >= lo && v <= hi => {}
                _ => return false,
            }
        }
    }
    true
}

pub fn is_valid_schedule(expr: &str) -> bool {
    let expr = expr.trim();
    if let Some((hh_s, mm_s)) = expr.split_once(':') {
        if !mm_s.contains(':') && !expr.contains(' ') {
            if let (Ok(hh), Ok(mm)) = (hh_s.parse::<u32>(), mm_s.parse::<u32>()) {
                return hh <= 23 && mm <= 59 && mm_s.len() == 2;
            }
        }
        return false;
    }
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() == 5 {
        return parts
            .iter()
            .zip(CRON_BOUNDS.iter())
            .all(|(p, &(lo, hi))| is_valid_cron_field(p, lo, hi));
    }
    false
}

impl Config {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let t = &self.triggers;
        if t.poll_interval_seconds < 1 {
            errors.push("triggers.poll_interval_seconds must be >= 1".into());
        }
        if !in_finite_range(t.hysteresis_percent, 0.0, 50.0) {
            errors.push("triggers.hysteresis_percent must be a finite number between 0 and 50".into());
        }
        if !in_finite_range(t.ram_used_percent, 0.0, 100.0) {
            errors.push("triggers.ram_used_percent must be a finite number between 0 and 100".into());
        }
        if !in_finite_range(t.swap_used_percent, 0.0, 100.0) {
            errors.push("triggers.swap_used_percent must be a finite number between 0 and 100".into());
        }
        if !in_finite_range(t.tmpfs_used_percent, 0.0, 100.0) {
            errors.push("triggers.tmpfs_used_percent must be a finite number between 0 and 100".into());
        }
        if !in_finite_range(t.psi_some_avg10, 0.0, 100.0) {
            errors.push("triggers.psi_some_avg10 must be a finite number between 0 and 100".into());
        }
        if !in_finite_range(t.psi_full_avg10, 0.0, 100.0) {
            errors.push("triggers.psi_full_avg10 must be a finite number between 0 and 100".into());
        }
        if !(t.max_load_avg_ratio.is_finite() && t.max_load_avg_ratio >= 0.0) {
            errors.push("triggers.max_load_avg_ratio must be a finite number >= 0".into());
        }
        if self.kernel.drop_caches_level > 3 {
            errors.push("kernel.drop_caches_level must be 0, 1, 2, or 3".into());
        }
        if !(self.swap.max_load_avg_ratio.is_finite() && self.swap.max_load_avg_ratio >= 0.0) {
            errors.push("swap.max_load_avg_ratio must be a finite number >= 0".into());
        }
        for p in &self.tmpfs.paths {
            let path = Path::new(p);
            if !path.is_absolute() || path == Path::new("/") {
                errors.push(format!("tmpfs.paths: unsafe or non-absolute path '{p}'"));
            }
        }
        for extra in &self.user_caches.extra_cache_dirs {
            let stripped = extra.trim();
            let rel = stripped.strip_prefix("~/").unwrap_or(stripped);
            let mut has_normal = false;
            let mut has_parent = false;
            for c in Path::new(rel).components() {
                match c {
                    Component::Normal(_) => has_normal = true,
                    Component::ParentDir => has_parent = true,
                    _ => {}
                }
            }
            if stripped.is_empty()
                || matches!(stripped, "~" | "~/" | "." | "/" | "..")
                || !has_normal
                || has_parent
            {
                errors.push(format!(
                    "user_caches.extra_cache_dirs: unsafe cache directory '{extra}'"
                ));
            }
        }
        if self.logging.max_bytes < 1024 {
            errors.push("logging.max_bytes must be >= 1024".into());
        }
        for rule in &self.watchdog.rules {
            if !matches!(rule.action.as_str(), "notify" | "sigterm" | "graceful" | "sigkill") {
                errors.push(format!(
                    "watchdog rule '{}': invalid action '{}'",
                    rule.name, rule.action
                ));
            }
            if rule.max_rss_mb < 1 {
                errors.push(format!("watchdog rule '{}': max_rss_mb must be >= 1", rule.name));
            }
            if rule.pattern.is_empty() || rule.pattern.len() > MAX_WATCHDOG_PATTERN_LEN {
                errors.push(format!(
                    "watchdog rule '{}': pattern length must be 1..{}",
                    rule.name, MAX_WATCHDOG_PATTERN_LEN
                ));
            } else if let Err(e) = Regex::new(&rule.pattern) {
                errors.push(format!(
                    "watchdog rule '{}': invalid regex '{}': {}",
                    rule.name, rule.pattern, e
                ));
            }
        }
        for sched in &t.schedules {
            if !is_valid_schedule(sched) {
                errors.push(format!(
                    "triggers.schedules: invalid entry '{sched}' (expected 'HH:MM' or 5-field cron)"
                ));
            }
        }
        errors
    }
}

pub fn verify_root_config_security(path: &Path) -> Result<(), String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| format!("Cannot resolve current dir: {e}"))?
            .join(path)
    };
    let meta = fs::symlink_metadata(&abs)
        .map_err(|e| format!("Cannot lstat config {}: {e}", abs.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "Refusing to load non-regular config file as root: {}",
            path.display()
        ));
    }
    if meta.uid() != 0 {
        return Err(format!(
            "Refusing to load config file not owned by root (uid={}): {}",
            meta.uid(),
            path.display()
        ));
    }
    if (meta.mode() & 0o022) != 0 {
        return Err(format!(
            "Refusing to load group/world-writable config file as root: {}",
            path.display()
        ));
    }
    for ancestor in abs.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        let ameta = fs::symlink_metadata(ancestor)
            .map_err(|e| format!("Cannot lstat ancestor {}: {e}", ancestor.display()))?;
        if !ameta.file_type().is_dir() {
            return Err(format!(
                "Refusing to load config through non-directory ancestor: {}",
                ancestor.display()
            ));
        }
        if ameta.uid() != 0 || (ameta.mode() & 0o022) != 0 {
            return Err(format!(
                "Refusing to load config from non-root or group/world-writable ancestor: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn read_config_text_secure(path: &Path, running_as_root: bool) -> Result<String, String> {
    if running_as_root {
        verify_root_config_security(path)?;
    }
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("Cannot open config {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("Cannot fstat config {}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!("Refusing to load non-regular config file: {}", path.display()));
    }
    if running_as_root && (meta.uid() != 0 || (meta.mode() & 0o022) != 0) {
        return Err(format!(
            "Config file permissions changed unsafely: {}",
            path.display()
        ));
    }
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .map_err(|e| format!("Cannot read config {}: {e}", path.display()))?;
    Ok(buf)
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(s) = path.to_str().ok_or(()) {
        if let Some(rest) = s.strip_prefix("~/") {
            return home_dir().join(rest);
        }
        if s == "~" {
            return home_dir();
        }
    }
    path.to_path_buf()
}

pub fn load_config(explicit: Option<&Path>) -> Result<Config, String> {
    let root = is_root();
    let chosen: Option<PathBuf> = if let Some(p) = explicit {
        let expanded = expand_tilde(p);
        if fs::symlink_metadata(&expanded).is_err() {
            return Err(format!("Configuration file not found: {}", expanded.display()));
        }
        Some(expanded)
    } else if root {
        let sys = PathBuf::from(DEFAULT_SYSTEM_CONFIG);
        if fs::symlink_metadata(&sys).is_ok() {
            Some(sys)
        } else {
            None
        }
    } else {
        let usr = default_user_config();
        let sys = PathBuf::from(DEFAULT_SYSTEM_CONFIG);
        if usr.exists() {
            Some(usr)
        } else if sys.exists() {
            Some(sys)
        } else {
            None
        }
    };

    let Some(path) = chosen else {
        return Ok(Config::default());
    };

    let text = read_config_text_secure(&path, root)?;
    let mut cfg: Config = toml::from_str(&text)
        .map_err(|e| format!("Failed parsing {}: {e}", path.display()))?;
    cfg.source_path = Some(path);

    let errors = cfg.validate();
    if !errors.is_empty() {
        return Err(format!("Invalid configuration:\n  - {}", errors.join("\n  - ")));
    }
    Ok(cfg)
}

/// Map friendly CLI setting names to `(section, key, toml_value_kind)`.
pub fn resolve_setting_alias(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match name.trim().to_ascii_lowercase().as_str() {
        "ram-percent" | "triggers.ram_used_percent" => Some(("triggers", "ram_used_percent", "float")),
        "min-avail-mb" | "triggers.min_available_mb" => Some(("triggers", "min_available_mb", "int")),
        "swap-percent" | "triggers.swap_used_percent" => Some(("triggers", "swap_used_percent", "float")),
        "swap-mb" | "triggers.swap_used_mb" => Some(("triggers", "swap_used_mb", "int")),
        "tmpfs-percent" | "triggers.tmpfs_used_percent" => Some(("triggers", "tmpfs_used_percent", "float")),
        "psi-some" | "triggers.psi_some_avg10" => Some(("triggers", "psi_some_avg10", "float")),
        "psi-full" | "triggers.psi_full_avg10" => Some(("triggers", "psi_full_avg10", "float")),
        "cooldown" | "triggers.cooldown_seconds" => Some(("triggers", "cooldown_seconds", "int")),
        "poll" | "triggers.poll_interval_seconds" => Some(("triggers", "poll_interval_seconds", "int")),
        "hysteresis" | "triggers.hysteresis_percent" => Some(("triggers", "hysteresis_percent", "float")),
        "interval" | "triggers.interval_minutes" => Some(("triggers", "interval_minutes", "int")),
        "load-limit" | "triggers.max_load_avg_ratio" => Some(("triggers", "max_load_avg_ratio", "float")),
        "notify" | "notifications.enabled" => Some(("notifications", "enabled", "bool")),
        "notify-min-mb" | "notifications.min_freed_mb" => Some(("notifications", "min_freed_mb", "int")),
        "notify-critical-only" | "notifications.on_critical_only" => Some(("notifications", "on_critical_only", "bool")),
        "drop-caches" | "kernel.drop_caches_level" => Some(("kernel", "drop_caches_level", "int")),
        "drop-caches-critical-only" | "kernel.drop_caches_on_critical_only" => {
            Some(("kernel", "drop_caches_on_critical_only", "bool"))
        }
        "cgroup-reclaim-mb" | "kernel.cgroup_reclaim_mb" => Some(("kernel", "cgroup_reclaim_mb", "int")),
        "tmpfs-age" | "tmpfs.min_age_minutes" => Some(("tmpfs", "min_age_minutes", "int")),
        _ => None,
    }
}

fn format_toml_scalar(kind: &str, raw_val: &str) -> Result<String, String> {
    let v = raw_val.trim();
    match kind {
        "bool" => match v.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Ok("true".into()),
            "false" | "off" | "no" | "0" => Ok("false".into()),
            _ => Err(format!("Expected boolean (true/false/on/off), got '{raw_val}'")),
        },
        "int" => {
            let n: i64 = v
                .parse()
                .map_err(|_| format!("Expected integer, got '{raw_val}'"))?;
            Ok(n.to_string())
        }
        "float" => {
            let f: f64 = v
                .parse()
                .map_err(|_| format!("Expected number, got '{raw_val}'"))?;
            if !f.is_finite() {
                return Err(format!("Expected finite number, got '{raw_val}'"));
            }
            if f.fract() == 0.0 {
                Ok(format!("{f:.1}"))
            } else {
                Ok(format!("{f}"))
            }
        }
        _ => Err(format!("Unsupported setting type '{kind}'")),
    }
}

fn patch_toml_key(text: &str, section: &str, key: &str, formatted_val: &str) -> String {
    let target_header = format!("[{section}]");
    let mut out = Vec::new();
    let mut in_section = false;
    let mut section_found = false;
    let mut replaced = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            if in_section && !replaced {
                out.push(format!("{key} = {formatted_val}"));
                replaced = true;
            }
            in_section = trimmed == target_header;
            if in_section {
                section_found = true;
            }
            out.push(line.to_string());
            continue;
        }
        if in_section && !replaced && !trimmed.starts_with('#') {
            if let Some((lhs, _)) = trimmed.split_once('=') {
                if lhs.trim() == key {
                    out.push(format!("{key} = {formatted_val}"));
                    replaced = true;
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }

    if in_section && !replaced {
        out.push(format!("{key} = {formatted_val}"));
        replaced = true;
    }
    if !section_found && !replaced {
        out.push(String::new());
        out.push(target_header);
        out.push(format!("{key} = {formatted_val}"));
    }

    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

pub fn active_config_write_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        expand_tilde(p)
    } else if is_root() {
        PathBuf::from(DEFAULT_SYSTEM_CONFIG)
    } else {
        default_user_config()
    }
}

/// Update a single setting in config.toml (creating from defaults if absent),
/// validate the resulting TOML, and atomically write it back.
pub fn update_config_setting(
    explicit: Option<&Path>,
    setting_name: &str,
    raw_value: &str,
) -> Result<(PathBuf, Config, String, String), String> {
    let Some((section, key, kind)) = resolve_setting_alias(setting_name) else {
        return Err(format!(
            "Unknown setting '{setting_name}'. Try: ram-percent, min-avail-mb, swap-percent, \
             tmpfs-percent, psi-some, psi-full, cooldown, interval, notify, notify-min-mb, \
             notify-critical-only, drop-caches, cgroup-reclaim-mb"
        ));
    };
    let formatted = format_toml_scalar(kind, raw_value)?;
    let dest = active_config_write_path(explicit);
    let root = is_root();

    let existing = if fs::symlink_metadata(&dest).is_ok() {
        read_config_text_secure(&dest, root)?
    } else {
        EXAMPLE_CONFIG_TOML.to_string()
    };

    let patched = patch_toml_key(&existing, section, key, &formatted);
    let mut cfg: Config = toml::from_str(&patched)
        .map_err(|e| format!("Updated config failed to parse: {e}"))?;
    let errs = cfg.validate();
    if !errs.is_empty() {
        return Err(format!("Validation failed:\n  - {}", errs.join("\n  - ")));
    }

    if let Some(parent) = dest.parent() {
        let dir_mode = if root { 0o755 } else { 0o700 };
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(dir_mode));
    }

    let file_mode = if root { 0o644 } else { 0o600 };
    let tmp = dest.with_extension(format!("toml.{}.tmp", std::process::id()));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(file_mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|e| format!("Cannot create temp config {}: {e}", tmp.display()))?;
    if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(file_mode))
        .and_then(|()| file.write_all(patched.as_bytes()))
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Failed writing config: {e}"));
    }
    drop(file);
    if let Err(e) = fs::rename(&tmp, &dest) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Failed replacing {}: {e}", dest.display()));
    }

    cfg.source_path = Some(dest.clone());
    Ok((dest, cfg, format!("{section}.{key}"), formatted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_and_validates() {
        let cfg: Config = toml::from_str(EXAMPLE_CONFIG_TOML).expect("example TOML must parse");
        assert!(cfg.validate().is_empty());
        assert_eq!(cfg.triggers.ram_used_percent, 80.0);
        assert_eq!(cfg.kernel.drop_caches_level, 2);
        assert!(cfg.swap.skip_zram);
    }

    #[test]
    fn rejects_unknown_sections_and_bad_values() {
        let bad = "[trigger]\nram_used_percent = 80.0\n";
        assert!(toml::from_str::<Config>(bad).is_err());

        let mut cfg = Config::default();
        cfg.triggers.schedules = vec!["99 99 99 99 99".into(), "*/0 * * * *".into()];
        cfg.tmpfs.paths = vec!["/".into()];
        cfg.user_caches.extra_cache_dirs = vec!["~/".into(), "".into(), ".".into()];
        cfg.triggers.max_load_avg_ratio = f64::NAN;
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("99 99")));
        assert!(errs.iter().any(|e| e.contains("*/0")));
        assert!(errs.iter().any(|e| e.contains("tmpfs.paths")));
        assert!(errs.iter().any(|e| e.contains("extra_cache_dirs")));
        assert!(errs.iter().any(|e| e.contains("max_load_avg_ratio")));
    }

    #[test]
    fn root_config_security_rejects_symlink() {
        let dir = std::env::temp_dir().join(format!("krc-test-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.toml");
        let link = dir.join("link.toml");
        fs::write(&real, "[triggers]\nram_used_percent = 80.0\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(verify_root_config_security(&link).is_err());
        if !is_root() {
            assert!(verify_root_config_security(&real).is_err());
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
