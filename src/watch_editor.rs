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

#[derive(PartialEq, Eq)]
enum Mode {
    Browse,
    Value,
    Confirm,
}

pub(crate) enum Outcome {
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
}

impl Editor {
    pub(crate) fn open(cfg: &Config) -> Result<Self, String> {
        Ok(Self {
            draft: WatchConfigDraft::open(cfg.source_path.as_deref())?,
            selected: 0,
            mode: Mode::Browse,
            buffer: String::new(),
            review_page: 0,
            review_seen: 0,
            note: String::new(),
        })
    }

    fn changes(&self) -> Vec<usize> {
        values(&self.draft.original)
            .iter()
            .zip(values(&self.draft.proposed))
            .enumerate()
            .filter_map(|(i, (old, new))| (old != &new).then_some(i))
            .collect()
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
                b'\r' | b'\n' => {
                    self.mode = Mode::Value;
                    self.buffer.clear();
                    self.note.clear();
                }
                b'w' if !self.changes().is_empty() => {
                    self.mode = Mode::Confirm;
                    self.review_page = 0;
                    self.review_seen = 1;
                    self.note.clear();
                }
                b'w' => self.note = "No changed values to save.".into(),
                27 => return Outcome::Cancel,
                b'q' => return Outcome::Quit,
                _ => {}
            },
            Mode::Value => match key {
                27 => {
                    self.mode = Mode::Browse;
                    self.note.clear();
                }
                8 | 127 => {
                    self.buffer.pop();
                }
                b'\r' | b'\n' => match self.draft.stage(FIELDS[self.selected].0, &self.buffer) {
                    Ok(()) => {
                        if values(&self.draft.original)[self.selected]
                            == values(&self.draft.proposed)[self.selected]
                        {
                            if let Err(error) = self.draft.unstage(FIELDS[self.selected].0) {
                                self.note = error;
                                return Outcome::Continue;
                            }
                        }
                        self.mode = Mode::Browse;
                        self.note = "Staged only. No file written.".into();
                    }
                    Err(e) => self.note = e,
                },
                b' '..=b'~' if self.buffer.len() < 48 => self.buffer.push(key as char),
                _ => {}
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
                _ => {}
            },
        }
        Outcome::Continue
    }

    pub(crate) fn selecting(&self) -> Option<usize> {
        (self.mode != Mode::Confirm).then_some(self.selected)
    }

    pub(crate) fn editing_value(&self) -> Option<&str> {
        (self.mode == Mode::Value).then_some(self.buffer.as_str())
    }

    pub(crate) fn review(&self) -> (Vec<Vec<String>>, Vec<String>) {
        let before = values(&self.draft.original);
        let after = values(&self.draft.proposed);
        let mut rows = Vec::new();
        let hints = match self.mode {
            Mode::Value => {
                rows.push(vec![
                    FIELDS[self.selected].0.into(),
                    before[self.selected].clone(),
                    self.buffer.clone(),
                    FIELDS[self.selected].1.into(),
                ]);
                vec!["Type the proposed value. Enter stages; Esc cancels this input.".into()]
            }
            Mode::Browse => unreachable!("browse has its own compact grid"),
            Mode::Confirm => {
                let changed = self.changes();
                let i = changed[self.review_page];
                rows.push(vec![
                    FIELDS[i].0.into(),
                    before[i].clone(),
                    after[i].clone(),
                    FIELDS[i].1.into(),
                ]);
                vec![format!("Review {}/{} | j/k Next/previous change", self.review_page + 1, changed.len()), "Saving changes what cleanup can do after a daemon reload or restart.".into(), "No signal or reload is sent. Enter Save file | Esc Back | q Quit without saving".into()]
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
            let dir = std::env::current_dir()
                .unwrap()
                .join("target")
                .join(format!("watch-fixture-{}-{suffix}", std::process::id()));
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
    fn stage_cancel_confirm_atomically_saves_only_the_fixture_with_original_permissions() {
        let fixture = Fixture::write();
        let dest = fixture.0.join("settings.toml");
        let other = fixture.0.join("untouched");
        let base = "[triggers] # preserve header\nram_used_percent = 87.0 # preserve inline\n# keep this comment\n";
        fs::write(&dest, base).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(&other, b"unchanged").unwrap();
        let launch = || Editor::open(&crate::config::load_config(Some(&dest)).unwrap()).unwrap();
        let mut first = launch();
        assert!(matches!(first.input(b'\r'), Outcome::Continue));
        for ch in b"999" {
            first.input(*ch);
        }
        first.input(b'\r');
        assert!(first.note.contains("between 0 and 100"));
        assert_eq!(first.draft.proposed.triggers.ram_used_percent, 87.0);
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
