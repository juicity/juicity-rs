//! Locale detection and initialization.
//!
//! Reads `LANG`, `LC_ALL`, or `LC_MESSAGES` (in that priority order) to pick
//! the display language.  Falls back to `"en"` when none is set or the locale
//! is not recognised.
//!
//! Supported locales: `en`, `zh-CN`.

/// Detect the system locale from environment variables and activate it.
///
/// Call this once at program startup, *after* `rust_i18n::i18n!("locales")` has
/// been processed (i.e. after `main()` begins execution).
pub fn init() {
    let locale = detect();
    rust_i18n::set_locale(&locale);
    tracing::debug!("i18n locale set to: {locale}");
}

/// Return a rust-i18n locale tag that best matches the system environment.
pub fn detect() -> String {
    // Check common POSIX env vars in priority order.
    let raw = std::env::var("LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .unwrap_or_default();

    normalise(&raw)
}

/// Normalise a POSIX locale string (e.g. `zh_CN.UTF-8`) to a rust-i18n tag.
fn normalise(raw: &str) -> String {
    // Strip codeset suffix: "zh_CN.UTF-8" → "zh_CN"
    let without_codeset = raw.split('.').next().unwrap_or("en");

    // Replace underscore separator: "zh_CN" → "zh-CN"
    let tag = without_codeset.replace('_', "-");

    // Map to supported locales; fall back to "en".
    match tag.as_str() {
        t if t.starts_with("zh") => {
            // Distinguish simplified (zh-CN, zh-SG) from traditional (zh-TW, zh-HK).
            // We only ship zh-CN for now; everything else falls back to en.
            if t == "zh-CN" || t == "zh-SG" || t == "zh" {
                "zh-CN".to_string()
            } else {
                "en".to_string()
            }
        }
        _ => "en".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::normalise;

    #[test]
    fn posix_zh_cn() {
        assert_eq!(normalise("zh_CN.UTF-8"), "zh-CN");
    }

    #[test]
    fn posix_en_us() {
        assert_eq!(normalise("en_US.UTF-8"), "en");
    }

    #[test]
    fn zh_tw_falls_back() {
        assert_eq!(normalise("zh_TW.UTF-8"), "en");
    }

    #[test]
    fn empty_falls_back() {
        assert_eq!(normalise(""), "en");
    }
}
