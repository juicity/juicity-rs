fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let svg_path = format!("{manifest_dir}/icon.svg");
    let svg_data = std::fs::read(&svg_path).expect("gui/icon.svg not found");

    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&svg_data, &opt)
        .expect("failed to parse icon.svg");

    let src_w = tree.size().width();
    let src_h = tree.size().height();

    for size in [16u32, 32, 48, 64, 128, 256] {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)
            .expect("failed to create pixmap");
        let transform = resvg::tiny_skia::Transform::from_scale(
            size as f32 / src_w,
            size as f32 / src_h,
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let out_path = format!("{out_dir}/{size}.png");
        pixmap.save_png(&out_path).expect("failed to save PNG");

        // For tray: also write raw ARGB32 big-endian (StatusNotifierItem format).
        // tiny-skia stores pixels as premultiplied RGBA; convert to ARGB big-endian.
        if matches!(size, 16 | 32 | 48) {
            let argb: Vec<u8> = pixmap
                .data()
                .chunks_exact(4)
                .flat_map(|p| [p[3], p[0], p[1], p[2]]) // RGBA → ARGB
                .collect();
            let raw_path = format!("{out_dir}/tray_{size}_argb.raw");
            std::fs::write(&raw_path, &argb).expect("failed to write raw tray icon");
        }
    }

    println!("cargo:rerun-if-changed={manifest_dir}/icon.svg");
}
