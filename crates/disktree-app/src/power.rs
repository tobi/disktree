//! Saved fixed worker presets. Read before the first scan starts.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use disktree_core::scan_threads::ScanThreads;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerEfficiency {
    Miser,
    #[default]
    Balanced,
    Aggressive,
    DrainMyBattery,
}

impl PowerEfficiency {
    pub const ALL: [Self; 4] = [
        Self::Miser,
        Self::Balanced,
        Self::Aggressive,
        Self::DrainMyBattery,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Miser => "miser",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
            Self::DrainMyBattery => "drain-my-battery",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Miser => "Miser",
            Self::Balanced => "Balanced",
            Self::Aggressive => "Aggressive",
            Self::DrainMyBattery => "Drain My Battery",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.key() == value)
    }

    pub fn threads(self, cpus: usize) -> usize {
        let cpus = cpus.max(1);
        match self {
            Self::Miser => 2.min(cpus),
            Self::Balanced => 4.min(cpus),
            Self::Aggressive => 8.min(cpus),
            Self::DrainMyBattery => cpus,
        }
    }

    /// The localized preset name and effective worker count.
    pub fn label(self, cpus: usize, language: crate::i18n::Language) -> String {
        format!(
            "{} ({})",
            language.text(self.name()),
            workers(self.threads(cpus), language)
        )
    }

    /// A preset that would run no more workers than the one below it, on a
    /// machine with this many CPUs, is a duplicate there and is not offered.
    pub fn available(self, cpus: usize) -> bool {
        // Declared in `ALL` order, so the discriminant is the index.
        let index = self as usize;
        index == 0 || self.threads(cpus) > Self::ALL[index - 1].threads(cpus)
    }

    /// How many of the four gauge bars `threads` workers fill: the presets
    /// offered here that run no more than that, so a custom count reads on
    /// the same scale.
    pub fn signal(threads: usize, cpus: usize) -> usize {
        Self::ALL
            .into_iter()
            .filter(|preset| {
                preset.available(cpus) && preset.threads(cpus) <= threads
            })
            .count()
            .max(1)
    }

    pub fn policy(self, cpus: usize) -> ScanThreads {
        ScanThreads {
            max_threads: self.threads(cpus),
            adaptive: false,
            system_cpu_limit: None,
            ..ScanThreads::default()
        }
    }
}

/// The worker count in the interface language.
pub fn workers(count: usize, language: crate::i18n::Language) -> String {
    let unit = if count == 1 { "worker" } else { "workers" };
    format!("{count} {}", language.text(unit))
}

pub fn cpu_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// Follow each desktop's user configuration location; never write beside
/// the executable or scanned root. No home/config location means session-only.
pub fn settings_path() -> Option<PathBuf> {
    let home = std::env::home_dir();
    let base = if cfg!(target_os = "macos") {
        home.map(|home| home.join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| home.map(|home| home.join(".config")))
    };
    base.map(|base| base.join("disktree/power-efficiency"))
}

pub fn load(path: &Path) -> io::Result<PowerEfficiency> {
    match std::fs::read_to_string(path) {
        Ok(value) => PowerEfficiency::parse(value.trim()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unknown power preset")
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(PowerEfficiency::default())
        }
        Err(error) => Err(error),
    }
}

pub fn save(path: &Path, preset: PowerEfficiency) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no settings directory")
    })?;
    std::fs::create_dir_all(parent)?;
    // Atomic replacement leaves the previous choice intact after a failed
    // write, and readers never see a half-written setting.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(file, "{}", preset.key())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_fixed_and_show_the_effective_cpu_limit() {
        for (preset, expected) in
            PowerEfficiency::ALL.into_iter().zip([2, 4, 8, 18])
        {
            let policy = preset.policy(18);
            assert_eq!(policy.max_threads, expected);
            assert!(!policy.adaptive);
            assert_eq!(policy.system_cpu_limit, None);
            assert_eq!(preset.threads(1), 1);
        }
        assert_eq!(PowerEfficiency::Balanced.threads(3), 3);
        assert_eq!(
            PowerEfficiency::DrainMyBattery
                .label(18, crate::i18n::Language::English),
            "Drain My Battery (18 workers)"
        );
    }

    #[test]
    fn presets_that_add_no_workers_are_not_offered() {
        use PowerEfficiency as P;
        let offered = |cpus| {
            P::ALL
                .into_iter()
                .filter(|preset| preset.available(cpus))
                .collect::<Vec<_>>()
        };
        assert_eq!(offered(18), P::ALL);
        assert_eq!(offered(10), P::ALL);
        assert_eq!(offered(8), [P::Miser, P::Balanced, P::Aggressive]);
        assert_eq!(offered(6), [P::Miser, P::Balanced, P::Aggressive]);
        assert_eq!(offered(4), [P::Miser, P::Balanced]);
        assert_eq!(offered(3), [P::Miser, P::Balanced]);
        assert_eq!(offered(2), [P::Miser]);
        assert_eq!(offered(1), [P::Miser]);
        // The gauge counts what is offered, so a small machine running
        // everything it has reads as full as the menu allows.
        assert_eq!(P::signal(4, 18), 2);
        assert_eq!(P::signal(18, 18), 4);
        assert_eq!(P::signal(4, 4), 2);
        assert_eq!(P::signal(1, 18), 1);
        assert_eq!(P::signal(12, 18), 3);
    }

    #[test]
    fn saved_choice_survives_replacement_and_bad_data_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config/power-efficiency");
        assert_eq!(load(&path).expect("missing"), PowerEfficiency::Balanced);
        for preset in PowerEfficiency::ALL {
            save(&path, preset).expect("save");
            assert_eq!(load(&path).expect("load"), preset);
        }
        std::fs::write(&path, "not-a-preset").expect("corrupt");
        assert_eq!(
            load(&path).expect_err("invalid").kind(),
            io::ErrorKind::InvalidData
        );
        assert!(save(dir.path(), PowerEfficiency::Miser).is_err());
    }
}
