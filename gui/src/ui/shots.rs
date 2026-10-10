use super::controller::{traffic_view, LogsSnapshot};
use super::{
    bind, fonts, prepare, AppState, Connection, MainWindow, NodeDraft, NodeRow, NodeStore, Notice,
    PacRule, Page, Protocol, ProxyMode, SettingsStore, StartupConnection, Theme, Versions,
};
use crate::i18n::UiLang;
use crate::logging::LogLine;
use crate::traffic::{Speed, TrafficSnapshot};
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
/// The minimum window size (app.slint), rendered at scale 1 for review.
const MIN_WIDTH: u32 = 960;
const MIN_HEIGHT: u32 = 640;
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
    logs_fixture(window);
}

/// Fixed log lines (Logs page) and traffic history (Overview page).
fn logs_fixture(window: &MainWindow) {
    use tracing::Level;
    let line = |time: &str, level, target: &str, message: &str| LogLine {
        time: time.into(),
        level,
        target: target.into(),
        message: message.into(),
    };
    let lines = vec![
        line("00:00:00", Level::INFO, "juicity_gui", "juicity-gui 1.0.3 starting"),
        line("00:00:00", Level::INFO, "juicity_gui::pac", "PAC server listening on 127.0.0.1:1090"),
        line("00:00:01", Level::INFO, "juicity_gui::core", "starting in-process Juicity core for profile Tokyo 01"),
        line("00:00:01", Level::INFO, "juicity_client", "listening on 127.0.0.1:1080 (SOCKS5 / HTTP)"),
        line("00:00:01", Level::DEBUG, "juicity_client::quic", "handshake with tokyo.example.com:443 done in 84 ms, congestion control bbr"),
        line("00:00:02", Level::INFO, "juicity_gui::system_proxy", "system proxy set to PAC http://127.0.0.1:1090/pac"),
        line("00:00:09", Level::INFO, "juicity_gui::pac", "fetched https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/direct-list.txt (118243 rules) in 1.42 s"),
        line("00:01:15", Level::WARN, "juicity_client::quic", "connection to tokyo.example.com:443 lost: timed out; reconnecting"),
        line("00:01:16", Level::INFO, "juicity_client::quic", "connected to tokyo.example.com:443"),
        line("00:03:40", Level::DEBUG, "juicity_client::relay", "tcp 127.0.0.1:52144 -> www.example.org:443 closed, up=18.2 KB down=1.4 MB"),
        line("00:04:02", Level::ERROR, "juicity_client::relay", "udp relay for 1.1.1.1:53 failed: stream reset by peer"),
        line("00:05:27", Level::INFO, "juicity_client::relay", "tcp 127.0.0.1:52210 -> api.example.net:443 opened"),
        line("00:05:28", Level::INFO, "juicity_client::relay", "tcp 127.0.0.1:52211 -> cdn.example.net:443 opened"),
        line("00:06:51", Level::WARN, "juicity_gui::pac", "online PAC file unreachable, serving the local rules"),
        line("00:07:03", Level::INFO, "juicity_client::relay", "tcp 127.0.0.1:52240 -> www.example.org:443 opened"),
    ];
    let samples: Vec<Speed> = (0..44u32)
        .map(|i| Speed {
            down: f64::from((i * 37) % 11) * 96_000.0 + 12_000.0,
            up: f64::from((i * 23) % 7) * 60_000.0 + 8_000.0,
        })
        .collect();
    let traffic = TrafficSnapshot {
        peak_down: samples.iter().map(|s| s.down).fold(0.0, f64::max),
        peak_up: samples.iter().map(|s| s.up).fold(0.0, f64::max),
        current: *samples.last().unwrap(),
        samples,
        total_down: 186_413_056,
        total_up: 21_495_808,
    };
    let snapshot = LogsSnapshot {
        version: 1,
        lines,
        dropped: 120,
        follow: true,
    };
    bind::logs::sync_fixture(window, &snapshot, &traffic_view(&traffic));
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

/// Report-only renders at the minimum window size into `min/`: every page in
/// every language, plus Nodes under a notice banner. No goldens.
fn min_shots(window: &MinimalSoftwareWindow, output: &Path) -> anyhow::Result<()> {
    let output = output.join("min");
    std::fs::create_dir_all(&output)?;
    for language in [UiLang::En, UiLang::ZhCn, UiLang::ZhTw, UiLang::Ru] {
        let ui = MainWindow::new()?;
        prepare(&ui, language)?;
        fixture(&ui);
        // A long value shows that settings rows elide instead of overflowing.
        ui.global::<SettingsStore>().set_direct_url(
            "https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/direct-list.txt"
                .into(),
        );
        window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
        ui.show()?;
        window.set_size(PhysicalSize::new(MIN_WIDTH, MIN_HEIGHT));
        for (page, page_name) in [
            (Page::Overview, "overview"),
            (Page::Nodes, "nodes"),
            (Page::Nodes, "nodes-banner"),
            (Page::Logs, "logs"),
            (Page::Settings, "settings"),
        ] {
            let state = ui.global::<AppState>();
            state.set_page(page);
            if page_name == "nodes-banner" {
                state.set_notice(Notice::ProfilesChanged);
                state.set_notice_error(true);
            } else {
                state.set_notice(Notice::None);
            }
            let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(MIN_WIDTH, MIN_HEIGHT);
            window.request_redraw();
            anyhow::ensure!(
                window.draw_if_needed(|renderer| {
                    renderer.render(pixels.make_mut_slice(), MIN_WIDTH as usize);
                }),
                "Headless window did not render"
            );
            let path = output.join(format!("{page_name}--{}.png", language.slint_tag()));
            image::save_buffer(
                &path,
                pixels.as_bytes(),
                MIN_WIDTH,
                MIN_HEIGHT,
                image::ColorType::Rgb8,
            )?;
            println!("Rendered {}", path.display());
        }
        ui.hide()?;
    }
    Ok(())
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
    for language in [UiLang::En, UiLang::ZhCn, UiLang::ZhTw, UiLang::Ru] {
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
                // Unsaved node edits: the enabled footer, then the prompt.
                (Page::Nodes, "nodes-dirty", HEIGHT),
                (Page::Nodes, "save-prompt", HEIGHT),
                (Page::Logs, "logs", HEIGHT),
                (Page::Settings, "settings", SETTINGS_HEIGHT),
            ] {
                window.set_size(PhysicalSize::new(WIDTH, height));
                ui.global::<AppState>().set_page(page);
                ui.global::<NodeStore>()
                    .set_dirty(page_name == "nodes-dirty" || page_name == "save-prompt");
                ui.global::<AppState>()
                    .set_save_prompt(page_name == "save-prompt");
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
                if matches!(language, UiLang::En | UiLang::ZhTw | UiLang::Ru)
                    || page == Page::Settings
                {
                    failures.extend(compare_golden(
                        &root.join("tests/golden").join(&filename),
                        &output.join(format!("diff--golden--{filename}")),
                        &pixels,
                    )?);
                }
                // Only these pages have mockups.
                if language == UiLang::ZhTw
                    && ["overview", "nodes", "settings"].contains(&page_name)
                {
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
    min_shots(&window, &output)?;
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
