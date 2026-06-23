use crate::config::{
    ProfileStore, ProxyProfile, ProxyProtocol, RuntimeState, StartupConnectionState, Storage,
    SS_METHODS, method_to_index,
};
use crate::core::CoreManager;
use crate::link;
use crate::pac;
use crate::system_proxy;
use crate::tray::{self, TrayEvent, TraySharedState};
use adw::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use rust_i18n::t;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct GuiState {
    storage: Storage,
    config: crate::config::AppConfig,
    profiles: ProfileStore,
    runtime: RuntimeState,
    core_manager: CoreManager,
    pac_server: Option<pac::PacServer>,
    pac_update_rx: Option<std::sync::mpsc::Receiver<anyhow::Result<()>>>,
    _tray_service: Option<tray::TrayService>,
}

impl GuiState {
    fn new() -> anyhow::Result<Self> {
        let storage = Storage::new()?;
        let config = storage.load_app_config()?;
        let mut profiles = storage.load_profiles()?;
        let mut runtime = storage.load_runtime_state()?;

        if profiles.profiles.is_empty() {
            profiles.profiles.push(ProxyProfile::default());
            runtime.selected_profile = 0;
        }

        Ok(Self {
            storage,
            config,
            profiles,
            runtime,
            core_manager: CoreManager::new(),
            pac_server: None,
            pac_update_rx: None,
            _tray_service: None,
        })
    }

    fn flush(&self) -> anyhow::Result<()> {
        self.storage.save_app_config(&self.config)?;
        self.storage.save_profiles(&self.profiles)?;
        self.storage.save_runtime_state(&self.runtime)?;
        Ok(())
    }

    fn selected_profile(&self) -> Option<&ProxyProfile> {
        self.profiles.profiles.get(self.runtime.selected_profile)
    }

    fn selected_profile_mut(&mut self) -> Option<&mut ProxyProfile> {
        self.profiles.profiles.get_mut(self.runtime.selected_profile)
    }

    fn normalize_selected_index(&mut self) {
        if self.profiles.profiles.is_empty() {
            self.profiles.profiles.push(ProxyProfile::default());
        }
        if self.runtime.selected_profile >= self.profiles.profiles.len() {
            self.runtime.selected_profile = self.profiles.profiles.len().saturating_sub(1);
        }
    }
}

/// Holds references to all editor widgets so that `load_fields` / `save_fields`
/// can be written as plain functions instead of giant closures.
#[derive(Clone)]
struct FieldWidgets {
    protocol_dropdown: gtk::DropDown,
    server_entry: gtk::Entry,
    port_entry: gtk::Entry,
    password_entry: gtk::Entry,
    uuid_entry: gtk::Entry,
    sni_entry: gtk::Entry,
    allow_insecure_check: gtk::CheckButton,
    method_dropdown: gtk::DropDown,
    plugin_entry: gtk::Entry,
    plugin_opts_entry: gtk::Entry,
    need_plugin_arg: gtk::CheckButton,
    plugin_args_entry: gtk::Entry,
    plugin_args_row: gtk::Box,
    remarks_entry: gtk::Entry,
    timeout_entry: gtk::Entry,
    group_entry: gtk::Entry,
    proxy_port_entry: gtk::Entry,
    close_to_tray_check: gtk::CheckButton,
    juicity_box: gtk::Box,
    ss_box: gtk::Box,
    status_label: gtk::Label,
}

/// Restart or update the PAC server with fresh rules from disk.
///
/// If `force_restart` is `true` (e.g. the listen address changed), a new
/// server is started even if one already exists.  Otherwise the existing
/// server is updated in-place, or a new one is started if none exists.
fn restart_pac_server(state: &mut GuiState, force_restart: bool) -> anyhow::Result<()> {
    let (direct, proxy) = pac::load_rules(&state.storage.paths().config_dir);
    let content = pac::generate_pac(
        state.config.pac_rule_mode,
        &state.config.socks_listen,
        &direct,
        &proxy,
    );
    if force_restart || state.pac_server.is_none() {
        // Start fresh — needed when the listen address changed or on first launch.
        state.pac_server = Some(pac::start(&state.config.pac_listen, content)?);
    } else if let Some(srv) = &state.pac_server {
        // Update existing server in-place.
        srv.update(content);
    }
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    let app = adw::Application::builder()
        .application_id("io.juicity.gui")
        .build();

    app.connect_activate(|app| {
        // Single-instance: if a window already exists (second activation from
        // another process via D-Bus), just bring it to the foreground.
        if let Some(win) = app.windows().first() {
            win.present();
            return;
        }
        if let Err(err) = build_ui(app) {
            tracing::error!("failed to initialize UI: {err:?}");
        }
    });

    app.run();
    Ok(())
}

fn build_ui(app: &adw::Application) -> anyhow::Result<()> {
    let state = Rc::new(RefCell::new(GuiState::new()?));

    // ── PAC server: start once at launch, keep alive for the app lifetime ─
    {
        let mut s = state.borrow_mut();
        if let Err(err) = restart_pac_server(&mut s, true) {
            tracing::warn!("PAC server failed to start: {err}");
        }
    }
    // ── Window ────────────────────────────────────────────────────────────
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(&*t!("window.title"))
        .default_width(530)
        .default_height(440)
        .resizable(true)
        .build();

    // Install application icon into the user-local icon theme.
    if let Some(display) = gtk::gdk::Display::default() {
        crate::icon::install(&display);
    }
    window.set_icon_name(Some(crate::icon::ICON_NAME));

    let outer_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    // ── Content area: left list + right details ───────────────────────────
    let content_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .vexpand(true)
        .build();

    // ── Left panel: server ListBox ────────────────────────────────────────
    let left_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(160)
        .build();

    let servers_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let servers_listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    servers_listbox.add_css_class("navigation-sidebar");
    servers_scroll.set_child(Some(&servers_listbox));

    // Buttons: Add / Delete
    let btns1 = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_start(4).margin_end(4).margin_top(4).margin_bottom(2)
        .build();
    let add_btn = gtk::Button::with_label(&*t!("btn.add"));
    let del_btn = gtk::Button::with_label(&*t!("btn.delete"));
    add_btn.set_hexpand(true);
    del_btn.set_hexpand(true);
    btns1.append(&add_btn);
    btns1.append(&del_btn);

    // Buttons: Duplicate / Move Up / Move Down
    let btns2 = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_start(4).margin_end(4).margin_top(2).margin_bottom(4)
        .build();
    let dup_btn = gtk::Button::with_label(&*t!("btn.duplicate"));
    let up_btn = gtk::Button::with_label(&*t!("btn.up"));
    let dn_btn = gtk::Button::with_label(&*t!("btn.down"));
    dup_btn.set_hexpand(true);
    up_btn.set_hexpand(true);
    dn_btn.set_hexpand(true);
    btns2.append(&dup_btn);
    btns2.append(&up_btn);
    btns2.append(&dn_btn);

    left_box.append(&servers_scroll);
    left_box.append(&btns1);
    left_box.append(&btns2);

    let left_sep = gtk::Separator::new(gtk::Orientation::Vertical);

    // ── Right panel: detail editor ────────────────────────────────────────
    let right_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();

    let right_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();

    // "Server" section header
    let server_hdr = gtk::Label::builder()
        .label(&*t!("field.server_hdr"))
        .xalign(0.0)
        .margin_bottom(6)
        .build();
    server_hdr.add_css_class("heading");
    right_box.append(&server_hdr);

    // Protocol row
    let protocol_dropdown = gtk::DropDown::from_strings(&[
        &*t!("protocol.juicity"),
        &*t!("protocol.shadowsocks"),
    ]);
    protocol_dropdown.set_hexpand(true);
    right_box.append(&make_field_row(&t!("field.protocol"), &protocol_dropdown));

    // Server IP
    let server_entry = gtk::Entry::builder().hexpand(true).build();
    right_box.append(&make_field_row(&t!("field.server_ip"), &server_entry));

    // Server Port
    let port_entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .max_length(5)
        .width_chars(8)
        .build();
    right_box.append(&make_field_row(&t!("field.server_port"), &port_entry));

    // Password + Show Password checkbox on same row
    let pwd_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    let pwd_lbl = gtk::Label::builder()
        .label(&*t!("field.password"))
        .xalign(1.0)
        .width_chars(14)
        .build();
    let password_entry = gtk::Entry::builder()
        .visibility(false)
        .hexpand(true)
        .build();
    let show_pwd_btn = gtk::CheckButton::with_label(&*t!("field.show_password"));
    pwd_row.append(&pwd_lbl);
    pwd_row.append(&password_entry);
    pwd_row.append(&show_pwd_btn);
    right_box.append(&pwd_row);

    // ── Juicity-specific section ──────────────────────────────────────────
    let juicity_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    juicity_box.append(
        &gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(4)
            .margin_bottom(4)
            .build(),
    );
    let uuid_entry = gtk::Entry::builder().hexpand(true).build();
    juicity_box.append(&make_field_row(&t!("field.uuid"), &uuid_entry));
    let sni_entry = gtk::Entry::builder().hexpand(true).build();
    juicity_box.append(&make_field_row(&t!("field.sni"), &sni_entry));
    let allow_insecure_check = gtk::CheckButton::builder()
        .label(&*t!("field.allow_insecure"))
        .margin_start(122)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    juicity_box.append(&allow_insecure_check);
    right_box.append(&juicity_box);

    // ── Shadowsocks-specific section ──────────────────────────────────────
    let ss_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    ss_box.append(
        &gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(4)
            .margin_bottom(4)
            .build(),
    );
    let method_dropdown = gtk::DropDown::from_strings(SS_METHODS);
    method_dropdown.set_hexpand(true);
    ss_box.append(&make_field_row(&t!("field.encryption"), &method_dropdown));
    let plugin_entry = gtk::Entry::builder().hexpand(true).build();
    ss_box.append(&make_field_row(&t!("field.plugin_program"), &plugin_entry));
    let plugin_opts_entry = gtk::Entry::builder().hexpand(true).build();
    ss_box.append(&make_field_row(&t!("field.plugin_options"), &plugin_opts_entry));
    let need_plugin_arg = gtk::CheckButton::builder()
        .label(&*t!("field.need_plugin_arg"))
        .margin_start(122)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    ss_box.append(&need_plugin_arg);
    let plugin_args_entry = gtk::Entry::builder().hexpand(true).build();
    let plugin_args_row = make_field_row(&t!("field.plugin_args"), &plugin_args_entry);
    plugin_args_row.set_visible(false);
    ss_box.append(&plugin_args_row);
    right_box.append(&ss_box);
    ss_box.set_visible(false); // Juicity is default

    // ── Common tail fields ────────────────────────────────────────────────
    right_box.append(
        &gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(4)
            .margin_bottom(4)
            .build(),
    );
    let remarks_entry = gtk::Entry::builder().hexpand(true).build();
    right_box.append(&make_field_row(&t!("field.remarks"), &remarks_entry));
    let timeout_entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .width_chars(6)
        .build();
    right_box.append(&make_field_row(&t!("field.timeout"), &timeout_entry));
    let group_entry = gtk::Entry::builder().hexpand(true).build();
    right_box.append(&make_field_row(&t!("field.group"), &group_entry));

    right_scroll.set_child(Some(&right_box));
    content_box.append(&left_box);
    content_box.append(&left_sep);
    content_box.append(&right_scroll);
    outer_box.append(&content_box);

    // ── Status bar ────────────────────────────────────────────────────────
    outer_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let status_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(8).margin_end(8).margin_top(4).margin_bottom(4)
        .build();
    let status_label = gtk::Label::builder()
        .label(&*t!("status.stopped"))
        .xalign(0.0)
        .hexpand(true)
        .build();
    let start_btn = gtk::Button::with_label(&*t!("btn.start"));
    let stop_btn = gtk::Button::with_label(&*t!("btn.stop"));
    status_bar.append(&status_label);
    status_bar.append(&start_btn);
    status_bar.append(&stop_btn);
    outer_box.append(&status_bar);

    // ── Bottom bar ────────────────────────────────────────────────────────
    outer_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let bottom_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(8).margin_end(8).margin_top(6).margin_bottom(8)
        .build();
    let proxy_port_entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .width_chars(5)
        .build();
    let close_to_tray_check = gtk::CheckButton::with_label(&*t!("field.close_to_tray"));
    let pac_settings_btn = gtk::Button::with_label(&*t!("btn.pac_settings"));
    let spacer = gtk::Box::builder().hexpand(true).build();
    let import_btn = gtk::Button::with_label(&*t!("btn.import_url"));
    let export_btn = gtk::Button::with_label(&*t!("btn.export_url"));
    let ok_btn = gtk::Button::with_label(&*t!("btn.ok"));
    let cancel_btn = gtk::Button::with_label(&*t!("btn.cancel"));
    let apply_btn = gtk::Button::with_label(&*t!("btn.apply"));
    ok_btn.add_css_class("suggested-action");
    bottom_bar.append(&gtk::Label::new(Some(&*t!("field.proxy_port"))));
    bottom_bar.append(&proxy_port_entry);
    bottom_bar.append(&close_to_tray_check);
    bottom_bar.append(&pac_settings_btn);
    bottom_bar.append(&spacer);
    bottom_bar.append(&import_btn);
    bottom_bar.append(&export_btn);
    bottom_bar.append(&ok_btn);
    bottom_bar.append(&cancel_btn);
    bottom_bar.append(&apply_btn);
    outer_box.append(&bottom_bar);

    window.set_child(Some(&outer_box));

    // ── Shared tray state ─────────────────────────────────────────────────
    let tray_shared: Arc<Mutex<TraySharedState>> = Arc::new(Mutex::new(TraySharedState::default()));

    // ── Suspend flag: blocks list row-selected signal during repopulation ─
    let suspend_list = Rc::new(Cell::new(false));

    // ── FieldWidgets: collect all editor-widget references ─────────────────
    let field_widgets = FieldWidgets {
        protocol_dropdown: protocol_dropdown.clone(),
        server_entry: server_entry.clone(),
        port_entry: port_entry.clone(),
        password_entry: password_entry.clone(),
        uuid_entry: uuid_entry.clone(),
        sni_entry: sni_entry.clone(),
        allow_insecure_check: allow_insecure_check.clone(),
        method_dropdown: method_dropdown.clone(),
        plugin_entry: plugin_entry.clone(),
        plugin_opts_entry: plugin_opts_entry.clone(),
        need_plugin_arg: need_plugin_arg.clone(),
        plugin_args_entry: plugin_args_entry.clone(),
        plugin_args_row: plugin_args_row.clone(),
        remarks_entry: remarks_entry.clone(),
        timeout_entry: timeout_entry.clone(),
        group_entry: group_entry.clone(),
        proxy_port_entry: proxy_port_entry.clone(),
        close_to_tray_check: close_to_tray_check.clone(),
        juicity_box: juicity_box.clone(),
        ss_box: ss_box.clone(),
        status_label: status_label.clone(),
    };

    // ── Helper: common save → modify → repopulate pattern ────────────────
    fn modify_profile_and_reload(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        populate: &dyn Fn(),
        load: &dyn Fn(),
        modify: impl FnOnce(&mut GuiState),
    ) {
        save();
        modify(&mut state.borrow_mut());
        populate();
        load();
    }

    // ── Button callback helpers (named functions) ─────────────────────────
    fn on_add_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        populate: &dyn Fn(),
        load: &dyn Fn(),
    ) {
        let n = state.borrow().profiles.profiles.len() + 1;
        let mut p = ProxyProfile::default();
        p.name = t!("misc.new_server", n = n).to_string();
        modify_profile_and_reload(state, save, populate, load, |s| {
            s.profiles.profiles.push(p);
            s.runtime.selected_profile = s.profiles.profiles.len() - 1;
        });
    }

    fn on_del_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        populate: &dyn Fn(),
        load: &dyn Fn(),
    ) {
        modify_profile_and_reload(state, save, populate, load, |s| {
            let idx = s.runtime.selected_profile;
            if s.profiles.profiles.len() > 1 {
                s.profiles.profiles.remove(idx);
                if idx >= s.profiles.profiles.len() {
                    s.runtime.selected_profile = s.profiles.profiles.len() - 1;
                }
            }
            s.normalize_selected_index();
        });
    }

    fn on_dup_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        populate: &dyn Fn(),
        load: &dyn Fn(),
    ) {
        modify_profile_and_reload(state, save, populate, load, |s| {
            let idx = s.runtime.selected_profile;
            if let Some(p) = s.profiles.profiles.get(idx).cloned() {
                s.profiles.profiles.insert(idx + 1, p);
                s.runtime.selected_profile = idx + 1;
            }
        });
    }

    fn on_up_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        populate: &dyn Fn(),
        load: &dyn Fn(),
    ) {
        modify_profile_and_reload(state, save, populate, load, |s| {
            let idx = s.runtime.selected_profile;
            if idx > 0 {
                s.profiles.profiles.swap(idx, idx - 1);
                s.runtime.selected_profile = idx - 1;
            }
        });
    }

    fn on_dn_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        populate: &dyn Fn(),
        load: &dyn Fn(),
    ) {
        modify_profile_and_reload(state, save, populate, load, |s| {
            let idx = s.runtime.selected_profile;
            if idx + 1 < s.profiles.profiles.len() {
                s.profiles.profiles.swap(idx, idx + 1);
                s.runtime.selected_profile = idx + 1;
            }
        });
    }

    fn on_start_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        refresh: &dyn Fn(),
        status: &gtk::Label,
        tray: &Arc<Mutex<TraySharedState>>,
    ) {
        save();
        refresh();
        let mut s = state.borrow_mut();
        if let Err(err) = s.flush() {
            status.set_text(&t!("status.save_failed", err = err.to_string()));
            return;
        }
        let profile = match s.selected_profile().cloned() {
            Some(p) => p,
            None => { status.set_text(&*t!("status.no_server")); return; }
        };
        let config_snap = s.config.clone();
        match s.core_manager.start_profile(&config_snap, &profile) {
            Ok(()) => {
                status.set_text(&t!(
                    "status.running",
                    proto = profile.protocol.label(),
                    name = profile.display_name()
                ));
                s.runtime.was_running = true;
                let _ = s.flush();
                if let Ok(mut ts) = tray.lock() {
                    ts.is_running = true;
                    ts.active_server_name = profile.display_name();
                }
            }
            Err(err) => status.set_text(&t!("status.start_failed", err = err.to_string())),
        }
    }

    fn on_stop_clicked(
        state: &Rc<RefCell<GuiState>>,
        status: &gtk::Label,
        tray: &Arc<Mutex<TraySharedState>>,
    ) {
        let mut s = state.borrow_mut();
        match s.core_manager.stop() {
            Ok(()) => {
                status.set_text(&*t!("status.stopped"));
                s.runtime.was_running = false;
                let _ = s.flush();
                if let Ok(mut ts) = tray.lock() {
                    ts.is_running = false;
                    ts.active_server_name = String::new();
                }
            }
            Err(err) => status.set_text(&t!("status.stop_failed", err = err.to_string())),
        }
    }

    fn on_import_clicked<S, P, L, R>(
        state: &Rc<RefCell<GuiState>>,
        save: S,
        populate: P,
        load: L,
        refresh: R,
        status_label: gtk::Label,
    ) where
        S: Fn() + Clone + 'static,
        P: Fn() + Clone + 'static,
        L: Fn() + Clone + 'static,
        R: Fn() + Clone + 'static,
    {
        if let Some(display) = gtk::gdk::Display::default() {
            let state2 = state.clone();
            let pl2 = populate.clone();
            let lf2 = load.clone();
            let rsl2 = refresh.clone();
            let sl2 = status_label.clone();
            let sf2 = save.clone();
            display.clipboard().read_text_async(
                None::<&gtk::gio::Cancellable>,
                move |res| {
                    let text = match res { Ok(Some(t)) => t.to_string(), _ => return };
                    let trimmed = text.trim();
                    match link::import_share_link(trimmed) {
                        Ok(imported) => {
                            sf2();
                            let mut s = state2.borrow_mut();
                            let idx = s.runtime.selected_profile;
                            if let Some(p) = s.profiles.profiles.get_mut(idx) {
                                imported.apply_to(p);
                            }
                            drop(s);
                            pl2();
                            lf2();
                            rsl2();
                            sl2.set_text(&*t!("status.imported"));
                        }
                        Err(err) => sl2.set_text(&t!("status.import_failed", err = err.to_string())),
                    }
                },
            );
        }
    }

    fn on_export_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        status: &gtk::Label,
    ) {
        save();
        let s = state.borrow();
        let Some(profile) = s.selected_profile() else {
            status.set_text(&*t!("status.no_server_selected"));
            return;
        };
        match link::export_share_link(profile) {
            Ok(url) => {
                if let Some(disp) = gtk::gdk::Display::default() {
                    disp.clipboard().set_text(&url);
                    status.set_text(&*t!("status.url_copied"));
                }
            }
            Err(err) => status.set_text(&t!("status.export_failed", err = err.to_string())),
        }
    }

    fn on_ok_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        refresh: &dyn Fn(),
        status: &gtk::Label,
        window: &gtk::ApplicationWindow,
    ) {
        save();
        refresh();
        match state.borrow().flush() {
            Ok(()) => { window.set_visible(false); }
            Err(err) => status.set_text(&t!("status.save_failed", err = err.to_string())),
        }
    }

    fn on_cancel_clicked(
        load: &dyn Fn(),
        window: &gtk::ApplicationWindow,
    ) {
        load();
        window.set_visible(false);
    }

    fn on_apply_clicked(
        state: &Rc<RefCell<GuiState>>,
        save: &dyn Fn(),
        refresh: &dyn Fn(),
        status: &gtk::Label,
    ) {
        save();
        refresh();
        let s = state.borrow();
        match s.flush() {
            Ok(()) => {
                let _ = system_proxy::apply_system_proxy(&s.config);
                status.set_text(&*t!("status.saved"));
            }
            Err(err) => status.set_text(&t!("status.save_failed", err = err.to_string())),
        }
    }

    // ── populate_list: rebuild ListBox from state ─────────────────────────
    let populate_list = {
        let state = state.clone();
        let servers_listbox = servers_listbox.clone();
        let suspend_list = suspend_list.clone();
        move || {
            suspend_list.set(true);
            while let Some(child) = servers_listbox.first_child() {
                servers_listbox.remove(&child);
            }
            let (labels, sel) = {
                let mut s = state.borrow_mut();
                s.normalize_selected_index();
                let labels: Vec<String> = s.profiles.profiles.iter()
                    .map(|p| p.display_name()).collect();
                (labels, s.runtime.selected_profile)
            };
            for label in &labels {
                let row = gtk::ListBoxRow::new();
                let lbl = gtk::Label::builder()
                    .label(label.as_str())
                    .xalign(0.0)
                    .margin_start(8).margin_end(4)
                    .margin_top(3).margin_bottom(3)
                    .build();
                row.set_child(Some(&lbl));
                servers_listbox.append(&row);
            }
            if let Some(row) = servers_listbox.row_at_index(sel as i32) {
                servers_listbox.select_row(Some(&row));
            }
            suspend_list.set(false);
        }
    };

    // ── refresh_selected_label: update just the current row's text ─────────
    let refresh_selected_label = {
        let state = state.clone();
        let servers_listbox = servers_listbox.clone();
        move || {
            let s = state.borrow();
            let idx = s.runtime.selected_profile;
            if let Some(row) = servers_listbox.row_at_index(idx as i32) {
                if let Some(lbl) = row.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                    let name = s.profiles.profiles.get(idx)
                        .map(|p| p.display_name()).unwrap_or_default();
                    lbl.set_label(&name);
                }
            }
        }
    };

    // ── load_fields: set all widget values from selected profile ───────────
    fn load_fields_impl(state: &Rc<RefCell<GuiState>>, w: &FieldWidgets) {
        let s = state.borrow();
        if let Some(p) = s.selected_profile() {
            w.protocol_dropdown.set_selected(p.protocol.index());
            w.server_entry.set_text(&p.server);
            w.port_entry.set_text(&p.server_port.to_string());
            w.password_entry.set_text(&p.password);
            w.uuid_entry.set_text(&p.uuid);
            w.sni_entry.set_text(p.sni.as_deref().unwrap_or(""));
            w.allow_insecure_check.set_active(p.allow_insecure);
            w.method_dropdown.set_selected(method_to_index(&p.method));
            w.plugin_entry.set_text(p.plugin.as_deref().unwrap_or(""));
            w.plugin_opts_entry.set_text(p.plugin_opts.as_deref().unwrap_or(""));
            let has_args = p.plugin_args.is_some();
            w.need_plugin_arg.set_active(has_args);
            w.plugin_args_row.set_visible(has_args);
            w.plugin_args_entry.set_text(p.plugin_args.as_deref().unwrap_or(""));
            w.remarks_entry.set_text(&p.name);
            w.timeout_entry.set_text(&p.timeout.to_string());
            w.group_entry.set_text(p.group.as_deref().unwrap_or(""));
            let is_juicity = p.protocol == ProxyProtocol::Juicity;
            w.juicity_box.set_visible(is_juicity);
            w.ss_box.set_visible(!is_juicity);
        }
        w.proxy_port_entry.set_text(&extract_port(&s.config.socks_listen).to_string());
        w.close_to_tray_check.set_active(s.runtime.close_to_tray);
    }

    let load_fields = {
        let state = state.clone();
        let w = field_widgets.clone();
        move || load_fields_impl(&state, &w)
    };

    // ── save_fields: read widgets into selected profile ────────────────────
    fn save_fields_impl(state: &Rc<RefCell<GuiState>>, w: &FieldWidgets) {
        let mut s = state.borrow_mut();
        s.normalize_selected_index();
        if let Some(p) = s.selected_profile_mut() {
            p.protocol = ProxyProtocol::from_index(w.protocol_dropdown.selected());
            p.server = w.server_entry.text().trim().to_string();
            p.server_port = match w.port_entry.text().parse() {
                Ok(v) => v,
                Err(_) => {
                    w.status_label.set_text("Status: Invalid server port");
                    443
                }
            };
            p.password = w.password_entry.text().to_string();
            p.uuid = w.uuid_entry.text().trim().to_string();
            p.sni = non_empty_text(w.sni_entry.text().as_str());
            p.allow_insecure = w.allow_insecure_check.is_active();
            let midx = w.method_dropdown.selected() as usize;
            p.method = SS_METHODS.get(midx).copied()
                .unwrap_or("chacha20-ietf-poly1305").to_string();
            p.plugin = non_empty_text(w.plugin_entry.text().as_str());
            p.plugin_opts = non_empty_text(w.plugin_opts_entry.text().as_str());
            p.plugin_args = if w.need_plugin_arg.is_active() {
                non_empty_text(w.plugin_args_entry.text().as_str())
            } else {
                None
            };
            let remarks = w.remarks_entry.text().trim().to_string();
            p.name = if remarks.is_empty() { "New Server".to_string() } else { remarks };
            p.timeout = match w.timeout_entry.text().parse() {
                Ok(v) => v,
                Err(_) => {
                    w.status_label.set_text("Status: Invalid timeout value");
                    5
                }
            };
            p.group = non_empty_text(w.group_entry.text().as_str());
        }
        let port: u16 = match w.proxy_port_entry.text().parse() {
            Ok(v) => v,
            Err(_) => {
                w.status_label.set_text("Status: Invalid proxy port");
                1080
            }
        };
        let (addr, _) = crate::util::split_host_port(&s.config.socks_listen);
        s.config.socks_listen = crate::util::format_host_port(&addr, port);
        s.runtime.close_to_tray = w.close_to_tray_check.is_active();
    }

    let save_fields = {
        let state = state.clone();
        let w = field_widgets.clone();
        move || save_fields_impl(&state, &w)
    };

    // ── List row-selected ─────────────────────────────────────────────────
    {
        let state = state.clone();
        let load_fields = load_fields.clone();
        let suspend_list = suspend_list.clone();
        servers_listbox.connect_row_selected(move |_, row| {
            if suspend_list.get() { return; }
            if let Some(row) = row {
                state.borrow_mut().runtime.selected_profile = row.index() as usize;
                load_fields();
            }
        });
    }

    // ── Protocol dropdown → toggle Juicity/SS section visibility ──────────
    {
        let juicity_box = juicity_box.clone();
        let ss_box = ss_box.clone();
        protocol_dropdown.connect_selected_notify(move |dd| {
            let is_juicity = dd.selected() == 0;
            juicity_box.set_visible(is_juicity);
            ss_box.set_visible(!is_juicity);
        });
    }

    // ── Show Password toggle ──────────────────────────────────────────────
    {
        let password_entry = password_entry.clone();
        show_pwd_btn.connect_toggled(move |btn| {
            password_entry.set_visibility(btn.is_active());
        });
    }

    // ── Need Plugin Argument toggle ───────────────────────────────────────
    {
        let plugin_args_row = plugin_args_row.clone();
        need_plugin_arg.connect_toggled(move |btn| {
            plugin_args_row.set_visible(btn.is_active());
        });
    }

    // ── Add button ────────────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let state = state.clone();
        add_btn.connect_clicked(move |_| {
            on_add_clicked(&state, &save_fields, &populate_list, &load_fields);
        });
    }

    // ── Delete button ─────────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let state = state.clone();
        del_btn.connect_clicked(move |_| {
            on_del_clicked(&state, &save_fields, &populate_list, &load_fields);
        });
    }

    // ── Duplicate button ──────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let state = state.clone();
        dup_btn.connect_clicked(move |_| {
            on_dup_clicked(&state, &save_fields, &populate_list, &load_fields);
        });
    }

    // ── Move Up button ────────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let state = state.clone();
        up_btn.connect_clicked(move |_| {
            on_up_clicked(&state, &save_fields, &populate_list, &load_fields);
        });
    }

    // ── Move Down button ──────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let state = state.clone();
        dn_btn.connect_clicked(move |_| {
            on_dn_clicked(&state, &save_fields, &populate_list, &load_fields);
        });
    }

    // ── Start button ──────────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let refresh_selected_label = refresh_selected_label.clone();
        let status_label = status_label.clone();
        let state = state.clone();
        let tray_shared = Arc::clone(&tray_shared);
        start_btn.connect_clicked(move |_| {
            on_start_clicked(&state, &save_fields, &refresh_selected_label, &status_label, &tray_shared);
        });
    }

    // ── Stop button ───────────────────────────────────────────────────────
    {
        let status_label = status_label.clone();
        let state = state.clone();
        let tray_shared = Arc::clone(&tray_shared);
        stop_btn.connect_clicked(move |_| {
            on_stop_clicked(&state, &status_label, &tray_shared);
        });
    }

    // ── Import URL from clipboard ──────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let refresh_selected_label = refresh_selected_label.clone();
        let status_label = status_label.clone();
        let state = state.clone();
        import_btn.connect_clicked(move |_| {
            on_import_clicked(&state, save_fields.clone(), populate_list.clone(), load_fields.clone(), refresh_selected_label.clone(), status_label.clone());
        });
    }

    // ── Export URL to clipboard ────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let status_label = status_label.clone();
        let state = state.clone();
        export_btn.connect_clicked(move |_| {
            on_export_clicked(&state, &save_fields, &status_label);
        });
    }

    // ── OK button ─────────────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let refresh_selected_label = refresh_selected_label.clone();
        let status_label = status_label.clone();
        let state = state.clone();
        let window = window.clone();
        ok_btn.connect_clicked(move |_| {
            on_ok_clicked(&state, &save_fields, &refresh_selected_label, &status_label, &window);
        });
    }

    // ── Cancel button ─────────────────────────────────────────────────────
    {
        let load_fields = load_fields.clone();
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| {
            on_cancel_clicked(&load_fields, &window);
        });
    }

    // ── Apply button ──────────────────────────────────────────────────────
    {
        let save_fields = save_fields.clone();
        let refresh_selected_label = refresh_selected_label.clone();
        let status_label = status_label.clone();
        let state = state.clone();
        apply_btn.connect_clicked(move |_| {
            on_apply_clicked(&state, &save_fields, &refresh_selected_label, &status_label);
        });
    }

    fn on_pac_settings_clicked(
        state: &Rc<RefCell<GuiState>>,
        status_label: &gtk::Label,
        window: &gtk::ApplicationWindow,
    ) {
        let cfg_snapshot = state.borrow().config.clone();

        // on_save: update config, persist, restart PAC server if listen addr changed.
        let state_save = state.clone();
        let sl_save = status_label.clone();
        let on_save = move |new_cfg: crate::config::AppConfig| {
            let mut s = state_save.borrow_mut();
            let pac_listen_changed = s.config.pac_listen != new_cfg.pac_listen;
            s.config = new_cfg;
            if let Err(err) = s.flush() {
                sl_save.set_text(&t!("status.save_failed", err = err.to_string()));
                return;
            }
            if pac_listen_changed || s.pac_server.is_none() {
                if let Err(e) = restart_pac_server(&mut s, pac_listen_changed) {
                    tracing::warn!("PAC server restart failed: {e}");
                }
            }
            sl_save.set_text(&*t!("status.saved"));
        };

        // on_update_now: spawn background download thread.
        let state_upd = state.clone();
        let sl_upd = status_label.clone();
        let on_update_now = move || {
            let (data_dir, direct_url, proxy_url, tx) = {
                let mut s = state_upd.borrow_mut();
                if s.pac_update_rx.is_some() {
                    return;
                }
                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                s.pac_update_rx = Some(rx);
                (
                    s.storage.paths().config_dir.clone(),
                    s.config.pac_direct_url.clone(),
                    s.config.pac_proxy_url.clone(),
                    tx,
                )
            };
            std::thread::spawn(move || {
                let _ = tx.send(
                    pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()),
                );
            });
            sl_upd.set_text(&*t!("status.pac_downloading"));
        };

        crate::pac_dialog::open(window, cfg_snapshot, on_save, on_update_now);
    }

    // ── PAC Settings button ───────────────────────────────────────────────
    {
        let state = state.clone();
        let status_label = status_label.clone();
        let window = window.clone();
        pac_settings_btn.connect_clicked(move |_| {
            on_pac_settings_clicked(&state, &status_label, &window);
        });
    }

    // ── Auto-update PAC rules on startup if interval is set and overdue ────
    {
        let mut s = state.borrow_mut();
        if s.config.pac_auto_update_hours > 0 {
            let age_h = pac::rules_age_hours(&s.storage.paths().config_dir);
            let overdue = age_h.map_or(true, |h| h >= s.config.pac_auto_update_hours as u64);
            if overdue {
                let data_dir = s.storage.paths().config_dir.clone();
                let direct_url = s.config.pac_direct_url.clone();
                let proxy_url = s.config.pac_proxy_url.clone();
                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                std::thread::spawn(move || {
                    let _ = tx.send(
                        pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()),
                    );
                });
                s.pac_update_rx = Some(rx);
            }
        }
    }

    // ── Tray ──────────────────────────────────────────────────────────────
    let (tray_tx, tray_rx) = std::sync::mpsc::channel::<TrayEvent>();
    {
        let mut s = state.borrow_mut();
        {
            let mut ts = tray_shared.lock().unwrap_or_else(|e| e.into_inner());
            ts.system_proxy_mode = s.config.system_proxy_mode;
            ts.pac_rule_mode = s.config.pac_rule_mode;
            ts.server_names = s.profiles.profiles.iter().map(|p| p.display_name()).collect();
            ts.active_server_idx = s.runtime.selected_profile;
        }
        s._tray_service = Some(tray::start(tray_tx, Arc::clone(&tray_shared)));
    }

    // ── Window close → optionally minimize to tray ─────────────────────────
    {
        let state = state.clone();
        window.connect_close_request(move |w| {
            if state.borrow().runtime.close_to_tray {
                w.set_visible(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    // ── Tray event + core status poll (every 300 ms) ─────────────────────
    {
        let state = state.clone();
        let status_label = status_label.clone();
        let window = window.clone();
        let app = app.clone();
        let populate_list = populate_list.clone();
        let load_fields = load_fields.clone();
        let tray_shared = Arc::clone(&tray_shared);
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            loop {
                match tray_rx.try_recv() {
                    Ok(TrayEvent::ShowEditServers) => window.present(),
                    Ok(TrayEvent::ShowPacSettings) => {
                        let cfg_snapshot = state.borrow().config.clone();
                        let state_s = state.clone();
                        let sl_s = status_label.clone();
                        let on_save = move |new_cfg: crate::config::AppConfig| {
                            let mut s = state_s.borrow_mut();
                            let pac_listen_changed = s.config.pac_listen != new_cfg.pac_listen;
                            s.config = new_cfg;
                            if let Err(err) = s.flush() {
                                sl_s.set_text(&t!("status.save_failed", err = err.to_string()));
                                return;
                            }
                            if pac_listen_changed || s.pac_server.is_none() {
                                if let Err(e) = restart_pac_server(&mut s, pac_listen_changed) {
                                    tracing::warn!("PAC server restart failed: {e}");
                                }
                            }
                            sl_s.set_text(&*t!("status.saved"));
                        };
                        let state_u = state.clone();
                        let sl_u = status_label.clone();
                        let on_update_now = move || {
                            let (data_dir, direct_url, proxy_url, tx) = {
                                let mut s = state_u.borrow_mut();
                                if s.pac_update_rx.is_some() { return; }
                                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                                s.pac_update_rx = Some(rx);
                                (
                                    s.storage.paths().config_dir.clone(),
                                    s.config.pac_direct_url.clone(),
                                    s.config.pac_proxy_url.clone(),
                                    tx,
                                )
                            };
                            std::thread::spawn(move || {
                                let _ = tx.send(
                                    pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()),
                                );
                            });
                            sl_u.set_text(&*t!("status.pac_downloading"));
                        };
                        crate::pac_dialog::open(&window, cfg_snapshot, on_save, on_update_now);
                    }
                    Ok(TrayEvent::ShowStartupSettings) => {
                        let runtime_snapshot = state.borrow().runtime.clone();
                        let state_s = state.clone();
                        let sl_s = status_label.clone();
                        let on_save = move |new_state: RuntimeState| {
                            let mut s = state_s.borrow_mut();
                            s.runtime = new_state;
                            if let Err(err) = s.flush() {
                                sl_s.set_text(&t!("status.save_failed", err = err.to_string()));
                                return;
                            }
                            // Apply auto-start setting.
                            if let Err(err) = apply_autostart(&s.runtime) {
                                tracing::warn!("failed to apply auto-start setting: {err}");
                            }
                            sl_s.set_text(&*t!("status.saved"));
                        };
                        crate::startup_dialog::open(&window, runtime_snapshot, on_save);
                    }
                    Ok(TrayEvent::SetSystemProxy(mode)) => {
                        let mut s = state.borrow_mut();
                        s.config.system_proxy_mode = mode;
                        let _ = s.flush();
                        let snap = s.config.clone();
                        drop(s);
                        let _ = system_proxy::apply_system_proxy(&snap);
                        if let Ok(mut ts) = tray_shared.lock() {
                            ts.system_proxy_mode = mode;
                        }
                        status_label.set_text(&t!("status.system_proxy", mode = mode.label()));
                    }
                    Ok(TrayEvent::SetPacRuleMode(pac_mode)) => {
                        let mut s = state.borrow_mut();
                        s.config.pac_rule_mode = pac_mode;
                        let _ = s.flush();
                        // Regenerate PAC with the new rule set.
                        let _ = restart_pac_server(&mut s, false);
                        if let Ok(mut ts) = tray_shared.lock() {
                            ts.pac_rule_mode = pac_mode;
                        }
                        status_label.set_text(&t!("status.pac_rule", mode = pac_mode.label()));
                    }
                    Ok(TrayEvent::UpdatePacRules) => {
                        let (data_dir, direct_url, proxy_url, tx) = {
                            let mut s = state.borrow_mut();
                            if s.pac_update_rx.is_some() {
                                return glib::ControlFlow::Continue;
                            }
                            let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                            s.pac_update_rx = Some(rx);
                            (
                                s.storage.paths().config_dir.clone(),
                                s.config.pac_direct_url.clone(),
                                s.config.pac_proxy_url.clone(),
                                tx,
                            )
                        };
                        std::thread::spawn(move || {
                            let result = pac::download_rules(&data_dir, &direct_url, &proxy_url)
                                .map(|_| ());
                            let _ = tx.send(result);
                        });
                        status_label.set_text(&*t!("status.pac_downloading"));
                    }
                    Ok(TrayEvent::SelectServer(idx)) => {
                        {
                            let mut s = state.borrow_mut();
                            s.runtime.selected_profile = idx;
                        }
                        populate_list();
                        load_fields();
                    }
                    Ok(TrayEvent::ImportFromClipboard) => window.present(),
                    Ok(TrayEvent::ToggleProxy) => {
                        let mut s = state.borrow_mut();
                        if s.core_manager.is_running() {
                            match s.core_manager.stop() {
                                Ok(()) => {
                                    status_label.set_text(&*t!("status.stopped"));
                                    s.runtime.was_running = false;
                                    let _ = s.flush();
                                    if let Ok(mut ts) = tray_shared.lock() {
                                        ts.is_running = false;
                                        ts.active_server_name = String::new();
                                    }
                                }
                                Err(err) => status_label.set_text(
                                    &t!("status.stop_failed", err = err.to_string())
                                ),
                            }
                        } else {
                            let profile = match s.selected_profile().cloned() {
                                Some(p) => p,
                                None => { status_label.set_text(&*t!("status.no_server")); return glib::ControlFlow::Continue; }
                            };
                            let config_snap = s.config.clone();
                            match s.core_manager.start_profile(&config_snap, &profile) {
                                Ok(()) => {
                                    status_label.set_text(&t!(
                                        "status.running",
                                        proto = profile.protocol.label(),
                                        name = profile.display_name()
                                    ));
                                    s.runtime.was_running = true;
                                    let _ = s.flush();
                                    if let Ok(mut ts) = tray_shared.lock() {
                                        ts.is_running = true;
                                        ts.active_server_name = profile.display_name();
                                    }
                                }
                                Err(err) => status_label.set_text(
                                    &t!("status.start_failed", err = err.to_string())
                                ),
                            }
                        }
                    }
                    Ok(TrayEvent::QuitApp) => {
                        let _ = state.borrow_mut().core_manager.stop();
                        app.quit();
                        return glib::ControlFlow::Break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }

            // Poll PAC download completion.
            let done = {
                let s = state.borrow();
                if let Some(rx) = &s.pac_update_rx {
                    match rx.try_recv() {
                        Ok(Ok(())) => Some(true),
                        Ok(Err(e)) => {
                            status_label.set_text(&t!("status.pac_download_failed", err = e.to_string()));
                            Some(false)
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(false),
                        Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    }
                } else {
                    None
                }
            };
            if let Some(success) = done {
                let mut s = state.borrow_mut();
                s.pac_update_rx = None;
                if success {
                    let _ = restart_pac_server(&mut s, false);
                    status_label.set_text(&*t!("status.pac_updated"));
                }
            }

            let mut s = state.borrow_mut();
            match s.core_manager.poll() {
                Ok(Some(exit)) => {
                    status_label.set_text(&t!("status.core_exited", code = exit.to_string()));
                    if let Ok(mut ts) = tray_shared.lock() {
                        ts.is_running = false;
                        ts.active_server_name = String::new();
                    }
                }
                Ok(None) if s.core_manager.is_running() => {
                    let proto = s.core_manager.current_protocol()
                        .unwrap_or(ProxyProtocol::Juicity).label();
                    let name = s.selected_profile()
                        .map(|p| p.display_name())
                        .unwrap_or_default();
                    status_label.set_text(&t!("status.running", proto = proto, name = name));
                    if let Ok(mut ts) = tray_shared.lock() {
                        ts.is_running = true;
                        ts.active_server_name = name;
                    }
                }
                Err(err) => status_label.set_text(&t!("status.poll_error", err = err.to_string())),
                _ => {}
            }

            glib::ControlFlow::Continue
        });
    }

    populate_list();
    load_fields();

    // ── Apply startup settings ──────────────────────────────────────────
    let start_connection = {
        let s = state.borrow();
        let should_start = match s.runtime.startup_connection_state {
            StartupConnectionState::On => true,
            StartupConnectionState::LastState => s.runtime.was_running,
            StartupConnectionState::Off => false,
        };
        // Clone values needed for auto-start outside the borrow.
        let config = s.config.clone();
        let profile = s.selected_profile().cloned();
        let profile_name = profile.as_ref().map(|p| p.display_name()).unwrap_or_default();
        let profile_protocol = profile.as_ref().map(|p| p.protocol).unwrap_or(ProxyProtocol::Juicity);
        (should_start, config, profile, profile_name, profile_protocol)
    };

    // Hide window if configured.
    if state.borrow().runtime.hide_window_on_startup {
        window.set_visible(false);
    } else {
        window.present();
    }

    // Auto-start proxy if configured.
    if start_connection.0 {
        if let Some(profile) = start_connection.2 {
            let mut s = state.borrow_mut();
            let config = start_connection.1;
            match s.core_manager.start_profile(&config, &profile) {
                Ok(()) => {
                    status_label.set_text(&t!(
                        "status.running",
                        proto = start_connection.4.label(),
                        name = start_connection.3
                    ));
                    s.runtime.was_running = true;
                    let _ = s.flush();
                    if let Ok(mut ts) = tray_shared.lock() {
                        ts.is_running = true;
                        ts.active_server_name = start_connection.3;
                    }
                }
                Err(err) => {
                    status_label.set_text(&t!("status.start_failed", err = err.to_string()));
                }
            }
        }
    }

    Ok(())
}

/// Apply or remove system auto-start for the application.
///
/// On Linux, this creates or removes a `.desktop` file in `~/.config/autostart/`.
/// On Windows, this would modify the registry Run key.
/// On macOS, this would use LSSharedFileList.
#[allow(unused_variables)]
fn apply_autostart(state: &RuntimeState) -> anyhow::Result<()> {
    fn autostart_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(&home).join(".config/autostart")
    }

    if !state.auto_start {
        // Remove existing autostart entry.
        #[cfg(target_os = "linux")]
        {
            let desktop_file = autostart_dir().join("io.juicity.gui.desktop");
            if desktop_file.exists() {
                let _ = std::fs::remove_file(&desktop_file);
            }
        }
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let dir = autostart_dir();
        std::fs::create_dir_all(&dir)?;

        // Determine the executable path.  Fall back to argv[0] when /proc/self/exe
        // is unavailable (e.g. some containers).
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("juicity-gui"));

        let desktop_content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Juicity GUI\n\
             Comment=Juicity GUI Client\n\
             Exec={}\n\
             Icon=io.juicity.gui\n\
             Terminal=false\n\
             Categories=Network;\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display()
        );

        let desktop_file = dir.join("io.juicity.gui.desktop");
        std::fs::write(&desktop_file, desktop_content.as_bytes())?;
        tracing::info!("autostart desktop file created at {}", desktop_file.display());
    }

    // TODO: Windows registry support via winreg crate.
    // TODO: macOS launchd plist support.

    Ok(())
}

/// Horizontal field row: right-aligned 14-char label + widget filling the rest.
fn make_field_row(label_text: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    let lbl = gtk::Label::builder()
        .label(label_text)
        .xalign(1.0)
        .width_chars(14)
        .build();
    row.append(&lbl);
    row.append(widget);
    row
}

fn extract_port(addr: &str) -> u16 {
    addr.rsplitn(2, ':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1080)
}

fn non_empty_text(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}
