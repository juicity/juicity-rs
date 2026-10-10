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

pub fn run() -> anyhow::Result<()> {
    let software = std::env::args().any(|arg| arg == "--software-render");
    let backend = slint::BackendSelector::new();
    let backend = if software {
        backend
            .backend_name("winit".into())
            .renderer_name("software".into())
    } else {
        backend
    };
    backend.select()?;
    slint::set_xdg_app_id(crate::desktop::tray::APP_ID)?;
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
