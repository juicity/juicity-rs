//! Small reusable GPUI layout helpers for the Juicity GUI.
//!
//! Text inputs, buttons, checkboxes and dropdowns are provided by the
//! `gpui-kit` crate; this module keeps the pure layout helper [`field_row`]
//! used to build the Shadowsocks-Windows-style editor rows, plus [`Palette`],
//! the snapshot of theme colours the GUI paints its own elements with.

use gpui_kit::component::Theme;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, App, Hsla, SharedString};

/// A snapshot of the theme colours the GUI paints its own elements with.
///
/// It is `Copy` so render closures can capture it freely, and it is taken once
/// per render so light and dark themes stay in step.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Window and panel background.
    pub background: Hsla,
    /// Subtle panel background (dialogs, toolbars, sidebars).
    pub panel: Hsla,
    /// Borders and separator lines.
    pub border: Hsla,
    /// Default text colour.
    pub foreground: Hsla,
    /// Muted text colour, used for form labels and secondary status text.
    pub muted_foreground: Hsla,
    /// Link and accent text colour.
    pub link: Hsla,
    /// System accent colour, used as the selected list-row background.
    pub accent: Hsla,
    /// Readable foreground on top of [`Palette::accent`].
    pub accent_foreground: Hsla,
    /// Background of a hovered list row.
    pub list_hover: Hsla,
}

/// Snapshot the current theme colours.
pub fn palette(app: &App) -> Palette {
    let theme = Theme::global(app);
    Palette {
        background: theme.background,
        panel: theme.secondary,
        border: theme.border,
        foreground: theme.foreground,
        muted_foreground: theme.muted_foreground,
        link: theme.link,
        accent: theme.accent,
        accent_foreground: theme.accent_foreground,
        list_hover: theme.list_hover,
    }
}

/// Horizontal row: right-aligned fixed-width label + widget filling the rest.
pub fn field_row(
    colors: Palette,
    label_text: impl Into<SharedString>,
    widget: impl gpui_kit::IntoElement,
) -> impl gpui_kit::IntoElement {
    let label_text: SharedString = label_text.into();
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
                .text_color(colors.muted_foreground)
                .child(label_text),
        )
        .child(widget)
}
