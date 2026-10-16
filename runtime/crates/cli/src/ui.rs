use fluent_bundle::FluentArgs;
use rivo_i18n::{LocalLanguageManager, LocaleId, ReloadableLanguageCatalog};
use std::sync::{OnceLock, RwLock};

static CLI_LOCALE: OnceLock<RwLock<Option<String>>> = OnceLock::new();

pub(crate) fn has_explicit_locale() -> bool {
    CLI_LOCALE
        .get()
        .and_then(|slot| slot.read().ok().and_then(|value| value.clone()))
        .is_some()
}

pub(crate) fn set_locale(locale: Option<String>) {
    let slot = CLI_LOCALE.get_or_init(|| RwLock::new(None));
    if let Ok(mut value) = slot.write() {
        *value = locale;
    }
}

pub(crate) fn text(id: &str, fallback: &str) -> String {
    let configured = CLI_LOCALE
        .get()
        .and_then(|slot| slot.read().ok().and_then(|value| value.clone()))
        .and_then(|value| LocaleId::parse(&value).ok())
        .or_else(|| LocaleId::parse("en-US").ok());
    let Some(locale) = configured else {
        return fallback.to_owned();
    };
    LocalLanguageManager::from_environment()
        .ok()
        .and_then(|manager| ReloadableLanguageCatalog::load(&manager, &locale).ok())
        .and_then(|catalog| catalog.catalog().format(id, None))
        .unwrap_or_else(|| fallback.to_owned())
}

/// Resolve a message through the active CLI catalog. With no explicit override the
/// product default is the bundled `en-US` catalog; an explicit `--language` selects
/// that canonical locale instead. This keeps all human-readable CLI paths on the
/// same fallback chain as help and TUI rendering.
pub(crate) fn explicit_text(id: &str, fallback: &str) -> String {
    text(id, fallback)
}

/// Format a catalog message with Fluent variables. The fallback remains code-owned and is
/// used when the selected catalog is unavailable or has an invalid pattern.
pub(crate) fn text_with(id: &str, fallback: &str, values: &[(&str, String)]) -> String {
    let configured = CLI_LOCALE
        .get()
        .and_then(|slot| slot.read().ok().and_then(|value| value.clone()))
        .and_then(|value| LocaleId::parse(&value).ok());
    let locale = configured
        .or_else(|| LocaleId::parse("en-US").ok())
        .expect("built-in en-US locale");
    let mut args = FluentArgs::new();
    for (key, value) in values {
        args.set(*key, value.clone());
    }
    LocalLanguageManager::from_environment()
        .ok()
        .and_then(|manager| ReloadableLanguageCatalog::load(&manager, &locale).ok())
        .and_then(|catalog| catalog.catalog().format(id, Some(&args)))
        .map(|value| {
            value
                .chars()
                .filter(|character| *character != '\u{2068}' && *character != '\u{2069}')
                .collect()
        })
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fluent_arguments_are_rendered_for_runtime_messages() {
        set_locale(Some("en-US".into()));
        let rendered = text_with(
            "cli-sessions-migration-complete",
            "fallback bound_sessions=1",
            &[
                ("backup", "backup.db".into()),
                ("sha256", "hash".into()),
                ("bound", "1".into()),
                ("unscoped", "0".into()),
            ],
        );
        assert!(rendered.contains("bound_sessions=1"), "{rendered}");
    }
}
