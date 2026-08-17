//! Startup Settings dialog — lets the user configure:
//!   - Hide window on startup
//!   - Connection state on startup: Off / On / LastState
//!   - Auto-start on boot

use crate::app::AppView;
use crate::config::{RuntimeState, StartupConnectionState};
use gpui::prelude::*;
use gpui::{
    div, px, rgb, size, App, Bounds, ClickEvent, Context, ElementId, SharedString, WeakEntity,
    Window, WindowBounds, WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use rust_i18n::t;

/// Open the startup settings dialog as its own window.
pub fn open(owner: &WeakEntity<AppView>, cx: &mut App) {
    let Some(state) = owner.update(cx, |view, _| view.runtime_snapshot()).ok() else {
        return;
    };
    let owner = owner.clone();

    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(460.), px(420.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&t!("startup_dialog.title"));
                window.set_app_id("io.juicity.gui");
                let dialog = cx.new(|cx| StartupDialog::new(owner, state, cx));
                cx.new(|cx| gpui_component::Root::new(dialog, window, cx))
            },
        )
        .ok();

    if let Some(handle) = handle {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .ok();
    }
}

/// Dialog state: the owner view plus editable fields.
pub struct StartupDialog {
    owner: WeakEntity<AppView>,
    hide_window: bool,
    connection_state: StartupConnectionState,
    auto_start: bool,
}

impl StartupDialog {
    fn new(owner: WeakEntity<AppView>, state: RuntimeState, _cx: &mut Context<Self>) -> Self {
        Self {
            owner,
            hide_window: state.hide_window_on_startup,
            connection_state: state.startup_connection_state,
            auto_start: state.auto_start,
        }
    }

    fn set_connection_state(&mut self, state: StartupConnectionState, cx: &mut Context<Self>) {
        self.connection_state = state;
        cx.notify();
    }

    fn toggle_hide_window(&mut self, cx: &mut Context<Self>) {
        self.hide_window = !self.hide_window;
        cx.notify();
    }

    fn toggle_auto_start(&mut self, cx: &mut Context<Self>) {
        self.auto_start = !self.auto_start;
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = RuntimeState {
            hide_window_on_startup: self.hide_window,
            startup_connection_state: self.connection_state,
            auto_start: self.auto_start,
            ..self
                .owner
                .update(cx, |view, _| view.runtime_snapshot())
                .ok()
                .unwrap_or_default()
        };
        let owner = self.owner.clone();
        owner
            .update(cx, |view, cx| view.apply_runtime_state(state, cx))
            .ok();
        window.remove_window();
    }

    fn cancel(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }
}

impl Render for StartupDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();

        let group_header = |title: &str| {
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(0x24292f))
                .mb_1()
                .child(title.to_string())
        };

        let conn_options = [
            StartupConnectionState::Off,
            StartupConnectionState::On,
            StartupConnectionState::LastState,
        ];

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf6f8fa))
            .child(
                div()
                    .id("startup-scroll")
                    .flex_grow()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        group_header(&t!("startup_dialog.group_hide_window"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(div().pl(px(8.)).child(chk(
                        "startup-hide-window",
                        t!("startup_dialog.hide_window_desc").to_string(),
                        self.hide_window,
                        {
                            let this = this.clone();
                            move |_checked, _window, cx| {
                                let _ = this.update(cx, |view, cx| view.toggle_hide_window(cx));
                            }
                        },
                    )))
                    .child(separator())
                    .child(
                        group_header(&t!("startup_dialog.group_connection"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .children(conn_options.iter().map(|&option| {
                                let selected = option == self.connection_state;
                                let label = option.label();
                                let this = this.clone();
                                btn(
                                    ("startup-conn", option.index()),
                                    label,
                                    selected,
                                    move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                        this.update(cx, |dialog, cx| {
                                            dialog.set_connection_state(option, cx);
                                        })
                                        .ok();
                                    },
                                )
                            })),
                    )
                    .child(separator())
                    .child(
                        group_header(&t!("startup_dialog.group_autostart"))
                            .child(div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).mt_1()),
                    )
                    .child(div().pl(px(8.)).child(chk(
                        "startup-auto-start",
                        t!("startup_dialog.autostart_desc").to_string(),
                        self.auto_start,
                        {
                            let this = this.clone();
                            move |_checked, _window, cx| {
                                let _ = this.update(cx, |view, cx| view.toggle_auto_start(cx));
                            }
                        },
                    )),
            ))
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
                    .child(div().flex_grow())
                    .child(btn(
                        "startup-cancel",
                        t!("btn.cancel").to_string(),
                        false,
                        {
                            let this = this.clone();
                            move |_e, window, cx| {
                                this.update(cx, |dialog, cx| dialog.cancel(window, cx)).ok();
                            }
                        },
                    ))
                    .child(btn(
                        "startup-save",
                        t!("startup_dialog.save").to_string(),
                        true,
                        {
                            let this = this.clone();
                            move |_e, window, cx| {
                                this.update(cx, |dialog, cx| dialog.save(window, cx)).ok();
                            }
                        },
                    )),
            )
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

/// Build a gpui-component `Checkbox`.
fn chk(
    id: impl Into<ElementId>,
    label: impl Into<gpui_component::text::Text>,
    checked: bool,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Checkbox {
    Checkbox::new(id).label(label).checked(checked).on_click(on_click)
}

/// Thin horizontal separator line.
fn separator() -> impl IntoElement {
    div().h(px(1.)).w_full().bg(rgb(0xe0e0e0)).my_1()
}
