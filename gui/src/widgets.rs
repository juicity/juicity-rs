//! Small reusable GPUI layout helpers for the Juicity GUI.
//!
//! Text inputs, buttons, checkboxes and dropdowns are provided by the
//! `gpui-component` crate; this module keeps only the pure layout helper
//! [`field_row`] used to build the Shadowsocks-Windows-style editor rows.

use gpui::prelude::*;
use gpui::{div, px, rgb, SharedString};

/// Horizontal row: right-aligned fixed-width label + widget filling the rest.
pub fn field_row(
    label_text: impl Into<SharedString>,
    widget: impl gpui::IntoElement,
) -> impl gpui::IntoElement {
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
                .text_color(rgb(0x57606a))
                .child(label_text),
        )
        .child(widget)
}
