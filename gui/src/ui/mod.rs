mod bind;
#[cfg(test)]
mod catalogs;
mod controller;
mod fmt;
mod fonts;
#[cfg(test)]
mod shots;

use crate::config::Storage;
use crate::i18n::{self, UiLang};
use controller::{Controller, NativeEffects};
use slint::ComponentHandle;

slint::include_modules!();

/// Translation, fonts and pure callbacks; no application data.
fn prepare(window: &MainWindow, language: UiLang) -> anyhow::Result<()> {
    slint::select_bundled_translation(language.slint_tag())?;
    window
        .global::<Theme>()
        .set_cjk(fonts::cjk_family(language).into());
    window.global::<Fmt>().on_elide_middle(fmt::elide_middle);
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
    slint::set_xdg_app_id("io.juicity.gui")?;
    let language = i18n::detect();
    fonts::register_medium(language)?;
    let window = MainWindow::new().map_err(|err| renderer_error(software, err))?;
    prepare(&window, language)?;

    let storage = Storage::new()?;
    let config_dir = storage.paths().config_dir.clone();
    bind::install(
        &window,
        Controller::new(storage, Box::new(NativeEffects::default())),
    );
    bind::watch_config_dir(&config_dir);
    bind::startup();

    // No tray yet: closing the window quits.
    window.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    if let Err(err) = window.show() {
        bind::shutdown();
        return Err(renderer_error(software, err));
    }
    let result = slint::run_event_loop_until_quit();
    bind::shutdown();
    result.map_err(|err| renderer_error(software, err))
}
