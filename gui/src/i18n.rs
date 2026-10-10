//! Locale detection and the Slint catalog tags.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLang {
    En,
    ZhCn,
    ZhTw,
    Ru,
}

impl UiLang {
    pub fn slint_tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh_CN",
            Self::ZhTw => "zh_TW",
            Self::Ru => "ru",
        }
    }
}

pub fn detect() -> UiLang {
    let var = |name| std::env::var(name).ok();
    let from_env = pick_locale([var("LC_ALL"), var("LC_MESSAGES"), var("LANG")]);
    let raw = from_env.unwrap_or_else(|| sys_locale::get_locale().unwrap_or_default());
    normalise(&raw)
}

/// The first non-empty value in POSIX precedence: LC_ALL, LC_MESSAGES, LANG.
fn pick_locale(vars: [Option<String>; 3]) -> Option<String> {
    vars.into_iter().flatten().find(|value| !value.is_empty())
}

fn normalise(raw: &str) -> UiLang {
    let tag = raw
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
        .to_ascii_lowercase();
    let mut parts = tag.split('-');
    match parts.next() {
        Some("ru") => return UiLang::Ru,
        Some("zh") => {}
        _ => return UiLang::En,
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
    use super::{normalise, pick_locale, UiLang};

    #[test]
    fn locale_variables_follow_posix_precedence() {
        let some = |v: &str| Some(v.to_string());
        assert_eq!(
            pick_locale([some("en_US.UTF-8"), None, some("zh_CN.UTF-8")]).as_deref(),
            Some("en_US.UTF-8")
        );
        assert_eq!(
            pick_locale([some(""), some("ru_RU.UTF-8"), some("zh_CN.UTF-8")]).as_deref(),
            Some("ru_RU.UTF-8")
        );
        assert_eq!(pick_locale([None, None, some("")]), None);
    }

    #[test]
    fn russian_locale_variants() {
        for tag in [
            "ru",
            "ru_RU",
            "ru-RU",
            "ru_RU.UTF-8",
            "RU-ru",
            "ru_BY@variant",
        ] {
            assert_eq!(normalise(tag), UiLang::Ru, "{tag}");
        }
        assert_eq!(normalise("rus"), UiLang::En);
    }

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
}
