//! Child-process locale defaults on macOS.

use std::env;
use std::sync::OnceLock;

use objc2::sel;
use objc2_foundation::{NSLocale, NSObjectProtocol};

/// Prepare immutable child locale defaults without changing the embedding process locale.
pub fn set_locale_environment() {
    let _ = child_locale();
}

/// A machine locale cache, independent of pane, owner, or embedding context.
pub(crate) fn child_locale() -> (&'static str, &'static str) {
    static LOCALE: OnceLock<(String, String)> = OnceLock::new();
    let (key, value) = LOCALE.get_or_init(|| {
        let locale = system_locale();
        if std::path::Path::new("/usr/share/locale").join(&locale).is_dir() {
            ("LC_ALL".into(), locale)
        } else {
            ("LC_CTYPE".into(), "UTF-8".into())
        }
    });
    (key, value)
}

pub(crate) fn needs_child_locale() -> bool {
    !["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|key| {
        env::var(key).is_ok_and(|value| !value.is_empty() && value != "C" && value != "POSIX")
    })
}

/// Determine system locale based on language and country code.
fn system_locale() -> String {
    let locale = NSLocale::currentLocale();

    // `localeIdentifier` returns extra metadata with the locale (including currency and
    // collator) on newer versions of macOS. This is not a valid locale, so we use
    // `languageCode` and `countryCode`, if they're available (macOS 10.12+):
    //
    // https://developer.apple.com/documentation/foundation/nslocale/1416263-localeidentifier?language=objc
    // https://developer.apple.com/documentation/foundation/nslocale/1643060-countrycode?language=objc
    // https://developer.apple.com/documentation/foundation/nslocale/1643026-languagecode?language=objc
    let is_language_code_supported: bool = locale.respondsToSelector(sel!(languageCode));
    let is_country_code_supported: bool = locale.respondsToSelector(sel!(countryCode));
    if is_language_code_supported && is_country_code_supported {
        let language_code = locale.languageCode();
        #[expect(
            deprecated,
            reason = "the platform API has no equivalent replacement for this supported behavior"
        )]
        if let Some(country_code) = locale.countryCode() {
            format!("{}_{}.UTF-8", language_code, country_code)
        } else {
            // Fall back to en_US in case the country code is not available.
            "en_US.UTF-8".into()
        }
    } else {
        locale.localeIdentifier().to_string() + ".UTF-8"
    }
}
