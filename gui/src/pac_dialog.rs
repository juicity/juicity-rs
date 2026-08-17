//! PAC Settings dialog — lets the user configure:
//!   - Rule list source URLs (direct-list / proxy-list)
//!   - Auto-update interval
//!   - PAC server listen address
//!   - Online PAC URL (bypasses local server)

use crate::app::AppView;
use crate::config::AppConfig;
use crate::pac;
use crate::widgets;
use gpui::prelude::*;
use gpui::{
    div, px, rgb, size, App, Bounds, ClickEvent, Context, ElementId, Entity, SharedString,
    WeakEntity, Window, WindowBounds, WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use rust_i18n::t;

/// Open the PAC settings dialog as its own window on top of the main view.
///
/// The dialog reads the current configuration from the persistent `AppView`
/// and writes changes back through `AppView::apply_pac_config` / the
/// "Update Now" handler.
pub fn open(owner: &WeakEntity<AppView>, cx: &mut App) {
    let Some(config) = owner.update(cx, |view, _| view.config_snapshot()).ok() else {
        return;
    };
    let owner = owner.clone();

    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(560.), px(520.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&t!("pac_dialog.title"));
                window.set_app_id("io.juicity.gui");
                let dialog = cx.new(|cx| PacDialog::new(owner, config, window, cx));
                cx.new(|cx| gpui_component::Root::new(dialog, window, cx))
            },
        )
        .ok();

    // Focus the dialog window.
    if let Some(handle) = handle {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .ok();
    }
}

/// Dialog state: the owner view plus editable fields.
pub struct PacDialog {
    owner: WeakEntity<AppView>,
    direct_url: Entity<InputState>,
    proxy_url: Entity<InputState>,
    interval: Entity<InputState>,
    listen_addr: Entity<InputState>,
    online_url: Entity<InputState>,
}

impl PacDialog {
    fn new(owner: WeakEntity<AppView>, cfg: AppConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mk = |window: &mut Window, cx: &mut Context<Self>, value: &str| -> Entity<InputState> {
            let state = cx.new(|cx| InputState::new(window, cx));
            state.update(cx, |s, scx| s.set_value(value.to_string(), window, scx));
            state
        };
        let direct_url = mk(window, cx, &cfg.pac_direct_url);
        let proxy_url = mk(window, cx, &cfg.pac_proxy_url);
        let interval = mk(window, cx, &cfg.pac_auto_update_hours.to_string());
        let listen_addr = mk(window, cx, &cfg.pac_listen);
        let online_url = mk(window, cx, &cfg.online_pac_url.unwrap_or_default());

        Self {
            owner,
            direct_url,
            proxy_url,
            interval,
            listen_addr,
            online_url,
        }
    }

    fn collect(&self, cx: &mut Context<Self>) -> AppConfig {
        let mut cfg = self
            .owner
            .update(cx, |view, _| view.config_snapshot())
            .ok()
            .unwrap_or_default();
        cfg.pac_direct_url = self.direct_url.read(cx).value().trim().to_string();
        cfg.pac_proxy_url = self.proxy_url.read(cx).value().trim().to_string();
        cfg.pac_auto_update_hours = self.interval.read(cx).value().trim().parse().unwrap_or(0);
        cfg.pac_listen = self.listen_addr.read(cx).value().trim().to_string();
        let url = self.online_url.read(cx).value().trim().to_string();
        cfg.online_pac_url = if url.is_empty() { None } else { Some(url) };
        cfg
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.collect(cx);
        let owner = self.owner.clone();
        owner
            .update(cx, |view, cx| view.apply_pac_config(cfg, cx))
            .ok();
        window.remove_window();
    }

    fn update_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.collect(cx);
        let owner = self.owner.clone();
        owner
            .update(cx, |view, cx| {
                view.apply_pac_config(cfg, cx);
                view.update_pac_rules_now(cx);
            })
            .ok();
        window.remove_window();
    }
}

impl Render for PacDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let pac_url = pac::pac_url(&self.listen_addr.read(cx).value());

        let group_header = |title: &str| {
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(0x24292f))
                .mb_1()
                .child(title.to_string())
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf6f8fa))
            .child(
                div()
                    .id("pac-scroll")
                    .flex_grow()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        group_header(&t!("pac_dialog.group_rules"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(widgets::field_row(
                        t!("pac_dialog.direct_url").to_string(),
                        Input::new(&self.direct_url),
                    ))
                    .child(widgets::field_row(
                        t!("pac_dialog.proxy_url").to_string(),
                        Input::new(&self.proxy_url),
                    ))
                    .child(separator())
                    .child(
                        group_header(&t!("pac_dialog.group_update"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(widgets::field_row(
                        t!("pac_dialog.update_interval").to_string(),
                        Input::new(&self.interval),
                    ))
                    .child(separator())
                    .child(
                        group_header(&t!("pac_dialog.group_server"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(widgets::field_row(
                        t!("pac_dialog.listen_addr").to_string(),
                        Input::new(&self.listen_addr),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .py_0p5()
                            .child(
                                div()
                                    .w(px(130.))
                                    .flex_none()
                                    .text_right()
                                    .text_color(rgb(0x57606a))
                                    .child(t!("pac_dialog.pac_url_label").to_string()),
                            )
                            .child(div().text_sm().text_color(rgb(0x0969da)).child(pac_url)),
                    )
                    .child(separator())
                    .child(
                        group_header(&t!("pac_dialog.group_online"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(widgets::field_row(
                        t!("pac_dialog.online_pac_url").to_string(),
                        Input::new(&self.online_url),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(rgb(0xd0d7de))
                    .bg(rgb(0xffffff))
                    .child(btn(
                        "pac-update-now",
                        t!("pac_dialog.update_now").to_string(),
                        false,
                        with_view(&this, PacDialog::update_now),
                    ))
                    .child(div().flex_grow())
                    .child(btn(
                        "pac-cancel",
                        t!("btn.cancel").to_string(),
                        false,
                        with_view(&this, PacDialog::cancel),
                    ))
                    .child(btn(
                        "pac-save",
                        t!("pac_dialog.save").to_string(),
                        true,
                        with_view(&this, PacDialog::save),
                    )),
            )
    }
}

impl PacDialog {
    fn cancel(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }
}

/// Build a gpui-component `Button`.
fn btn(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let b = Button::new(id).label(label);
    let b = if primary { b.primary() } else { b };
    b.on_click(on_click)
}

/// Build a click handler that routes to a `&mut self` view method.
fn with_view<F>(
    this: &WeakEntity<PacDialog>,
    f: F,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static
where
    F: Fn(&mut PacDialog, &mut Window, &mut Context<PacDialog>) + 'static,
{
    let this = this.clone();
    move |_e, window, cx| {
        this.update(cx, |view, cx| f(view, window, cx)).ok();
    }
}

/// Thin horizontal separator line.
fn separator() -> impl IntoElement {
    div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).my_1()
}
