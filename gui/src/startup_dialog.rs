//! Startup Settings dialog — lets the user configure:
//!   - Hide window on startup
//!   - Connection state on startup: Off / On / LastState
//!   - Auto-start on boot

use adw::prelude::*;
use gtk::prelude::*;
use gtk4 as gtk;
use rust_i18n::t;

use crate::config::{RuntimeState, StartupConnectionState};

/// Open the startup settings dialog as a modal window on top of `parent`.
///
/// * `state`   – current runtime state snapshot for initial widget values.
/// * `on_save` – called with the updated `RuntimeState` when the user clicks Save.
pub fn open(
    parent: &gtk::ApplicationWindow,
    state: RuntimeState,
    on_save: impl Fn(RuntimeState) + 'static,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(&*t!("startup_dialog.title"))
        .default_width(420)
        .resizable(false)
        .build();

    // ── Root layout ───────────────────────────────────────────────────────
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    // ── "Hide window on startup" group ─────────────────────────────────────────
    let hide_group = adw::PreferencesGroup::builder()
        .title(&*t!("startup_dialog.group_hide_window"))
        .description(&*t!("startup_dialog.hide_window_desc"))
        .build();

    let hide_yes_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.yes"))
        .css_classes(["radio"])
        .group(&gtk::CheckButton::new())
        .build();
    let hide_no_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.no"))
        .css_classes(["radio"])
        .group(&hide_yes_radio)
        .active(true)
        .build();

    // Set initial value
    if state.hide_window_on_startup {
        hide_yes_radio.set_active(true);
    } else {
        hide_no_radio.set_active(true);
    }

    let hide_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_start(12)
        .margin_top(6)
        .build();
    hide_box.append(&hide_yes_radio);
    hide_box.append(&hide_no_radio);

    let hide_row = adw::ActionRow::new();
    hide_row.add_suffix(&hide_box);
    hide_group.add(&hide_row);
    content.append(&hide_group);

    // ── "Connection state on startup" group ─────────────────────────────────────────
    let conn_group = adw::PreferencesGroup::builder()
        .title(&*t!("startup_dialog.group_connection"))
        .description(&*t!("startup_dialog.connection_desc"))
        .build();

    let conn_off_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.connection_off"))
        .css_classes(["radio"])
        .group(&gtk::CheckButton::new())
        .build();
    let conn_on_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.connection_on"))
        .css_classes(["radio"])
        .group(&conn_off_radio)
        .build();
    let conn_last_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.connection_last"))
        .css_classes(["radio"])
        .group(&conn_off_radio)
        .build();

    // Set initial value
    match state.startup_connection_state {
        StartupConnectionState::On => conn_on_radio.set_active(true),
        StartupConnectionState::LastState => conn_last_radio.set_active(true),
        StartupConnectionState::Off => conn_off_radio.set_active(true),
    }

    let conn_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_start(12)
        .margin_top(6)
        .build();
    conn_box.append(&conn_off_radio);
    conn_box.append(&conn_on_radio);
    conn_box.append(&conn_last_radio);

    let conn_row = adw::ActionRow::new();
    conn_row.add_suffix(&conn_box);
    conn_group.add(&conn_row);
    content.append(&conn_group);

    // ── "Auto-start on boot" group ─────────────────────────────────────────────
    let autostart_group = adw::PreferencesGroup::builder()
        .title(&*t!("startup_dialog.group_autostart"))
        .description(&*t!("startup_dialog.autostart_desc"))
        .build();

    let autostart_yes_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.yes"))
        .css_classes(["radio"])
        .group(&gtk::CheckButton::new())
        .build();
    let autostart_no_radio = gtk::CheckButton::builder()
        .label(&*t!("startup_dialog.no"))
        .css_classes(["radio"])
        .group(&autostart_yes_radio)
        .active(true)
        .build();

    // Set initial value
    if state.auto_start {
        autostart_yes_radio.set_active(true);
    } else {
        autostart_no_radio.set_active(true);
    }

    let autostart_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_start(12)
        .margin_top(6)
        .build();
    autostart_box.append(&autostart_yes_radio);
    autostart_box.append(&autostart_no_radio);

    let autostart_row = adw::ActionRow::new();
    autostart_row.add_suffix(&autostart_box);
    autostart_group.add(&autostart_row);
    content.append(&autostart_group);

    root.append(&content);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // ── Bottom action bar ─────────────────────────────────────────────────
    let btn_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let spacer = gtk::Box::builder().hexpand(true).build();
    let cancel_btn = gtk::Button::with_label(&*t!("btn.cancel"));
    let save_btn = gtk::Button::with_label(&*t!("startup_dialog.save"));
    save_btn.add_css_class("suggested-action");

    btn_bar.append(&spacer);
    btn_bar.append(&cancel_btn);
    btn_bar.append(&save_btn);
    root.append(&btn_bar);

    window.set_child(Some(&root));

    // Helper: collect current widget values into a RuntimeState.
    let collect = {
        let state = state.clone();
        std::rc::Rc::new(move || -> RuntimeState {
            let mut s = state.clone();
            s.hide_window_on_startup = hide_yes_radio.is_active();
            if conn_on_radio.is_active() {
                s.startup_connection_state = StartupConnectionState::On;
            } else if conn_last_radio.is_active() {
                s.startup_connection_state = StartupConnectionState::LastState;
            } else {
                s.startup_connection_state = StartupConnectionState::Off;
            }
            s.auto_start = autostart_yes_radio.is_active();
            s
        })
    };

    let on_save = std::rc::Rc::new(on_save);

    // ── Cancel ────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| window.close());
    }

    // ── Save ──────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let collect = std::rc::Rc::clone(&collect);
        let on_save = std::rc::Rc::clone(&on_save);
        save_btn.connect_clicked(move |_| {
            on_save(collect());
            window.close();
        });
    }

    window.present();
}
