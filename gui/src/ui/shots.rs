use super::{fonts, prepare, AppState, Connection, MainWindow, PacRule, ProxyMode, Theme};
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
}

fn compare_mockup(
    path: &Path,
    output: &Path,
    pixels: &SharedPixelBuffer<Rgb8Pixel>,
) -> anyhow::Result<()> {
    let reference = image::open(path)?.to_rgb8();
    anyhow::ensure!(
        reference.dimensions() == (WIDTH, HEIGHT),
        "Mockup dimensions differ: {}",
        path.display()
    );
    let mut changed = 0usize;
    let mut diff = image::RgbImage::new(WIDTH, HEIGHT);
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
        changed as f64 * 100.0 / (WIDTH * HEIGHT) as f64,
        output.display()
    );
    std::fs::write(output.with_extension("txt"), &report)?;
    println!("{report}");
    Ok(())
}

#[test]
#[ignore = "Set JUICITY_SHOTS=1 to render the six overview fixtures"]
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
            let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(WIDTH, HEIGHT);
            window.dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: SCALE,
            });
            window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
            ui.show()?;
            window.request_redraw();
            anyhow::ensure!(
                window.draw_if_needed(|renderer| {
                    renderer.render(pixels.make_mut_slice(), WIDTH as usize);
                }),
                "Headless window did not render"
            );
            let theme = if dark { "dark" } else { "light" };
            let filename = format!("overview--{theme}--{}.png", language.slint_tag());
            let path = output.join(&filename);
            image::save_buffer(
                &path,
                pixels.as_bytes(),
                WIDTH,
                HEIGHT,
                image::ColorType::Rgb8,
            )?;
            println!("Rendered {}", path.display());
            if language == UiLang::ZhTw {
                compare_mockup(
                    &root.join(format!("tests/mockup/overview--{theme}--zh-TW.png")),
                    &output.join(format!("diff--overview--{theme}--zh_TW.png")),
                    &pixels,
                )?;
            }
            ui.hide()?;
        }
    }
    Ok(())
}
