mod bind;
#[cfg(test)]
mod catalogs;
mod controller;
mod fmt;
mod fonts;
#[cfg(test)]
mod shots;

use crate::config::Storage;
use crate::desktop::single_instance;
use crate::i18n::UiLang;
use controller::{Controller, NativeEffects};
use slint::ComponentHandle;

slint::include_modules!();

/// Translation, fonts and pure callbacks; no application data.
fn prepare(window: &MainWindow, language: UiLang) -> anyhow::Result<()> {
    apply_language(window, language)?;
    window.global::<Fmt>().on_elide_middle(fmt::elide_middle);
    window.global::<Fmt>().on_mask(fmt::mask);
    Ok(())
}

fn apply_language(window: &MainWindow, language: UiLang) -> anyhow::Result<()> {
    fonts::register_medium(language)?;
    slint::select_bundled_translation(language.slint_tag())?;
    window
        .global::<Theme>()
        .set_cjk(fonts::cjk_family(language).into());
    Ok(())
}

/// Name the fallback renderer when the OpenGL one could not open a window.
fn renderer_error(software: bool, err: impl Into<anyhow::Error>) -> anyhow::Error {
    let err = err.into();
    if software {
        err
    } else {
        err.context(
            "Could not open the window with the OpenGL renderer. \
             Retry with --software-render or SLINT_BACKEND=winit-software.",
        )
    }
}

#[cfg(not(target_os = "macos"))]
fn select_backend(software: bool) -> anyhow::Result<()> {
    let backend = slint::BackendSelector::new();
    let backend = if software {
        backend
            .backend_name("winit".into())
            .renderer_name("software".into())
    } else {
        backend
    };
    backend.select()?;
    Ok(())
}

/// On macOS the winit backend is built here because `BackendSelector`
/// cannot turn off Slint's default menu bar. Its Quit item sends AppKit
/// `terminate:`, which winit cannot cancel: it would skip the save prompt
/// and `bind::shutdown`. The tray module installs an application menu
/// whose Quit runs the same request as tray Quit instead. A `terminate:`
/// that remains (Dock Quit, logging out) ends the process from inside the
/// event loop, so `run_event_loop_until_quit` never returns;
/// [`ShutdownOnExit`] runs the shutdown there.
#[cfg(target_os = "macos")]
fn select_backend(software: bool) -> anyhow::Result<()> {
    let renderer = if software {
        Some("software".to_owned())
    } else {
        std::env::var("SLINT_BACKEND")
            .ok()
            .and_then(|value| env_renderer(&value))
    };
    let builder = i_slint_backend_winit::Backend::builder()
        .with_default_menu_bar(false)
        .with_custom_application_handler(Box::new(ShutdownOnExit));
    let builder = match renderer {
        Some(name) => builder.with_renderer_name(name),
        None => builder,
    };
    slint::platform::set_platform(Box::new(builder.build()?))?;
    Ok(())
}

/// The renderer named by `SLINT_BACKEND`, read as Slint's selector reads it
/// (`winit-software`, `software`, `skia`, ...). Winit is the only backend.
#[cfg(any(target_os = "macos", test))]
fn env_renderer(value: &str) -> Option<String> {
    let value = value.to_lowercase();
    let renderer = match value.split_once('-') {
        Some((_, renderer)) => renderer,
        None => match value.as_str() {
            "sw" | "software" => "software",
            "femtovg" | "skia" | "vello" => value.as_str(),
            _ => "",
        },
    };
    (!renderer.is_empty()).then(|| renderer.to_owned())
}

/// Runs `bind::shutdown` when winit's loop exits, including through
/// `applicationWillTerminate:`. It runs again after a normal return from the
/// event loop and does nothing then.
#[cfg(target_os = "macos")]
struct ShutdownOnExit;

#[cfg(target_os = "macos")]
impl i_slint_backend_winit::CustomApplicationHandler for ShutdownOnExit {
    fn exiting(
        &mut self,
        _event_loop: &i_slint_backend_winit::winit::event_loop::ActiveEventLoop,
    ) -> i_slint_backend_winit::EventResult {
        bind::shutdown();
        i_slint_backend_winit::EventResult::Propagate
    }
}

pub fn run() -> anyhow::Result<()> {
    let software = std::env::args().any(|arg| arg == "--software-render");
    select_backend(software)?;
    slint::set_xdg_app_id(crate::desktop::tray::APP_ID)?;
    #[cfg(target_os = "linux")]
    crate::desktop::integration::install();
    // Before any side effect: a second launch shows the first window and exits.
    let on_activate = Box::new(|| {
        if let Err(err) = slint::invoke_from_event_loop(bind::request_activation) {
            tracing::warn!("event loop is gone: {err}");
        }
    });
    let _instance = match single_instance::acquire(on_activate) {
        single_instance::Startup::Primary(instance) => instance,
        single_instance::Startup::Secondary => return Ok(()),
    };
    let storage = Storage::new()?;
    let config_dir = storage.paths().config_dir.clone();
    let controller = Controller::new(storage, Box::new(NativeEffects::default()));
    let language = controller.language().resolve();
    fonts::register_medium(language)?;
    let window = MainWindow::new().map_err(|err| renderer_error(software, err))?;
    prepare(&window, language)?;

    bind::install(&window, controller);
    bind::watch_config_dir(&config_dir);
    bind::startup();
    bind::start_tray();

    if let Err(err) = bind::show_initial(&window) {
        bind::shutdown();
        return Err(renderer_error(software, err));
    }
    bind::window_ready();
    let result = slint::run_event_loop_until_quit();
    bind::shutdown();
    result.map_err(|err| renderer_error(software, err))
}

#[cfg(test)]
mod tests {
    use super::env_renderer;

    #[test]
    fn env_renderer_matches_slint_backend_values() {
        assert_eq!(env_renderer("winit-software").as_deref(), Some("software"));
        assert_eq!(env_renderer("Winit-Skia").as_deref(), Some("skia"));
        assert_eq!(env_renderer("sw").as_deref(), Some("software"));
        assert_eq!(env_renderer("femtovg").as_deref(), Some("femtovg"));
        assert_eq!(env_renderer("winit"), None);
        assert_eq!(env_renderer(""), None);
    }
}
