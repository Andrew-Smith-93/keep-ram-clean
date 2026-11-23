//! Keyboard editor nested in Watch Settings. Its only write is an explicit transaction.
use crate::{config::WatchConfigDraft, Config};

pub(crate) const FIELDS: [(&str, &str); 19] = [
    ("ram-percent", "%"),
    ("min-avail-mb", "MiB"),
    ("swap-percent", "%"),
    ("swap-mb", "MiB"),
    ("tmpfs-percent", "%"),
    ("psi-some", "% avg10"),
    ("psi-full", "% avg10"),
    ("cooldown", "s"),
    ("poll", "s"),
    ("hysteresis", "pp"),
    ("interval", "min"),
    ("load-limit", "ratio"),
    ("notify", "true/false"),
    ("notify-min-mb", "MiB"),
    ("notify-critical-only", "true/false"),
    ("drop-caches", "level 0-3"),
    ("drop-caches-critical-only", "true/false"),
    ("cgroup-reclaim-mb", "MiB"),
    ("tmpfs-age", "min"),
];

pub(crate) const LABELS: [&str; FIELDS.len()] = [
    "RAM used",
    "Available floor",
    "Swap used %",
    "Swap used MiB",
    "tmpfs used",
    "PSI some avg10",
    "PSI full avg10",
    "Cooldown",
    "Poll interval",
    "Re-arm margin",
    "Periodic interval",
    "CPU load limit",
    "Desktop notices",
    "Free before notice",
    "Critical-only notice",
    "Drop caches",
    "Drop critical-only",
    "Cgroup reclaim",
    "Minimum tmpfs age",
];

pub(crate) const GROUPS: [(&str, &[usize]); 4] = [
    ("PRESSURE THRESHOLDS", &[0, 1, 2, 3, 4, 5, 6]),
    ("NOTIFICATIONS", &[12, 13, 14]),
    ("TIMING & LOAD", &[7, 8, 9, 10, 11]),
    ("CLEANUP ACTIONS", &[15, 16, 17, 18]),
];

const GRID_ORDER: [usize; FIELDS.len()] = [
    0, 1, 2, 3, 4, 5, 6, 12, 13, 14, 7, 8, 9, 10, 11, 15, 16, 17, 18,
];

pub(crate) fn display_value(index: usize, raw: &str) -> String {
    match FIELDS[index].1 {
        "%" | "% avg10" => format!("{raw}%"),
        "true/false" => {
            if raw == "true" {
                "on".into()
            } else {
                "off".into()
            }
        }
        "level 0-3" => format!("level {raw}"),
        unit => format!("{raw} {unit}"),
    }
}

pub(crate) fn values(c: &Config) -> [String; FIELDS.len()] {
    let t = &c.triggers;
    [
        t.ram_used_percent.to_string(),
        t.min_available_mb.to_string(),
        t.swap_used_percent.to_string(),
        t.swap_used_mb.to_string(),
        t.tmpfs_used_percent.to_string(),
        t.psi_some_avg10.to_string(),
        t.psi_full_avg10.to_string(),
        t.cooldown_seconds.to_string(),
        t.poll_interval_seconds.to_string(),
        t.hysteresis_percent.to_string(),
        t.interval_minutes.to_string(),
        t.max_load_avg_ratio.to_string(),
        c.notifications.enabled.to_string(),
        c.notifications.min_freed_mb.to_string(),
        c.notifications.on_critical_only.to_string(),
        c.kernel.drop_caches_level.to_string(),
        c.kernel.drop_caches_on_critical_only.to_string(),
        c.kernel.cgroup_reclaim_mb.to_string(),
        c.tmpfs.min_age_minutes.to_string(),
    ]
}

pub(crate) const FLOOR: usize = 1;

#[derive(Clone, Copy)]
pub(crate) struct SystemPreset {
    pub(crate) ram_mib: u64,
    pub(crate) floor_mib: u64,
}

impl SystemPreset {
    pub(crate) fn from_ram_mib(ram_mib: u64) -> Option<Self> {
        if ram_mib < 128 {
            return None;
        }
        // Round 4% of physical RAM up to a 64 MiB boundary; never raise the
        // trigger above the shipping default or use current memory pressure.
        let cap = crate::config::TriggerConfig::default().min_available_mb;
        let floor_mib = ram_mib.div_ceil(1600).min(cap.div_ceil(64)) * 64;
        Some(Self {
            ram_mib,
            floor_mib: floor_mib.clamp(128, cap),
        })
    }

    pub(crate) fn detect() -> Option<Self> {
        #[cfg(test)]
        if let Ok(fixture) = std::env::var("KRC_WATCH_PRESET_RAM_MIB") {
            return Self::from_ram_mib(fixture.parse().ok()?);
        }
        let pages = u64::try_from(unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) }).ok()?;
        let size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        Self::from_ram_mib(pages.checked_mul(size)? / (1024 * 1024))
    }
}

#[derive(Clone, Copy)]
enum EditSource {
    Preset { system: bool },
    Manual,
}

pub(crate) struct Choice {
    pub(crate) raw: String,
    pub(crate) system: bool,
}

// Common choices (not policy): "on" is never applied simply by opening this
// picker, and each selection still goes through full config validation.
fn field_choices(index: usize, machine: Option<SystemPreset>) -> Vec<Choice> {
    const MENU: [&[&str]; FIELDS.len()] = [
        &["0", "70", "75", "80", "85", "90", "95"],  // RAM used %
        &["0", "128", "256", "512", "1024", "1536"], // available floor
        &["0", "25", "50", "65", "75", "90"],        // swap used %
        &["0", "128", "256", "512", "1024", "2048"], // swap used MiB
        &["0", "50", "60", "70", "80", "90"],        // tmpfs used %
        &["0", "2", "5", "10", "15", "20"],          // PSI some
        &["0", "0.5", "1", "2", "5", "10"],          // PSI full
        &["0", "30", "60", "120", "300", "600"],     // cooldown
        &["1", "5", "10", "15", "30", "60"],         // poll (never zero)
        &["0", "2", "5", "10", "20", "50"],          // re-arm margin <= 50
        &["0", "5", "15", "30", "60", "120"],        // periodic
        &["0", "0.5", "1", "1.5", "2"],              // load ratio
        &["false", "true"],                          // desktop notices
        &["0", "50", "100", "256", "512"],           // free before notice
        &["false", "true"],                          // notices critical-only
        &["0", "1", "2", "3"],                       // drop-caches valid levels
        &["false", "true"],                          // drop-caches critical-only
        &["0", "128", "256", "512", "1024", "2048"], // cgroup reclaim
        &["0", "5", "15", "30", "60", "120"],        // tmpfs min age
    ];
    let mut out: Vec<_> = MENU[index]
        .iter()
        .map(|s| Choice {
            raw: (*s).into(),
            system: false,
        })
        .collect();
    if let Some(machine) = machine.filter(|_| index == FLOOR) {
        let raw = machine.floor_mib.to_string();
        if let Some(choice) = out.iter_mut().find(|choice| choice.raw == raw) {
            choice.system = true;
        } else {
            let at = out
                .partition_point(|choice| choice.raw.parse::<u64>().unwrap() < machine.floor_mib);
            out.insert(at, Choice { raw, system: true });
        }
    }
    out
}

pub(crate) fn source(cfg: &Config, index: usize, editor: Option<&Editor>) -> char {
    let key = FIELDS[index].0;
    if let Some(editor) = editor.filter(|editor| editor.draft.has_staged(key)) {
        return match editor.staged_source[index] {
            Some(EditSource::Preset { .. }) => 'P',
            _ => 'M',
        };
    }
    if cfg.setting_explicit(key) {
        'F'
    } else {
        'B'
    }
}

#[derive(PartialEq, Eq)]
enum Mode {
    Browse,
    Presets,
    Manual,
    Confirm,
}

pub(crate) enum Outcome {
    Ignore,
    Continue,
    Cancel,
    Quit,
    Save,
}

pub(crate) struct Editor {
    pub(crate) draft: WatchConfigDraft,
    selected: usize,
    mode: Mode,
    buffer: String,
    review_page: usize,
    review_seen: usize,
    pub(crate) note: String,
    preset: Option<SystemPreset>,
    staged_source: [Option<EditSource>; FIELDS.len()],
    choices: Vec<Choice>,
    choice_index: usize,
}

impl Editor {
    pub(crate) fn open(cfg: &Config, preset: Option<SystemPreset>) -> Result<Self, String> {
        Ok(Self {
            draft: WatchConfigDraft::open(cfg.source_path.as_deref())?,
            selected: 0,
            mode: Mode::Browse,
            buffer: String::new(),
            review_page: 0,
            review_seen: 0,
            note: String::new(),
            preset,
            staged_source: [None; FIELDS.len()],
            choices: Vec::new(),
            choice_index: 0,
        })
    }

    fn changes(&self) -> Vec<usize> {
        FIELDS
            .iter()
            .enumerate()
            .filter_map(|(i, (alias, _))| self.draft.has_staged(alias).then_some(i))
            .collect()
    }

    fn open_presets(&mut self) {
        self.choices = field_choices(self.selected, self.preset);
        let key = FIELDS[self.selected].0;
        let current = &values(&self.draft.proposed)[self.selected];
        let system_first =
            !self.draft.original.setting_explicit(key) && !self.draft.has_staged(key);
        let exact = || {
            self.choices
                .iter()
                .position(|choice| &choice.raw == current)
        };
        self.choice_index = (if system_first {
            self.choices
                .iter()
                .position(|choice| choice.system)
                .or_else(exact)
        } else {
            exact()
        })
        .or_else(|| {
            let current: f64 = current.parse().ok()?;
            self.choices
                .iter()
                .enumerate()
                .filter_map(|(i, choice)| {
                    Some((i, (choice.raw.parse::<f64>().ok()? - current).abs()))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        })
        .unwrap_or(0);
        self.mode = Mode::Presets;
        self.note.clear();
    }

    fn stage_selected(&mut self, raw: &str, source: EditSource) {
        let key = FIELDS[self.selected].0;
        match self.draft.stage(key, raw) {
            Ok(()) => {
                let unchanged = self.draft.original.setting_explicit(key)
                    && values(&self.draft.original)[self.selected]
                        == values(&self.draft.proposed)[self.selected];
                if unchanged {
                    if let Err(error) = self.draft.unstage(key) {
                        self.note = error;
                        return;
                    }
                }
                self.staged_source[self.selected] = (!unchanged).then_some(source);
                self.mode = Mode::Browse;
                self.note = if unchanged {
                    "Matches the file. No change staged for this field."
                } else {
                    "Staged only. No file written."
                }
                .into();
            }
            Err(error) => self.note = error,
        }
    }

    pub(crate) fn input(&mut self, key: u8) -> Outcome {
        match self.mode {
            Mode::Browse => match key {
                b'j' | b'k' => {
                    let pos = GRID_ORDER.iter().position(|&i| i == self.selected).unwrap();
                    let next = if key == b'j' {
                        (pos + 1).min(FIELDS.len() - 1)
                    } else {
                        pos.saturating_sub(1)
                    };
                    self.selected = GRID_ORDER[next];
                }
                b'\r' | b'\n' => self.open_presets(),
                b'w' if !self.changes().is_empty() => {
                    self.mode = Mode::Confirm;
                    self.review_page = 0;
                    self.review_seen = 1;
                    self.note.clear();
                }
                b'w' => self.note = "No changed values to save.".into(),
                27 => return Outcome::Cancel,
                b'q' => return Outcome::Quit,
                _ => return Outcome::Ignore,
            },
            Mode::Presets => match key {
                b'j' => self.choice_index = (self.choice_index + 1).min(self.choices.len() - 1),
                b'k' => self.choice_index = self.choice_index.saturating_sub(1),
                b'm' => {
                    self.mode = Mode::Manual;
                    self.buffer.clear();
                    self.note.clear();
                }
                b'\r' | b'\n' => {
                    let choice = &self.choices[self.choice_index];
                    let (raw, system) = (choice.raw.clone(), choice.system);
                    self.stage_selected(&raw, EditSource::Preset { system });
                }
                27 => {
                    self.mode = Mode::Browse;
                    self.note.clear();
                }
                b'q' => return Outcome::Quit,
                _ => return Outcome::Ignore,
            },
            Mode::Manual => match key {
                b'p' | 27 => {
                    self.mode = Mode::Presets;
                    self.note.clear();
                }
                b'q' => return Outcome::Quit,
                8 | 127 => {
                    self.buffer.pop();
                }
                b'\r' | b'\n' => {
                    let value = self.buffer.clone();
                    self.stage_selected(&value, EditSource::Manual);
                }
                b' '..=b'~' if self.buffer.len() < 48 => self.buffer.push(key as char),
                _ => return Outcome::Ignore,
            },
            Mode::Confirm => match key {
                b'j' => {
                    self.review_page = (self.review_page + 1).min(self.changes().len() - 1);
                    self.review_seen = self.review_seen.max(self.review_page + 1);
                    self.note.clear();
                }
                b'k' => self.review_page = self.review_page.saturating_sub(1),
                b'\r' | b'\n' if self.review_seen == self.changes().len() => return Outcome::Save,
                b'\r' | b'\n' => self.note = "Press j to review every change before saving.".into(),
                27 => {
                    self.mode = Mode::Browse;
                    self.note.clear();
                }
                b'q' => return Outcome::Quit,
                _ => return Outcome::Ignore,
            },
        }
        Outcome::Continue
    }

    pub(crate) fn selecting(&self) -> Option<usize> {
        (self.mode != Mode::Confirm).then_some(self.selected)
    }

    pub(crate) fn editing_value(&self) -> Option<&str> {
        (self.mode == Mode::Manual).then_some(self.buffer.as_str())
    }

    pub(crate) fn choices(&self) -> Option<(&[Choice], usize)> {
        (self.mode == Mode::Presets).then_some((&self.choices, self.choice_index))
    }

    pub(crate) fn review(&self) -> (Vec<Vec<String>>, Vec<String>) {
        let before = values(&self.draft.original);
        let after = values(&self.draft.proposed);
        let mut rows = Vec::new();
        let hints = match self.mode {
            Mode::Browse | Mode::Presets | Mode::Manual => unreachable!("not in review"),
            Mode::Confirm => {
                let changed = self.changes();
                let i = changed[self.review_page];
                rows.push(vec![
                    FIELDS[i].0.into(),
                    before[i].clone(),
                    after[i].clone(),
                    FIELDS[i].1.into(),
                ]);
                let origin = if self.draft.original.setting_explicit(FIELDS[i].0) {
                    "file"
                } else {
                    "built-in fallback"
                };
                let proposal = match self.staged_source[i] {
                    Some(EditSource::Preset { system: true }) => "system preset",
                    Some(EditSource::Preset { system: false }) => "preset",
                    _ => "manual",
                };
                let mut hints = vec![
                    format!(
                        "Review {}/{} | j/k Next/previous change",
                        self.review_page + 1,
                        changed.len()
                    ),
                    format!("Source: {origin} → {proposal}"),
                ];
                if i == FLOOR
                    && self.draft.proposed.triggers.min_available_mb
                        > self.draft.original.triggers.min_available_mb
                {
                    hints.push("A higher floor can trigger cleanup sooner after reload.".into());
                } else {
                    hints.push(
                        "Saving changes what cleanup can do after a daemon reload or restart."
                            .into(),
                    );
                }
                hints.push("No signal or reload is sent. Enter Save file | Esc Back | q Quit without saving".into());
                hints
            }
        };
        (rows, hints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn write() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            // The release build mounts its checkout read-only and sets an
            // external Cargo target directory; test files belong in TMPDIR.
            let dir =
                std::env::temp_dir().join(format!("watch-fixture-{}-{suffix}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn watch_preset_is_capacity_only_and_avoids_idle_small_host_trigger() {
        use crate::metrics::{MemoryInfo, PsiMemory, SystemSnapshot};
        use crate::triggers::{LocalTime, TriggerEngine};
        use std::time::Instant;
        for (ram_mib, floor_mib) in [
            (1024, 128),
            (4096, 192),
            (8192, 384),
            (16384, 704),
            (32768, 1344),
            (65536, 1536),
        ] {
            let preset = SystemPreset::from_ram_mib(ram_mib).unwrap();
            assert_eq!(preset.floor_mib, floor_mib);
            let now = Instant::now();
            let local = LocalTime {
                year: 2026,
                month: 11,
                day: 23,
                hour: 12,
                minute: 0,
                weekday: 1,
            };
            for (available_mb, load_avg_1m) in [(ram_mib * 3 / 4, 0.0), (ram_mib / 2, 9.0)] {
                let snap = SystemSnapshot {
                    memory: MemoryInfo {
                        total_mb: ram_mib,
                        available_mb,
                        used_mb: ram_mib - available_mb,
                        ..Default::default()
                    },
                    psi: PsiMemory::default(),
                    swaps: vec![],
                    tmpfs_mounts: vec![],
                    load_avg_1m,
                    load_avg_5m: 0.0,
                    load_avg_15m: 0.0,
                    cpu_count: 1,
                };
                let mut cfg = Config::default();
                let legacy = TriggerEngine::new(cfg.clone()).evaluate(&snap, local, now);
                assert_eq!(legacy.should_run, ram_mib == 1024);
                if ram_mib == 1024 {
                    assert!(legacy.reasons.iter().any(|r| r.contains("Available RAM")));
                }
                cfg.triggers.min_available_mb = SystemPreset::from_ram_mib(snap.memory.total_mb)
                    .unwrap()
                    .floor_mib;
                assert_eq!(cfg.triggers.min_available_mb, floor_mib);
                assert!(cfg.validate().is_empty());
                assert!(
                    !TriggerEngine::new(cfg)
                        .evaluate(&snap, local, now)
                        .should_run
                );
            }
        }
        assert!(SystemPreset::from_ram_mib(0).is_none());
        assert_eq!(
            SystemPreset::from_ram_mib(u64::MAX).unwrap().floor_mib,
            1536
        );
    }

    fn pick(editor: &mut Editor, raw: &str) {
        editor.input(b'\r');
        while editor.choice_index > 0 {
            editor.input(b'k');
        }
        let target = editor
            .choices
            .iter()
            .position(|choice| choice.raw == raw)
            .unwrap();
        for _ in 0..target {
            editor.input(b'j');
        }
        editor.input(b'\r');
    }

    #[test]
    fn watch_menu_values_are_field_valid_ordered_and_finite() {
        let fixture = Fixture::write();
        let dest = fixture.0.join("settings.toml");
        fs::write(&dest, "# preset test\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        let machine = SystemPreset::from_ram_mib(8192);
        let mut draft = WatchConfigDraft::open(Some(&dest)).unwrap();
        for (i, (key, unit)) in FIELDS.iter().enumerate() {
            let choices = field_choices(i, machine);
            if *unit == "true/false" {
                assert_eq!(
                    choices
                        .iter()
                        .map(|choice| choice.raw.as_str())
                        .collect::<Vec<_>>(),
                    ["false", "true"]
                );
            } else {
                assert!(choices
                    .windows(2)
                    .all(|pair| pair[0].raw.parse::<f64>().unwrap()
                        < pair[1].raw.parse::<f64>().unwrap()));
            }
            for choice in choices {
                draft
                    .stage(key, &choice.raw)
                    .unwrap_or_else(|e| panic!("{key}={} {e}", choice.raw));
                draft.unstage(key).unwrap();
            }
        }
        assert_eq!(fs::read_to_string(&dest).unwrap(), "# preset test\n");
        assert_eq!(
            field_choices(FLOOR, machine)
                .iter()
                .filter(|choice| choice.system)
                .count(),
            1
        );
        assert!(!field_choices(FLOOR, None)
            .iter()
            .any(|choice| choice.system));
    }

    #[test]
    fn watch_editor_requires_an_explicit_switch_before_manual_entry() {
        let fixture = Fixture::write();
        let dest = fixture.0.join("settings.toml");
        let baseline = "[triggers]\nram_used_percent = 87.0\n";
        fs::write(&dest, baseline).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        let cfg = crate::config::load_watch_config(Some(&dest)).unwrap();
        let mut editor = Editor::open(&cfg, None).unwrap();
        editor.input(b'\r');
        assert_eq!(editor.choices().unwrap().0[editor.choice_index].raw, "85");
        for key in b"83.5w" {
            assert!(matches!(editor.input(*key), Outcome::Ignore));
        }
        assert!(editor.changes().is_empty());
        editor.input(b'j');
        editor.input(b'\r');
        assert_eq!(editor.draft.proposed.triggers.ram_used_percent, 90.0);
        assert_eq!(source(&cfg, 0, Some(&editor)), 'P');
        editor.input(b'\r');
        editor.input(b'm');
        for key in b"83.5\r" {
            editor.input(*key);
        }
        assert_eq!(editor.draft.proposed.triggers.ram_used_percent, 83.5);
        assert_eq!(source(&cfg, 0, Some(&editor)), 'M');
        assert_eq!(fs::read_to_string(&dest).unwrap(), baseline);
    }

    #[test]
    fn watch_preset_respects_explicit_source_until_deliberate_review_and_save() {
        let fixture = Fixture::write();
        let dest = fixture.0.join("settings.toml");
        let missing = "[triggers] # existing heading\r\nram_used_percent = 87.0\r\n[logging]\r\nlevel = \"WARN\"\r\n";
        let preset = SystemPreset::from_ram_mib(8192);
        fs::write(&dest, missing).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        let make_editor = || {
            Editor::open(
                &crate::config::load_watch_config(Some(&dest)).unwrap(),
                preset,
            )
            .unwrap()
        };
        let mut editor = make_editor();
        let original = editor.draft.original.clone();
        assert_eq!(source(&original, 0, None), 'F');
        assert_eq!(source(&original, FLOOR, None), 'B');
        editor.input(b'p');
        assert!(editor.changes().is_empty());
        editor.input(b'j');
        editor.input(b'\r');
        assert!(editor.choices().unwrap().0[editor.choice_index].system);
        assert!(editor.changes().is_empty());
        editor.input(b'\r');
        assert_eq!(source(&original, FLOOR, Some(&editor)), 'P');
        assert_eq!(editor.draft.proposed.triggers.min_available_mb, 384);
        editor.input(b'w');
        assert!(editor
            .review()
            .1
            .iter()
            .any(|line| line.contains("built-in fallback → system preset")));
        assert_eq!(fs::read_to_string(&dest).unwrap(), missing);
        assert!(matches!(editor.input(b'\r'), Outcome::Save));
        let saved = editor.draft.save().unwrap();
        assert_eq!(source(&saved, FLOOR, None), 'F');
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            missing.replace("[logging]", "min_available_mb = 384\r\n[logging]")
        );
        drop(editor);
        let mut editor = make_editor();
        assert_eq!(editor.draft.proposed.triggers.min_available_mb, 384);
        editor.input(b'j');
        editor.input(b'\r');
        editor.input(b'm');
        for b in b"256\r" {
            editor.input(*b);
        }
        assert_eq!(source(&saved, FLOOR, Some(&editor)), 'M');
        pick(&mut editor, "384");
        // Explicitly choosing the exact existing source clears the manual draft.
        assert!(editor.changes().is_empty());
        fs::write(
            &dest,
            missing.replace("[logging]", "min_available_mb = 0\r\n[logging]"),
        )
        .unwrap();
        let mut editor = make_editor();
        assert_eq!(editor.draft.proposed.triggers.min_available_mb, 0);
        editor.input(b'j');
        pick(&mut editor, "384");
        editor.input(b'w');
        assert!(editor
            .review()
            .1
            .iter()
            .any(|line| line.contains("file → system preset")));
        assert!(editor
            .review()
            .1
            .iter()
            .any(|line| line.contains("trigger cleanup sooner")));
        assert_eq!(
            crate::config::load_watch_config(Some(&dest))
                .unwrap()
                .triggers
                .min_available_mb,
            0
        );
    }

    #[test]
    fn stage_cancel_confirm_atomically_saves_only_the_fixture_with_original_permissions() {
        let fixture = Fixture::write();
        let dest = fixture.0.join("settings.toml");
        let other = fixture.0.join("untouched");
        let base = "[triggers] # preserve header\nram_used_percent = 87.0 # preserve inline\n# keep this comment\n";
        fs::write(&dest, base).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(&other, b"unchanged").unwrap();
        let launch = || {
            Editor::open(
                &crate::config::load_watch_config(Some(&dest)).unwrap(),
                None,
            )
            .unwrap()
        };
        let mut first = launch();
        assert!(matches!(first.input(b'\r'), Outcome::Continue));
        first.input(b'm');
        for ch in b"999" {
            first.input(*ch);
        }
        first.input(b'\r');
        assert!(first.note.contains("between 0 and 100"));
        assert_eq!(first.draft.proposed.triggers.ram_used_percent, 87.0);
        first.input(27);
        first.input(27);
        first.draft.stage("ram-percent", "85").unwrap();
        assert!(matches!(first.input(27), Outcome::Cancel));
        assert_eq!(fs::read_to_string(&dest).unwrap(), base);
        drop(first);
        let mut editor = launch();
        editor.draft.stage("ram-percent", "85").unwrap();
        editor.draft.stage("notify", "true").unwrap();
        editor.input(b'w');
        assert_eq!(editor.review().0.len(), 1);
        assert!(matches!(editor.input(b'\r'), Outcome::Continue));
        editor.input(b'j');
        assert!(editor.review().1[0].contains("2/2"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), base);
        assert!(matches!(editor.input(b'\r'), Outcome::Save));
        let saved = editor.draft.save().unwrap();
        assert_eq!(saved.triggers.ram_used_percent, 85.0);
        assert!(saved.notifications.enabled);
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(fs::read_to_string(&dest)
            .unwrap()
            .contains("85.0 # preserve inline"));
        assert_eq!(fs::read(&other).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }

    #[test]
    fn rejects_stale_config_and_symlink_and_never_bootstraps_defaults() {
        let fixture = Fixture::write();
        let dest = fixture.0.join("settings.toml");
        let link = fixture.0.join("link.toml");
        fs::write(&dest, "[triggers]\nram_used_percent=87.0\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&dest, &link).unwrap();
        assert!(WatchConfigDraft::open(Some(&link)).is_err());
        assert!(WatchConfigDraft::open(None).is_err());
        let mut draft = WatchConfigDraft::open(Some(&dest)).unwrap();
        draft.stage("ram-percent", "85").unwrap();
        let concurrent = "[triggers]\nram_used_percent=89.0\n";
        fs::write(&dest, concurrent).unwrap();
        assert!(draft.save().unwrap_err().contains("changed on disk"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), concurrent);
    }
}
