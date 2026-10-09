use crate::i18n::UiLang;

pub fn cjk_family(lang: UiLang) -> &'static str {
    let traditional = lang == UiLang::ZhTw;
    if cfg!(target_os = "windows") {
        if traditional {
            "Microsoft JhengHei UI"
        } else {
            "Microsoft YaHei UI"
        }
    } else if cfg!(target_os = "macos") {
        if traditional {
            "PingFang TC"
        } else {
            "PingFang SC"
        }
    } else if traditional {
        "Noto Sans CJK TC"
    } else {
        "Noto Sans CJK SC"
    }
}

#[cfg(any(target_os = "linux", test))]
const CJK_DIRS: [&str; 4] = [
    "/usr/share/fonts/noto-cjk",
    "/usr/share/fonts/opentype/noto",
    "/usr/share/fonts/google-noto-cjk",
    "/usr/share/fonts/google-noto-sans-cjk-fonts",
];

#[cfg(any(target_os = "linux", test))]
fn register(path: &std::path::Path) -> anyhow::Result<()> {
    use slint::fontique_011::{fontique, shared_collection};
    let data = std::fs::read(path)
        .map_err(|err| anyhow::anyhow!("Missing or unreadable font {}: {err}", path.display()))?;
    let registered =
        shared_collection().register_fonts(fontique::Blob::new(std::sync::Arc::new(data)), None);
    anyhow::ensure!(
        !registered.is_empty(),
        "Could not register font {}",
        path.display()
    );
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn cjk_file(name: &str) -> Option<std::path::PathBuf> {
    CJK_DIRS
        .iter()
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|path| path.is_file())
}

pub fn register_medium(lang: UiLang) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use slint::fontique_011::{fontique, shared_collection};
        let has_medium = || {
            shared_collection()
                .family_by_name(cjk_family(lang))
                .is_some_and(|family| {
                    family
                        .fonts()
                        .iter()
                        .any(|font| font.weight() == fontique::FontWeight::MEDIUM)
                })
        };
        if has_medium() {
            return Ok(());
        }
        if let Some(path) = cjk_file("NotoSansCJK-Medium.ttc") {
            register(&path)?;
            if has_medium() {
                return Ok(());
            }
        }
        tracing::warn!(
            "No CJK Medium face found for {}; searched directories: {}",
            cjk_family(lang),
            CJK_DIRS.join(", ")
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = lang;
    Ok(())
}

#[cfg(test)]
pub fn register_screenshot_fonts() -> anyhow::Result<()> {
    for name in ["NotoSansCJK-Regular.ttc", "NotoSansCJK-Medium.ttc"] {
        let path =
            cjk_file(name).ok_or_else(|| anyhow::anyhow!("Missing screenshot font: {name}"))?;
        register(&path)?;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
    for name in ["NotoSans-Regular.ttf", "NotoSans-Medium.ttf"] {
        register(&root.join(name))?;
    }
    Ok(())
}
