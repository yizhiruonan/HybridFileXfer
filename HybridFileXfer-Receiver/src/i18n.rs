//! Tiny two-language helper (Chinese / English).

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn set_lang(lang: Lang) {
    let _ = LANG.set(lang);
}

pub fn lang() -> Lang {
    *LANG.get().unwrap_or(&Lang::Zh)
}

/// Pick the Chinese or English variant of a message.
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// Same as [`tr`] but returns an owned String (for formatting).
#[allow(dead_code)]
pub fn trs(zh: &str, en: &str) -> String {
    match lang() {
        Lang::Zh => zh.to_string(),
        Lang::En => en.to_string(),
    }
}