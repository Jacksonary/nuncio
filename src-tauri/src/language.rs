//! Resolve the effective UI language for OS-level surfaces (tray menu,
//! notifications) when the setting is `system`.
//!
//! GUI processes started from the OS launcher (Finder, Explorer, a desktop
//! session) usually do not inherit `LANG`/`LC_ALL`, so the process locale is
//! only a fallback: the platform UI language is queried first.

use nuncio_core::models::{AppSettings, Language};

/// Effective language: explicit setting, otherwise the OS UI language.
pub fn resolve(settings: &AppSettings) -> Language {
    match settings.language {
        Language::ZhCn => Language::ZhCn,
        Language::EnUs => Language::EnUs,
        Language::System => {
            if system_prefers_chinese() {
                Language::ZhCn
            } else {
                Language::EnUs
            }
        }
    }
}

/// Read the OS UI language, falling back to the POSIX locale environment.
fn system_prefers_chinese() -> bool {
    // `sys-locale` reads `CFLocaleCopyPreferredLanguages` (AppleLanguages) on
    // macOS, `GetUserPreferredUILanguages` on Windows and the POSIX variables
    // on Linux, so it works in GUI processes launched without a shell.
    if let Some(locale) = sys_locale::get_locale() {
        if !locale.trim().is_empty() {
            return locale_prefers_chinese(&locale);
        }
    }
    // Fallback for environments where the OS query is unavailable (for
    // example a bare development shell or a minimal Linux container).
    for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() && locale_prefers_chinese(&value) {
                return true;
            }
        }
    }
    false
}

/// True for any Chinese locale tag (`zh`, `zh-CN`, `zh_Hans_CN`, ...).
fn locale_prefers_chinese(locale: &str) -> bool {
    // `LANGUAGE` may contain a colon-separated priority list; only the first
    // (highest priority) entry decides the UI language.
    let Some(first) = locale.split(':').find(|part| !part.trim().is_empty()) else {
        return false;
    };
    let normalized = first.trim().replace('_', "-").to_ascii_lowercase();
    normalized == "zh" || normalized.starts_with("zh-")
}

/// True for zh-CN.
pub fn is_chinese(language: Language) -> bool {
    matches!(language, Language::ZhCn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_languages_win() {
        let english = AppSettings {
            language: Language::EnUs,
            ..AppSettings::default()
        };
        assert_eq!(resolve(&english), Language::EnUs);
        let chinese = AppSettings {
            language: Language::ZhCn,
            ..AppSettings::default()
        };
        assert_eq!(resolve(&chinese), Language::ZhCn);
    }

    #[test]
    fn recognizes_chinese_locale_tags() {
        for tag in [
            "zh",
            "zh-CN",
            "zh_CN",
            "zh-Hans-CN",
            "zh_TW.UTF-8",
            "zh-CN:en-US",
        ] {
            assert!(locale_prefers_chinese(tag), "{tag} should be Chinese");
        }
    }

    #[test]
    fn rejects_non_chinese_locale_tags() {
        for tag in ["en", "en-US", "de_DE.UTF-8", "ja-JP", "en-US:zh-CN", ""] {
            assert!(!locale_prefers_chinese(tag), "{tag} should not be Chinese");
        }
    }
}
