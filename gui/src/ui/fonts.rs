use crate::i18n::UiLang;

pub fn cjk_family(lang: UiLang) -> &'static str {
    // Cyrillic reads better in the bundled Latin face than in the CJK one.
    if lang == UiLang::Ru {
        return "Noto Sans";
    }
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
    if lang == UiLang::Ru {
        return Ok(());
    }
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

/// `head.fontRevision` (16.16) of Noto Sans CJK 2.004, the release the
/// screenshot goldens were rendered with (Ubuntu 26.04 `fonts-noto-cjk` and
/// `fonts-noto-cjk-extra`).
#[cfg(test)]
const SCREENSHOT_CJK_REVISION: u32 = 0x0002_0106;

/// Register the pinned screenshot fonts. `JUICITY_SHOTS_CJK_DIR` names a
/// directory with the 2.004 CJK files when the system has another release.
#[cfg(test)]
pub fn register_screenshot_fonts() -> anyhow::Result<()> {
    let cjk_dir = std::env::var_os("JUICITY_SHOTS_CJK_DIR").map(std::path::PathBuf::from);
    for name in ["NotoSansCJK-Regular.ttc", "NotoSansCJK-Medium.ttc"] {
        let path = match &cjk_dir {
            Some(dir) => dir.join(name),
            None => {
                cjk_file(name).ok_or_else(|| anyhow::anyhow!("Missing screenshot font: {name}"))?
            }
        };
        let data = std::fs::read(&path)
            .map_err(|err| anyhow::anyhow!("Missing screenshot font {}: {err}", path.display()))?;
        let revision = font_revision(&data);
        anyhow::ensure!(
            revision == Some(SCREENSHOT_CJK_REVISION),
            "Screenshot font {} has revision {}, the goldens need Noto Sans CJK 2.004 \
             ({SCREENSHOT_CJK_REVISION:#x}); set JUICITY_SHOTS_CJK_DIR to a directory with it",
            path.display(),
            revision.map_or_else(|| "unknown".to_string(), |value| format!("{value:#x}"))
        );
        register(&path)?;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
    for name in [
        "NotoSans-Regular.ttf",
        "NotoSans-Medium.ttf",
        "NotoSansMono-Regular.ttf",
    ] {
        register(&root.join(name))?;
    }
    Ok(())
}

/// `head.fontRevision` of a font file, or of the first face of a collection.
#[cfg(test)]
fn font_revision(data: &[u8]) -> Option<u32> {
    let u16_at = |at: usize| Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let face = if data.get(..4)? == b"ttcf" {
        u32_at(12)? as usize
    } else {
        0
    };
    let tables = u16_at(face + 4)? as usize;
    (0..tables)
        .map(|index| face + 12 + 16 * index)
        .find(|&record| data.get(record..record + 4) == Some(b"head"))
        .and_then(|record| u32_at(record + 8))
        .and_then(|head| u32_at(head as usize + 4))
}
