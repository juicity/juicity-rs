//! "Save changes?" prompt shown when the editor is closed with unsaved edits.
//!
//! Opened by [`crate::app`] when the main window (or the whole app) is about to
//! close while the editor still holds changes the user has not saved.

use crate::app::{AppView, SaveChoice};
use crate::widgets;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, size, App, Bounds, ClickEvent, Context, ElementId, SharedString, WeakEntity, Window,
    WindowBounds, WindowOptions,
};
use rust_i18n::t;

/// Open the prompt.  `view` receives the user's [`SaveChoice`].
pub fn open(view: WeakEntity<AppView>, cx: &mut App) {
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(420.), px(170.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&t!("save_prompt.title"));
                window.set_app_id("io.juicity.gui");
                {
                    // Dismissing the prompt without choosing keeps editing, the
                    // same as the Cancel button.
                    let view = view.clone();
                    window.on_window_should_close(cx, move |_window, cx| {
                        view.update(cx, |view, cx| {
                            view.resolve_unsaved(SaveChoice::Cancel, cx)
                        })
                        .ok();
                        true
                    });
                }
                let dialog = cx.new(|_cx| SavePrompt { view });
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

struct SavePrompt {
    view: WeakEntity<AppView>,
}

impl SavePrompt {
    fn choose(&mut self, choice: SaveChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.view
            .update(cx, |view, cx| view.resolve_unsaved(choice, cx))
            .ok();
        window.remove_window();
    }
}

impl Render for SavePrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = widgets::palette(cx);
        let this = cx.weak_entity();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .child(
                div()
                    .flex_grow(1.)
                    .flex()
                    .items_center()
                    .px_4()
                    .text_sm()
                    .text_color(colors.foreground)
                    .child(t!("save_prompt.message").to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .bg(colors.panel)
                    .child(prompt_btn(
                        "save-prompt-save",
                        t!("save_prompt.save").to_string(),
                        true,
                        this.clone(),
                        SaveChoice::Save,
                    ))
                    .child(prompt_btn(
                        "save-prompt-discard",
                        t!("save_prompt.discard").to_string(),
                        false,
                        this.clone(),
                        SaveChoice::Discard,
                    ))
                    .child(prompt_btn(
                        "save-prompt-cancel",
                        t!("save_prompt.cancel").to_string(),
                        false,
                        this,
                        SaveChoice::Cancel,
                    )),
            )
    }
}

/// A button that hands `choice` back to the editor.
fn prompt_btn(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    this: WeakEntity<SavePrompt>,
    choice: SaveChoice,
) -> Button {
    let button = Button::new(id).label(label);
    let button = if primary { button.primary() } else { button };
    button.on_click(move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
        let _ = this.update(cx, |prompt, cx| prompt.choose(choice, window, cx));
    })
}
