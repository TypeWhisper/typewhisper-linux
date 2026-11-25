//! Translations through gettext, in the language of the system.
//!
//! As in the macOS app's string catalog, the English text is the message ID, so
//! a text without a translation shows in English. `tools/i18n/update-translations.py`
//! collects the texts passed to [`tr`], [`tr_with`] and [`n_`] into `po/`, and the
//! build compiles the catalogs to `<prefix>/share/locale/<language>/LC_MESSAGES/typewhisper.mo`.
//! GTK sets the locale from the environment when it starts.

use std::path::{Path, PathBuf};

/// The gettext domain, also the name of the `.mo` files.
pub const DOMAIN: &str = "typewhisper";

/// Binds the domain to the installed catalogs. Call it before any text is
/// translated; without catalogs, every text stays English.
pub fn init() {
    let directory = locale_dir();
    let _ = gettextrs::bindtextdomain(DOMAIN, directory);
    let _ = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = gettextrs::textdomain(DOMAIN);
}

/// `text` in the language of the system.
pub fn tr(text: &str) -> String {
    gettextrs::gettext(text)
}

/// Translates `text` and fills its placeholders, each a name in braces such as
/// `{error}`. Named placeholders let a translation put them in any order.
pub fn tr_with(text: &str, values: &[(&str, &str)]) -> String {
    fill(tr(text), values)
}

/// Marks `text` for translation without translating it, for constants that are
/// translated with [`tr`] where they are shown.
pub const fn n_(text: &'static str) -> &'static str {
    text
}

fn fill(mut text: String, values: &[(&str, &str)]) -> String {
    for (name, value) in values {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// `share/locale` next to the executable's `bin` directory, which covers
/// `/usr`, `/app` in Flatpak and the development build. Otherwise the first
/// locale directory in the XDG data directories that has catalogs.
fn locale_dir() -> PathBuf {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("share/locale")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    let candidates: Vec<PathBuf> = beside
        .iter()
        .cloned()
        .chain(
            std::env::split_paths(&data_dirs)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join("locale")),
        )
        .collect();
    candidates
        .iter()
        .find(|dir| has_catalogs(dir))
        .or(beside.as_ref())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/usr/share/locale"))
}

fn has_catalogs(directory: &Path) -> bool {
    std::fs::read_dir(directory).is_ok_and(|languages| {
        languages.flatten().any(|language| {
            language
                .path()
                .join("LC_MESSAGES")
                .join(format!("{DOMAIN}.mo"))
                .is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_named_placeholders_in_any_order() {
        let text = "{b} vor {a}".to_owned();
        assert_eq!(fill(text, &[("a", "eins"), ("b", "zwei")]), "zwei vor eins");
    }

    #[test]
    fn finds_catalogs_by_their_domain() {
        let dir = std::env::temp_dir().join(format!("tw-locale-{}", std::process::id()));
        let messages = dir.join("de").join("LC_MESSAGES");
        std::fs::create_dir_all(&messages).unwrap();
        assert!(!has_catalogs(&dir));
        std::fs::write(messages.join("typewhisper.mo"), b"").unwrap();
        assert!(has_catalogs(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }
}
