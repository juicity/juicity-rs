use std::path::Path;

/// Sizes rasterized from `icon.svg` and embedded into the binary.
///
/// 512 and 1024 are not used by the GUI itself; they exist so the macOS
/// bundler can assemble `icon.icns` from the build output without needing an
/// SVG converter.
const PNG_SIZES: [u32; 8] = [16, 32, 48, 64, 128, 256, 512, 1024];

/// Sizes packed into the Windows `.ico`; 256 is the largest dimension the ICO
/// format can express.
const ICO_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

fn main() {
    let gpui = std::env::var_os("CARGO_FEATURE_UI_GPUI").is_some();
    let slint = std::env::var_os("CARGO_FEATURE_UI_SLINT").is_some();
    if gpui == slint {
        return;
    }
    #[cfg(feature = "ui-slint")]
    if slint {
        slint_build::compile_with_config(
            "ui/app.slint",
            slint_build::CompilerConfiguration::new()
                // slint-build sets the domain to CARGO_PKG_NAME (juicity-gui).
                .with_default_translation_context(slint_build::DefaultTranslationContext::None)
                .with_bundled_translations("lang"),
        )
        .expect("Failed to compile Slint UI");
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let svg_path = format!("{manifest_dir}/icon.svg");
    let svg_data = std::fs::read(&svg_path).expect("gui/icon.svg not found");

    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&svg_data, &opt).expect("failed to parse icon.svg");

    let src_w = tree.size().width();
    let src_h = tree.size().height();

    let mut ico_images = Vec::new();
    for size in PNG_SIZES {
        let pixmap = rasterize(&tree, src_w, src_h, size);
        let out_path = format!("{out_dir}/{size}.png");
        pixmap.save_png(&out_path).expect("failed to save PNG");

        // For tray: also write raw ARGB32 big-endian (StatusNotifierItem format).
        // tiny-skia stores pixels as premultiplied RGBA; convert to ARGB big-endian.
        #[cfg(feature = "ui-gpui")]
        if matches!(size, 16 | 32 | 48) {
            let argb: Vec<u8> = pixmap
                .data()
                .chunks_exact(4)
                .flat_map(|p| [p[3], p[0], p[1], p[2]]) // RGBA → ARGB
                .collect();
            let raw_path = format!("{out_dir}/tray_{size}_argb.raw");
            std::fs::write(&raw_path, &argb).expect("failed to write raw tray icon");
        }

        if ICO_SIZES.contains(&size) {
            ico_images.push((size, pixmap));
        }
    }

    let ico_path = Path::new(&out_dir).join("icon.ico");
    write_ico(&ico_images, &ico_path);

    // On Windows GPUI takes the window icon from the executable's own icon
    // resource, so the icon has to end up inside the binary, not next to it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon_resource(Path::new(&out_dir), &ico_path);
    }

    // The About dialog reports the versions of the embedded protocol backends,
    // so read them from the lockfile to match what is actually linked.
    let lock = std::fs::read_to_string(format!("{manifest_dir}/../Cargo.lock")).unwrap_or_default();
    for (package, var) in [
        ("shadowsocks-service", "JUICITY_DEPS_SHADOWSOCKS_SERVICE"),
        ("juicity-client", "JUICITY_DEPS_JUICITY_CLIENT"),
    ] {
        let version = locked_version(&lock, package).unwrap_or_else(|| "unknown".to_string());
        println!("cargo:rustc-env={var}={version}");
    }

    println!("cargo:rerun-if-changed={manifest_dir}/icon.svg");
    println!("cargo:rerun-if-changed={manifest_dir}/../Cargo.lock");
}

/// Render `tree` into a square `size` × `size` pixmap.
fn rasterize(
    tree: &resvg::usvg::Tree,
    src_w: f32,
    src_h: f32,
    size: u32,
) -> resvg::tiny_skia::Pixmap {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("failed to create pixmap");
    let transform =
        resvg::tiny_skia::Transform::from_scale(size as f32 / src_w, size as f32 / src_h);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    pixmap
}

/// Write a multi-size Windows `.ico` from the rasterized icon.
///
/// Every entry stores an uncompressed 32-bit BGRA bitmap (bottom-up) followed by
/// an empty AND mask; that layout is understood by every resource compiler and
/// by every supported Windows release.
fn write_ico(images: &[(u32, resvg::tiny_skia::Pixmap)], path: &Path) {
    let mut payloads = Vec::with_capacity(images.len());
    for (size, pixmap) in images {
        let mut xor = Vec::with_capacity((size * size * 4) as usize);
        for row in (0..*size).rev() {
            for column in 0..*size {
                let color = pixmap
                    .pixel(column, row)
                    .expect("pixel is inside the pixmap")
                    .demultiply();
                xor.extend_from_slice(&[color.blue(), color.green(), color.red(), color.alpha()]);
            }
        }
        // AND mask rows are padded to 4 bytes; leaving it empty is fine because
        // the alpha channel of the 32-bit bitmap takes precedence.
        let mask_stride = size.div_ceil(32) * 4;
        let and = vec![0u8; (mask_stride * size) as usize];

        let mut image = Vec::with_capacity(40 + xor.len() + and.len());
        image.extend_from_slice(&40u32.to_le_bytes()); // biSize
        image.extend_from_slice(&(*size as i32).to_le_bytes()); // biWidth
        image.extend_from_slice(&(*size as i32 * 2).to_le_bytes()); // biHeight (XOR + AND)
        image.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        image.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        image.extend_from_slice(&0u32.to_le_bytes()); // biCompression (BI_RGB)
        image.extend_from_slice(&((xor.len() + and.len()) as u32).to_le_bytes()); // biSizeImage
        image.extend_from_slice(&[0u8; 16]); // resolution and palette
        image.extend_from_slice(&xor);
        image.extend_from_slice(&and);
        payloads.push(image);
    }

    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    ico.extend_from_slice(&(payloads.len() as u16).to_le_bytes());

    // The directory entries come first, so the payload offsets are known already.
    let mut offset = 6 + 16 * payloads.len() as u32;
    for ((size, _), payload) in images.iter().zip(&payloads) {
        let dimension = if *size >= 256 { 0 } else { *size as u8 }; // 0 means 256
        ico.push(dimension);
        ico.push(dimension);
        ico.push(0); // palette size
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += payload.len() as u32;
    }
    for payload in &payloads {
        ico.extend_from_slice(payload);
    }

    std::fs::write(path, ico).expect("failed to write icon.ico");
}

/// Compile the generated `.ico` into the executable as resource ID 1.
///
/// A missing resource compiler is reported as a warning only: the GUI runs fine
/// without an embedded icon, and the compilers used by the supported targets
/// (MSVC `rc.exe`, `windres`/`llvm-rc` from MSYS2) are part of the CI setup.
fn embed_icon_resource(out_dir: &Path, ico_path: &Path) {
    let rc_path = out_dir.join("juicity-gui.rc");
    // Resource files are line oriented, and forward slashes keep the path portable.
    let ico_literal = ico_path.display().to_string().replace('\\', "/");
    if let Err(err) = std::fs::write(&rc_path, format!("1 ICON \"{ico_literal}\"\n")) {
        println!("cargo:warning=could not write the icon resource script: {err}");
        return;
    }

    if let Err(err) = embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=could not embed the application icon: {err}");
    }
}

/// Look up the resolved version of `package` in a `Cargo.lock`.
fn locked_version(lock: &str, package: &str) -> Option<String> {
    let mut name = None;
    for line in lock.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"'));
        } else if let Some(value) = line.strip_prefix("version = ") {
            if name == Some(package) {
                return Some(value.trim_matches('"').to_string());
            }
        }
    }
    None
}
