/// Application icon name as registered in the icon theme.
pub const ICON_NAME: &str = "io.juicity.gui";

/// Asset paths of the embedded icon, as resolved by [`Assets`].
pub const SVG_ASSET: &str = "icon.svg";
pub const PNG_ASSET: &str = "icon.png";

/// `gui/icon.svg` embedded verbatim, so the binary carries the source artwork
/// in addition to the rasterized variants below.
pub const SVG: &[u8] = include_bytes!("../icon.svg");

// PNG bytes at standard sizes, generated from icon.svg by build.rs.
const ICON_16: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/16.png"));
const ICON_32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/32.png"));
const ICON_48: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/48.png"));
const ICON_64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/64.png"));
const ICON_128: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/128.png"));
const ICON_256: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/256.png"));

/// Asset source serving the application icon plus the bundled gpui-kit assets.
///
/// GPUI resolves `svg()` and `img()` paths through the application's asset
/// source, so exposing the icon here keeps it usable without shipping a file
/// next to the executable. Every other path — notably the Lucide icon set used
/// by gpui-kit components, such as the Select chevron — is delegated to the
/// bundled gpui-kit assets.
#[cfg(feature = "ui-gpui")]
pub struct Assets;

#[cfg(feature = "ui-gpui")]
impl gpui_kit::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes = match path {
            SVG_ASSET => SVG,
            PNG_ASSET => ICON_256,
            _ => return gpui_kit::AssetSource::load(&gpui_kit::assets::Assets, path),
        };
        Ok(Some(std::borrow::Cow::Borrowed(bytes)))
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut names = if path.is_empty() {
            vec![SVG_ASSET.into(), PNG_ASSET.into()]
        } else {
            Vec::new()
        };
        names.extend(gpui_kit::AssetSource::list(&gpui_kit::assets::Assets, path)?);
        Ok(names)
    }
}

/// Install application icons into the user-local icon theme.
///
/// Windows, desktop environments and the GPUI window icon look up icons by
/// app-id in the icon theme; writing the PNGs here makes `io.juicity.gui`
/// resolvable without root.  Safe to call multiple times; always overwrites
/// the files so they stay in sync with the binary.
pub fn install() {
    if let Err(err) = try_install() {
        tracing::warn!("could not install application icon: {err}");
    }
}

fn try_install() -> anyhow::Result<()> {
    use directories::ProjectDirs;

    let dirs = ProjectDirs::from("io", "juicity", "juicity-gui")
        .ok_or_else(|| anyhow::anyhow!("cannot determine project dirs"))?;

    // Icons are stored at <base>/hicolor/<size>x<size>/apps/<ICON_NAME>.png
    let base = dirs.data_local_dir().join("icons");

    let sizes: &[(&str, &[u8])] = &[
        ("16x16", ICON_16),
        ("32x32", ICON_32),
        ("48x48", ICON_48),
        ("64x64", ICON_64),
        ("128x128", ICON_128),
        ("256x256", ICON_256),
    ];

    for (size_dir, bytes) in sizes {
        let dir = base.join("hicolor").join(size_dir).join("apps");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{ICON_NAME}.png")), bytes)?;
    }

    Ok(())
}
