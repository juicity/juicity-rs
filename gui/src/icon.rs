/// Application icon name as registered in the icon theme.
pub const ICON_NAME: &str = "io.juicity.gui";

// PNG bytes at standard sizes, generated from icon.svg by build.rs.
const ICON_16: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/16.png"));
const ICON_32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/32.png"));
const ICON_48: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/48.png"));
const ICON_64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/64.png"));
const ICON_128: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/128.png"));
const ICON_256: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/256.png"));

/// Install application icons into the user-local icon theme.
///
/// Desktop environments and the tray look up icons by app-id in the icon
/// theme; writing the PNGs here makes `io.juicity.gui` resolvable without
/// root.  Safe to call multiple times; always overwrites
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
