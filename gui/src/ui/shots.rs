use super::{
    fonts, prepare, AppState, Connection, MainWindow, NodeDraft, NodeRow, NodeStore, PacRule, Page,
    Protocol, ProxyMode, SettingsStore, StartupConnection, Theme, Versions,
};
use crate::i18n::UiLang;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, PhysicalSize, Rgb8Pixel, SharedPixelBuffer};
use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

const WIDTH: u32 = 1440;
const HEIGHT: u32 = 960;
const SCALE: f32 = 1.5;
/// The settings mockup is 960 × 900 logical pixels.
const SETTINGS_HEIGHT: u32 = 1350;
/// A render fails when more than this share of its pixels differ from the
/// golden in any channel.
const GOLDEN_TOLERANCE: f64 = 0.002;

struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
    started: Instant,
}
impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Mockup fixture data (東京 01 …).
fn fixture(window: &MainWindow) {
    let state = window.global::<AppState>();
    state.set_connection(Connection::Running);
    state.set_active_name("東京 01".into());
    state.set_active_address("juicity · tokyo.example.com:443".into());
    state.set_local_port("1080".into());
    state.set_local_host("127.0.0.1".into());
    state.set_pac_url("http://127.0.0.1:1090/pac".into());
    state.set_proxy_mode(ProxyMode::Pac);
    state.set_pac_rule(PacRule::BypassChina);
    state.set_rules_age_days(0);
    state.set_rules_time("09:42".into());

    let row = |name: &str, address: &str, protocol| NodeRow {
        id: name.into(),
        name: name.into(),
        address: address.into(),
        protocol,
        in_use: name == "東京 01",
        group: "".into(),
    };
    let rows = vec![
        row("東京 01", "tokyo.example.com:443", Protocol::Juicity),
        row("東京 02", "tokyo2.example.com:443", Protocol::Juicity),
        row("大阪 01", "osaka.example.com:443", Protocol::Juicity),
        row("香港 01", "hk.example.com:8388", Protocol::Shadowsocks),
        row("新加坡 01", "sg.example.com:443", Protocol::Juicity),
    ];
    let nodes = window.global::<NodeStore>();
    nodes.set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    nodes.set_selected(0);
    nodes.set_draft(NodeDraft {
        name: "東京 01".into(),
        protocol: Protocol::Juicity,
        server: "tokyo.example.com".into(),
        port: "443".into(),
        uuid: "8f3c2a71-5b9e-4d0a-9c6f-2e7b1d4a6c90".into(),
        password: "correct-pass".into(),
        method: "chacha20-ietf-poly1305".into(),
        congestion_control: "bbr".into(),
        timeout: "5".into(),
        ..Default::default()
    });

    let settings = window.global::<SettingsStore>();
    settings.set_autostart(true);
    settings.set_close_to_tray(true);
    settings.set_startup_connection(StartupConnection::Last);
    settings.set_mixed_port("1080".into());
    settings.set_direct_url("Loyalsoldier · direct-list.txt".into());
    settings.set_proxy_url("Loyalsoldier · proxy-list.txt".into());
    settings.set_update_hours(24);
    settings.set_pac_listen("127.0.0.1:1090".into());
    settings.set_versions(Versions {
        gui: "1.0.3".into(),
        juicity: "1.0.3".into(),
        shadowsocks: "1.24.0".into(),
        tag: "v1.0.3".into(),
        commit: "4c4f9f0".into(),
    });
}

fn compare_mockup(
    path: &Path,
    output: &Path,
    pixels: &SharedPixelBuffer<Rgb8Pixel>,
) -> anyhow::Result<()> {
    let (width, height) = (pixels.width(), pixels.height());
    let reference = image::open(path)?.to_rgb8();
    anyhow::ensure!(
        reference.dimensions() == (width, height),
        "Mockup dimensions differ: {}",
        path.display()
    );
    let mut changed = 0usize;
    let mut diff = image::RgbImage::new(width, height);
    for ((x, y, expected), actual) in reference.enumerate_pixels().zip(pixels.as_slice()) {
        let delta = [
            expected[0].abs_diff(actual.r),
            expected[1].abs_diff(actual.g),
            expected[2].abs_diff(actual.b),
        ];
        if *delta.iter().max().unwrap() > 24 {
            changed += 1;
        }
        diff.put_pixel(x, y, image::Rgb(delta));
    }
    diff.save(output)?;
    let report = format!(
        "Mockup diff {}: {:.4}% (max channel delta >24); diff {}",
        path.display(),
        changed as f64 * 100.0 / (width * height) as f64,
        output.display()
    );
    std::fs::write(output.with_extension("txt"), &report)?;
    println!("{report}");
    Ok(())
}

/// Compare a render with its committed golden. Returns a failure message, and
/// writes a diff image next to the render when the golden has the same size.
fn compare_golden(
    path: &Path,
    output: &Path,
    pixels: &SharedPixelBuffer<Rgb8Pixel>,
) -> anyhow::Result<Option<String>> {
    let (width, height) = (pixels.width(), pixels.height());
    if !path.is_file() {
        return Ok(Some(format!("Missing golden {}", path.display())));
    }
    let golden = image::open(path)?.to_rgb8();
    if golden.dimensions() != (width, height) {
        return Ok(Some(format!(
            "Golden {} is {:?}, the render is {width}x{height}",
            path.display(),
            golden.dimensions()
        )));
    }
    let mut changed = 0usize;
    let mut diff = image::RgbImage::new(width, height);
    for ((x, y, expected), actual) in golden.enumerate_pixels().zip(pixels.as_slice()) {
        let delta = [
            expected[0].abs_diff(actual.r),
            expected[1].abs_diff(actual.g),
            expected[2].abs_diff(actual.b),
        ];
        if delta != [0; 3] {
            changed += 1;
        }
        diff.put_pixel(x, y, image::Rgb(delta));
    }
    let share = changed as f64 / (width * height) as f64;
    println!(
        "Golden diff {}: {:.4}% of pixels",
        path.display(),
        share * 100.0
    );
    if share <= GOLDEN_TOLERANCE {
        return Ok(None);
    }
    diff.save(output)?;
    Ok(Some(format!(
        "Golden {} differs in {:.4}% of pixels (limit {:.1}%); diff {}",
        path.display(),
        share * 100.0,
        GOLDEN_TOLERANCE * 100.0,
        output.display()
    )))
}

#[test]
#[ignore = "Set JUICITY_SHOTS=1 to render the fixtures and compare them with the goldens"]
fn shots() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("JUICITY_SHOTS").as_deref() == Ok("1"),
        "Set JUICITY_SHOTS=1 to run screenshots"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("../target"));
    let output = target.join("juicity-shots");
    std::fs::create_dir_all(&output)?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(HeadlessPlatform {
        window: window.clone(),
        started: Instant::now(),
    }))?;
    fonts::register_screenshot_fonts()?;
    let mut failures = Vec::new();
    for language in [UiLang::En, UiLang::ZhCn, UiLang::ZhTw] {
        for dark in [false, true] {
            let ui = MainWindow::new()?;
            prepare(&ui, language)?;
            fixture(&ui);
            ui.global::<Theme>().set_dark(dark);
            if language == UiLang::ZhTw {
                assert_eq!(
                    ui.global::<AppState>().get_connection_text(),
                    "已連線 · 東京 01"
                );
                assert_eq!(ui.global::<AppState>().get_rules_updated_at(), "今天 09:42");
            }
            window.dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: SCALE,
            });
            ui.show()?;
            for (page, page_name, height) in [
                (Page::Overview, "overview", HEIGHT),
                (Page::Nodes, "nodes", HEIGHT),
                (Page::Settings, "settings", SETTINGS_HEIGHT),
            ] {
                window.set_size(PhysicalSize::new(WIDTH, height));
                ui.global::<AppState>().set_page(page);
                let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(WIDTH, height);
                window.request_redraw();
                anyhow::ensure!(
                    window.draw_if_needed(|renderer| {
                        renderer.render(pixels.make_mut_slice(), WIDTH as usize);
                    }),
                    "Headless window did not render"
                );
                let theme = if dark { "dark" } else { "light" };
                let filename = format!("{page_name}--{theme}--{}.png", language.slint_tag());
                let path = output.join(&filename);
                image::save_buffer(
                    &path,
                    pixels.as_bytes(),
                    WIDTH,
                    height,
                    image::ColorType::Rgb8,
                )?;
                println!("Rendered {}", path.display());
                if matches!(language, UiLang::En | UiLang::ZhTw) {
                    failures.extend(compare_golden(
                        &root.join("tests/golden").join(&filename),
                        &output.join(format!("diff--golden--{filename}")),
                        &pixels,
                    )?);
                }
                if language == UiLang::ZhTw {
                    compare_mockup(
                        &root.join(format!("tests/mockup/{page_name}--{theme}--zh-TW.png")),
                        &output.join(format!("diff--{page_name}--{theme}--zh_TW.png")),
                        &pixels,
                    )?;
                }
            }
            ui.hide()?;
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
