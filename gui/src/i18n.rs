//! Locale detection with backend-specific catalog tags.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLang {
    En,
    ZhCn,
    ZhTw,
}

impl UiLang {
    #[cfg(feature = "ui-slint")]
    pub fn slint_tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh_CN",
            Self::ZhTw => "zh_TW",
        }
    }

    #[cfg(feature = "ui-gpui")]
    fn gpui_tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn | Self::ZhTw => "zh-CN",
        }
    }
}

#[cfg(feature = "ui-gpui")]
pub fn init() {
    let locale = detect().gpui_tag();
    rust_i18n::set_locale(locale);
    tracing::debug!("i18n locale set to: {locale}");
}

pub fn detect() -> UiLang {
    let from_env = std::env::var("LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .ok();
    let raw = from_env.unwrap_or_else(|| sys_locale::get_locale().unwrap_or_default());
    normalise(&raw)
}

fn normalise(raw: &str) -> UiLang {
    let tag = raw
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
        .to_ascii_lowercase();
    let mut parts = tag.split('-');
    if parts.next() != Some("zh") {
        return UiLang::En;
    }
    if parts
        .clone()
        .any(|part| matches!(part, "tw" | "hk" | "mo" | "hant"))
    {
        UiLang::ZhTw
    } else if tag == "zh" || parts.any(|part| matches!(part, "cn" | "sg" | "hans")) {
        UiLang::ZhCn
    } else {
        UiLang::En
    }
}

#[cfg(test)]
mod tests {
    use super::{normalise, UiLang};

    #[test]
    fn posix_zh_cn() {
        assert_eq!(normalise("zh_CN.UTF-8"), UiLang::ZhCn);
    }

    #[test]
    fn posix_en_us() {
        assert_eq!(normalise("en_US.UTF-8"), UiLang::En);
    }

    #[test]
    fn zh_tw_uses_traditional() {
        assert_eq!(normalise("zh_TW.UTF-8"), UiLang::ZhTw);
    }

    #[test]
    fn empty_falls_back() {
        assert_eq!(normalise(""), UiLang::En);
    }

    #[test]
    fn script_and_region_variants() {
        for tag in [
            "zh-HK",
            "zh_MO",
            "ZH-hANT",
            "zh-Hant-HK",
            "zh_TW@calendar=roc",
        ] {
            assert_eq!(normalise(tag), UiLang::ZhTw, "{tag}");
        }
        for tag in ["zh", "zh-SG", "zh-Hans", "zh-Hans-CN"] {
            assert_eq!(normalise(tag), UiLang::ZhCn, "{tag}");
        }
        for tag in ["fr-FR", "C.UTF-8", "zho", "zhish-TW", "zh-JP"] {
            assert_eq!(normalise(tag), UiLang::En, "{tag}");
        }
    }

    #[cfg(feature = "ui-gpui")]
    #[test]
    fn gpui_uses_existing_catalogs() {
        assert_eq!(UiLang::ZhTw.gpui_tag(), "zh-CN");
        assert_eq!(UiLang::ZhCn.gpui_tag(), "zh-CN");
        assert_eq!(UiLang::En.gpui_tag(), "en");
    }
}
