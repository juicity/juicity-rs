//! About dialog — reachable only from the tray menu.
//!
//! Shows the application icon (`gui/icon.svg`, embedded into the binary by
//! `build.rs`) together with the version of the GUI, of the embedded
//! Shadowsocks-Rust backend (`shadowsocks-service`) and of the embedded
//! Juicity-RS backend (`juicity-client`).

use crate::icon;
use crate::widgets;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, img, px, rems, size, App, Bounds, ClickEvent, Context, ElementId, FontWeight,
    SharedString, Window, WindowBounds, WindowOptions,
};
use rust_i18n::t;

/// Versions reported by the dialog.  The dependency versions are injected by
/// `build.rs` from `Cargo.lock`, so they cannot drift from the linked crates.
pub struct Versions {
    /// Version of the GUI itself.
    pub app: &'static str,
    /// Release tag the binary was built from.
    pub tag: &'static str,
    /// Commit the binary was built from.
    pub commit: &'static str,
    /// Version of the embedded `shadowsocks-service` (Shadowsocks-Rust).
    pub shadowsocks: &'static str,
    /// Version of the embedded `juicity-client`.
    pub juicity: &'static str,
}

impl Versions {
    pub const fn current() -> Self {
        Self {
            app: env!("CARGO_PKG_VERSION"),
            tag: juicity_common::BuildInfo::GIT_TAG,
            commit: juicity_common::BuildInfo::GIT_HASH,
            shadowsocks: env!("JUICITY_DEPS_SHADOWSOCKS_SERVICE"),
            juicity: env!("JUICITY_DEPS_JUICITY_CLIENT"),
        }
    }

    /// Release tag plus the abbreviated commit the binary was built from.
    pub fn build(&self) -> String {
        format!("{} ({})", self.tag, short_commit(self.commit))
    }
}

/// Abbreviate a commit hash the way `git rev-parse --short` does.
fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Open the About dialog as its own window on top of the main view.
pub fn open(cx: &mut App) {
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(520.), px(440.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&t!("about_dialog.title"));
                window.set_app_id("io.juicity.gui");
                let dialog = cx.new(|_cx| AboutDialog);
                cx.new(|cx| gpui_kit::base::Root::new(dialog, window, cx))
            },
        )
        .ok();

    if let Some(handle) = handle {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .ok();
    }
}

/// The dialog holds no state: everything it shows is fixed at build time.
struct AboutDialog;

impl AboutDialog {
    fn close(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }
}

impl Render for AboutDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let colors = widgets::palette(cx);
        let versions = Versions::current();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .child(
                div()
                    .flex_grow(1.)
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .px_6()
                    .py_5()
                    .child(
                        // `svg()` only paints a monochrome alpha mask, which would
                        // flatten the logo's gradients; the embedded PNG keeps the
                        // full-colour artwork intact.
                        img(icon::PNG_ASSET)
                            .w(px(112.))
                            .h(px(112.)),
                    )
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.foreground)
                            .child(t!("about_dialog.app_name").to_string()),
                    )
                    .child(
                        div()
                            .w(px(420.))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(row(
                                colors,
                                t!("about_dialog.label_version").to_string(),
                                versions.app.to_string(),
                            ))
                            .child(row(
                                colors,
                                t!("about_dialog.label_build").to_string(),
                                versions.build(),
                            ))
                            .child(row(
                                colors,
                                t!("about_dialog.label_shadowsocks").to_string(),
                                versions.shadowsocks.to_string(),
                            ))
                            .child(row(
                                colors,
                                t!("about_dialog.label_juicity").to_string(),
                                versions.juicity.to_string(),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .bg(colors.panel)
                    .child(div().flex_grow(1.))
                    .child(btn(
                        "about-ok",
                        t!("btn.ok").to_string(),
                        true,
                        move |_e, window, cx| {
                            this.update(cx, |dialog, cx| dialog.close(window, cx)).ok();
                        },
                    )),
            )
    }
}

/// One `label: value` row for the About dialog details block.
///
/// The row is laid out around the dialog's centre line: the label column is
/// right-aligned and the value column left-aligned, with a gap two characters
/// wide straddling the centre (`1rem` is the base font size, so 1rem is about
/// two average characters at the small text size used here).
fn row(colors: widgets::Palette, label: String, value: String) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(rems(CENTER_GAP_REMS))
        .child(
            div()
                .flex_1()
                .text_right()
                .text_sm()
                .text_color(colors.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .text_left()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.foreground)
                .child(value),
        )
}

/// Width of the gap straddling the centre line, in `rem` units. It is roughly
/// two characters wide at the small text size used for the rows.
const CENTER_GAP_REMS: f32 = 1.0;

/// Build a gpui-kit `Button`.
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

#[cfg(test)]
mod tests {
    use super::{short_commit, Versions};

    #[test]
    fn reports_build_time_versions() {
        let versions = Versions::current();
        for value in [
            versions.app,
            versions.tag,
            versions.commit,
            versions.shadowsocks,
            versions.juicity,
        ] {
            assert!(!value.is_empty(), "every version field must be populated");
        }
    }

    #[test]
    fn abbreviates_commits() {
        assert_eq!(
            short_commit("4c4f9f0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f"),
            "4c4f9f0"
        );
        assert_eq!(short_commit("unknown"), "unknown");
        assert_eq!(short_commit("abc"), "abc");
    }
}
