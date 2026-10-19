//! Two-language UI text: English by default, Turkish when the system language
//! is Turkish. `ARP_LANG=en` / `ARP_LANG=tr` overrides the detection.
//!
//! Strings live next to each other at the call site via the [`t!`] macro, so
//! there is no catalog to keep in sync:
//!
//! ```ignore
//! log.push(t!("connected: {src}", "bağlandı: {src}"));
//! ```

use std::sync::OnceLock;

static TURKISH: OnceLock<bool> = OnceLock::new();

/// True when UI text should be Turkish (detected once, then cached).
pub fn is_turkish() -> bool {
    *TURKISH.get_or_init(detect)
}

fn detect() -> bool {
    if let Ok(v) = std::env::var("ARP_LANG") {
        return v.trim().to_lowercase().starts_with("tr");
    }
    sys_locale::get_locale()
        .map(|l| l.to_lowercase().starts_with("tr"))
        .unwrap_or(false)
}

/// `t!("English {x}", "Türkçe {x}")` → `String` in the UI language.
/// Both literals are ordinary `format!` strings (inline captures work).
#[macro_export]
macro_rules! t {
    ($en:literal, $tr:literal $(,)?) => {
        if $crate::i18n::is_turkish() {
            format!($tr)
        } else {
            format!($en)
        }
    };
    ($en:literal, $tr:literal, $($arg:tt)+) => {
        if $crate::i18n::is_turkish() {
            format!($tr, $($arg)+)
        } else {
            format!($en, $($arg)+)
        }
    };
}
