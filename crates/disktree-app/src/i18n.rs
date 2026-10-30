//! Window language and a small, dependency-free message catalog.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use gpui_kit::{App, Global};

#[path = "locales/zh_cn.rs"]
mod zh_cn;

/// Languages offered by the window, using stable keys in the saved setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    English,
    SimplifiedChinese,
}

impl Global for Language {}

impl Language {
    /// Display order of the built-in languages.
    pub const ALL: [Self; 2] = [Self::English, Self::SimplifiedChinese];

    /// Stable locale identifier; independent of the translated display name.
    pub const fn key(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
        }
    }

    /// Autonyms keep the language control usable after an accidental switch.
    pub const fn name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }

    /// Unknown or newer keys safely retain the original English interface.
    pub fn parse(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|lang| lang.key() == key)
            .unwrap_or_default()
    }

    /// English is also the fallback for a message not yet translated.
    pub fn text(self, key: &str) -> &str {
        if self == Self::SimplifiedChinese
            && let Ok(index) =
                zh_cn::MESSAGES.binary_search_by_key(&key, |&(en, _)| en)
        {
            return zh_cn::MESSAGES[index].1;
        }
        key
    }
}

/// Read the language from this GPUI app, keeping window tests isolated.
pub fn language(cx: &App) -> Language {
    cx.try_global::<Language>().copied().unwrap_or_default()
}

/// Translate presentation text; never use this for paths or protocol values.
pub fn text<'a>(cx: &App, key: &'a str) -> &'a str {
    language(cx).text(key)
}

/// Substitute numbered slots in one pass. Inserted paths are never parsed
/// again, even when they contain braces or look like another slot.
pub fn format(template: &str, values: &[String]) -> String {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(end) = rest.find('}')
            && let Ok(index) = rest[1..end].parse::<usize>()
            && let Some(value) = values.get(index)
        {
            result.push_str(value);
            rest = &rest[end + 1..];
        } else {
            result.push('{');
            rest = &rest[1..];
        }
    }
    result.push_str(rest);
    result
}

/// Save beside the existing per-user power preference, never in a scan root.
pub fn settings_path() -> Option<PathBuf> {
    crate::power::settings_path().map(|path| path.with_file_name("language"))
}

/// Missing preferences use English; filesystem failures remain visible.
pub fn load(path: &Path) -> io::Result<Language> {
    match std::fs::read_to_string(path) {
        Ok(value) => Ok(Language::parse(value.trim())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(Language::default())
        }
        Err(error) => Err(error),
    }
}

/// Atomic replacement preserves the previous setting after a failed write.
pub fn save(path: &Path, language: Language) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no settings directory")
    })?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(file, "{}", language.key())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Static messages borrow the catalog; numbered messages format their values.
macro_rules! t {
    ($cx:expr, $key:expr) => { crate::i18n::text($cx, $key) };
    ($cx:expr, $key:expr, $($value:expr),+ $(,)?) => {
        crate::i18n::format(crate::i18n::text($cx, $key), &[$(format!("{}", $value)),+])
    };
}
pub(crate) use t;

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(template: &str) -> Vec<usize> {
        let mut slots = template
            .split('{')
            .skip(1)
            .map(|part| {
                part.split_once('}')
                    .expect("closed slot")
                    .0
                    .parse()
                    .expect("numbered slot")
            })
            .collect::<Vec<_>>();
        slots.sort_unstable();
        slots
    }

    #[test]
    fn catalog_is_sorted_complete_and_preserves_every_slot() {
        let mut previous = "";
        for &(english, chinese) in zh_cn::MESSAGES {
            assert!(english > previous, "duplicate or unsorted: {english}");
            assert!(!chinese.is_empty(), "missing: {english}");
            assert_eq!(slots(english), slots(chinese), "slots: {english}");
            assert_eq!(Language::English.text(english), english);
            assert_eq!(Language::SimplifiedChinese.text(english), chinese);
            previous = english;
        }
        assert_eq!(
            Language::SimplifiedChinese.text("untranslated diagnostic"),
            "untranslated diagnostic"
        );
    }

    #[test]
    fn numbered_slots_can_reorder_without_interpreting_inserted_paths() {
        assert_eq!(
            format("{1}: {0} / {1}", &["folder/{1}/中文".into(), "3".into()]),
            "3: folder/{1}/中文 / 3"
        );
        assert_eq!(format("{9} {bad} {", &[]), "{9} {bad} {");
    }

    #[test]
    fn saved_language_survives_replacement_and_unknown_values_fall_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config/language");
        assert_eq!(load(&path).expect("missing"), Language::English);
        for language in Language::ALL {
            save(&path, language).expect("save");
            assert_eq!(load(&path).expect("load"), language);
        }
        std::fs::write(&path, "future-language").expect("write");
        assert_eq!(load(&path).expect("fallback"), Language::English);
        assert!(save(dir.path(), Language::English).is_err());
        assert!(load(dir.path()).is_err());
    }
}
