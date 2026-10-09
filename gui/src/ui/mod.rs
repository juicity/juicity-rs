#[cfg(test)]
mod catalogs;
mod fmt;
mod fonts;
#[cfg(test)]
mod shots;

use crate::i18n::{self, UiLang};
use slint::ComponentHandle;

slint::include_modules!();

fn prepare(window: &MainWindow, language: UiLang) -> anyhow::Result<()> {
    slint::select_bundled_translation(language.slint_tag())?;
    window
        .global::<Theme>()
        .set_cjk(fonts::cjk_family(language).into());
    window.global::<Fmt>().on_elide_middle(fmt::elide_middle);
    let state = window.global::<AppState>();
    state.set_connection(Connection::Running);
    state.set_active_name("東京 01".into());
    state.set_active_address("juicity · tokyo.example.com:443".into());
    state.set_local_port("1080".into());
    state.set_local_host("127.0.0.1".into());
    state.set_pac_url("http://127.0.0.1:1090/pac".into());
    state.set_proxy_mode(ProxyMode::Pac);
    state.set_pac_rule(PacRule::BypassChina);
    state.set_rules_updated_at(window.get_fixture_updated_at());
    Ok(())
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
    let window = MainWindow::new()?;
    prepare(&window, language)?;
    window.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    window.run()?;
    Ok(())
}
