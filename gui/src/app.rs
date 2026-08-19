use crate::config::{
    method_to_index, ProxyProfile, ProxyProtocol, RuntimeState, StartupConnectionState, SS_METHODS,
};
use crate::link;
use crate::pac;
use crate::state::{extract_port, non_empty_text, restart_pac_server, GuiState};
use crate::system_proxy;
use crate::tray::{TrayEvent, TraySharedState};
use crate::widgets;
use gpui::prelude::*;
use gpui::{
    actions, div, px, rgb, size, App, Bounds, ClickEvent, Context, ElementId, Entity, FontWeight,
    Global, KeyBinding, SharedString, Timer, WeakEntity, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::IndexPath;
use rust_i18n::t;
use std::sync::{Arc, Mutex};
use std::time::Duration;

actions!(app, [Quit]);

/// App-wide registry that survives the main window being closed to the tray.
#[derive(Default)]
struct AppRoot {
    view: Option<Entity<AppView>>,
    main_window: Option<WindowHandle<gpui_component::Root>>,
    /// Set right before the main window is closed via OK/Cancel so the
    /// `on_window_closed` handler keeps the app alive in the tray.
    suppress_quit: bool,
    /// Whether the main window has already been closed (to the tray).  Once
    /// set, closing *dialog* windows never quits the application; only the
    /// first close of the main window is able to trigger the quit path.
    main_window_closed: bool,
}

impl Global for AppRoot {}

/// Open (or focus) the main window bound to the persistent `AppView` entity.
fn open_main_window(cx: &mut App) {
    let Some(view) = cx.default_global::<AppRoot>().view.clone() else {
        return;
    };
    let existing = cx.default_global::<AppRoot>().main_window;
    let already_active = existing
        .as_ref()
        .and_then(|h| h.update(cx, |_, window, _| window.activate_window()).ok())
        .is_some();
    if already_active {
        return;
    }
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(760.), px(600.)),
                    cx,
                ))),
                app_id: Some("io.juicity.gui".to_string()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(&t!("window.title"));
                window.set_app_id("io.juicity.gui");

                // Detect main-window close *before* gpui removes it from the
                // window map, so that `on_window_closed` can distinguish the
                // main window from dialog windows.
                window.on_window_should_close(cx, |_window, cx| {
                    // Read close_to_tray before mutating the global.
                    let view_entity = cx
                        .default_global::<AppRoot>()
                        .view
                        .clone();
                    let close_to_tray = view_entity
                        .as_ref()
                        .map(|v| v.read(cx).gui.runtime.close_to_tray)
                        .unwrap_or(false);
                    {
                        let g = cx.default_global::<AppRoot>();
                        g.main_window_closed = true;
                        g.main_window = None;
                        let suppress = g.suppress_quit;
                        g.suppress_quit = false;
                        if !suppress && !close_to_tray {
                            cx.quit();
                        }
                    }
                    true
                });

                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            },
        )
        .ok();
    if let Some(handle) = handle {
        let g = cx.default_global::<AppRoot>();
        g.main_window = Some(handle);
        g.main_window_closed = false;
    }
}

pub struct AppView {
    gui: GuiState,
    tray_tx: std::sync::mpsc::Sender<TrayEvent>,
    tray_rx: std::sync::mpsc::Receiver<TrayEvent>,
    tray_shared: Arc<Mutex<TraySharedState>>,

    // ── Editor text fields (gpui-component InputState; built lazily on first render) ──
    server: Option<Entity<InputState>>,
    port: Option<Entity<InputState>>,
    password: Option<Entity<InputState>>,
    uuid: Option<Entity<InputState>>,
    sni: Option<Entity<InputState>>,
    plugin: Option<Entity<InputState>>,
    plugin_opts: Option<Entity<InputState>>,
    plugin_args: Option<Entity<InputState>>,
    remarks: Option<Entity<InputState>>,
    timeout: Option<Entity<InputState>>,
    group: Option<Entity<InputState>>,
    proxy_port: Option<Entity<InputState>>,

    // ── Editor select fields ──
    protocol_select: Option<Entity<SelectState<Vec<SharedString>>>>,
    method_select: Option<Entity<SelectState<Vec<SharedString>>>>,
    protocol_options: Vec<SharedString>,
    method_options: Vec<SharedString>,

    // ── Editor widget state ──
    protocol: usize,
    method: usize,
    show_password: bool,
    allow_insecure: bool,
    need_plugin_arg: bool,
    close_to_tray: bool,
    inputs_inited: bool,
    pending_reload: bool,
    /// Set when the protocol dropdown changes; the render method will
    /// call `load_fields` (which requires a `&mut Window`).
    protocol_changed: bool,

    status: String,

    // ── Config hot-reload ───────────────────────────────────────────────
    #[allow(dead_code)]
    config_watcher: Option<notify::RecommendedWatcher>,
    config_reload_rx: Option<std::sync::mpsc::Receiver<()>>,
    /// Timestamp of the last `flush()` call – used to ignore self-inflicted
    /// watcher events that would otherwise cause an infinite reload loop.
    last_flush_at: std::time::Instant,
}

impl AppView {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut gui = GuiState::new().expect("failed to initialize app state");
        if let Err(err) = restart_pac_server(&mut gui, true) {
            tracing::warn!("PAC server failed to start: {err}");
        }

        // Auto-update PAC rules on startup if interval is set and overdue.
        if gui.config.pac_auto_update_hours > 0 {
            let age_h = pac::rules_age_hours(&gui.storage.paths().config_dir);
            let overdue = age_h.is_none_or(|h| h >= gui.config.pac_auto_update_hours as u64);
            if overdue {
                let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
                std::thread::spawn({
                    let data_dir = gui.storage.paths().config_dir.clone();
                    let direct_url = gui.config.pac_direct_url.clone();
                    let proxy_url = gui.config.pac_proxy_url.clone();
                    move || {
                        let _ = tx.send(
                            pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()),
                        );
                    }
                });
                gui.pac_update_rx = Some(rx);
            }
        }

        // ── Shared tray state + service ─────────────────────────────────────
        let (tray_tx, tray_rx) = std::sync::mpsc::channel::<TrayEvent>();
        let tray_shared = Arc::new(Mutex::new(TraySharedState::default()));
        {
            let mut ts = tray_shared.lock().unwrap_or_else(|e| e.into_inner());
            ts.system_proxy_mode = gui.config.system_proxy_mode;
            ts.pac_rule_mode = gui.config.pac_rule_mode;
            ts.server_names = gui
                .profiles
                .profiles
                .iter()
                .map(|p| p.display_name())
                .collect();
            ts.active_server_idx = gui.runtime.selected_profile;
        }
        gui._tray_service = Some(crate::tray::start(tray_tx.clone(), Arc::clone(&tray_shared)));

        let protocol_options: Vec<SharedString> = vec![
            t!("protocol.juicity").to_string().into(),
            t!("protocol.shadowsocks").to_string().into(),
        ];
        let method_options: Vec<SharedString> =
            SS_METHODS.iter().map(|s| SharedString::from(*s)).collect();

        let mut view = Self {
            gui,
            tray_tx,
            tray_rx,
            tray_shared,
            server: None,
            port: None,
            password: None,
            uuid: None,
            sni: None,
            plugin: None,
            plugin_opts: None,
            plugin_args: None,
            remarks: None,
            timeout: None,
            group: None,
            proxy_port: None,
            protocol_select: None,
            method_select: None,
            protocol_options,
            method_options,
            protocol: 0,
            method: 0,
            show_password: false,
            allow_insecure: false,
            need_plugin_arg: false,
            close_to_tray: false,
            inputs_inited: false,
            pending_reload: false,
            protocol_changed: false,
            status: t!("status.stopped").to_string(),
            config_watcher: None,
            config_reload_rx: None,
            last_flush_at: std::time::Instant::now(),
        };

        // ── Config hot-reload watcher ──
        view.spawn_config_watcher(view.gui.storage.paths().config_dir.clone());

        // ── Periodic poll loop: tray events + PAC + core status ────────────
        cx.spawn(async move |this, cx| {
            let mut timer = Timer::after(Duration::from_millis(300));
            loop {
                timer.await;
                if this.update(cx, |view, cx| view.poll(cx)).is_err() {
                    break;
                }
                timer = Timer::after(Duration::from_millis(300));
            }
        })
        .detach();

        view
    }

    // ── Status helper ─────────────────────────────────────────────────────

    fn set_status(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.status != text {
            self.status = text.to_string();
            // After the main window is closed to the tray, skip notifying gpui
            // to avoid "window not found" errors from the 300ms poll loop.
            if !cx.default_global::<AppRoot>().main_window_closed {
                cx.notify();
            }
        }
    }

    // ── Lazy input construction (needs a Window, so built on first render) ──

    fn init_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mk = |window: &mut Window, cx: &mut Context<Self>, value: &str| -> Entity<InputState> {
            let state = cx.new(|cx| InputState::new(window, cx));
            state.update(cx, |s, scx| {
                s.set_value(value.to_string(), window, scx);
            });
            state
        };

        let server = mk(window, cx, "");
        let port = mk(window, cx, "");
        let password = mk(window, cx, "");
        let uuid = mk(window, cx, "");
        let sni = mk(window, cx, "");
        let plugin = mk(window, cx, "");
        let plugin_opts = mk(window, cx, "");
        let plugin_args = mk(window, cx, "");
        let remarks = mk(window, cx, "");
        let timeout = mk(window, cx, "");
        let group = mk(window, cx, "");
        let proxy_port = mk(window, cx, "");

        // Password starts masked.
        password.update(cx, |s, scx| s.set_masked(true, window, scx));

        let protocol_select = cx.new(|cx| {
            SelectState::new(
                self.protocol_options.clone(),
                Some(IndexPath::new(self.protocol)),
                window,
                cx,
            )
        });
        let method_select = cx.new(|cx| {
            SelectState::new(
                self.method_options.clone(),
                Some(IndexPath::new(self.method)),
                window,
                cx,
            )
        });

        let owner = cx.weak_entity();
        let _ = cx.subscribe(&protocol_select, {
            let owner = owner.clone();
            move |_view, _state, event, cx| {
                if let SelectEvent::Confirm(Some(value)) = event {
                    let _ = owner.update(cx, |view, vcx| {
                        let new_protocol = view
                            .protocol_options
                            .iter()
                            .position(|o| o == value)
                            .unwrap_or(0);
                        // Pass the new protocol index so save_fields writes the
                        // correct value even though SelectState may not have
                        // updated yet at Confirm-event time.
                        view.save_fields_with_protocol(vcx, Some(new_protocol));
                        let _ = view.gui.flush();
                        view.protocol = new_protocol;
                        view.protocol_changed = true;
                        vcx.notify();
                    });
                }
            }
        });
        let _ = cx.subscribe(&method_select, {
            let owner = owner.clone();
            move |_view, _state, event, cx| {
                if let SelectEvent::Confirm(Some(value)) = event {
                    let _ = owner.update(cx, |view, vcx| {
                        view.method = view
                            .method_options
                            .iter()
                            .position(|o| o == value)
                            .unwrap_or(0);
                        // Persist the method change immediately.
                        view.save_fields(vcx);
                        let _ = view.gui.flush();
                        vcx.notify();
                    });
                }
            }
        });

        self.server = Some(server);
        self.port = Some(port);
        self.password = Some(password);
        self.uuid = Some(uuid);
        self.sni = Some(sni);
        self.plugin = Some(plugin);
        self.plugin_opts = Some(plugin_opts);
        self.plugin_args = Some(plugin_args);
        self.remarks = Some(remarks);
        self.timeout = Some(timeout);
        self.group = Some(group);
        self.proxy_port = Some(proxy_port);
        self.protocol_select = Some(protocol_select);
        self.method_select = Some(method_select);

        self.load_fields(window, cx);
    }

    // ── Field load / save ─────────────────────────────────────────────────

    fn load_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.gui.selected_profile().cloned();
        if let Some(p) = profile {
            self.protocol = p.protocol.index() as usize;
            if let Some(f) = &self.server {
                f.update(cx, |s, scx| s.set_value(p.server, window, scx));
            }
            if let Some(f) = &self.port {
                f.update(cx, |s, scx| s.set_value(p.server_port.to_string(), window, scx));
            }
            if let Some(f) = &self.password {
                f.update(cx, |s, scx| s.set_value(p.password, window, scx));
            }
            if let Some(f) = &self.uuid {
                f.update(cx, |s, scx| s.set_value(p.uuid, window, scx));
            }
            if let Some(f) = &self.sni {
                f.update(cx, |s, scx| s.set_value(p.sni.unwrap_or_default(), window, scx));
            }
            self.allow_insecure = p.allow_insecure;
            self.method = method_to_index(&p.method) as usize;
            if let Some(f) = &self.plugin {
                f.update(cx, |s, scx| s.set_value(p.plugin.unwrap_or_default(), window, scx));
            }
            if let Some(f) = &self.plugin_opts {
                f.update(cx, |s, scx| {
                    s.set_value(p.plugin_opts.unwrap_or_default(), window, scx)
                });
            }
            self.need_plugin_arg = p.plugin_args.is_some();
            if let Some(f) = &self.plugin_args {
                f.update(cx, |s, scx| {
                    s.set_value(p.plugin_args.unwrap_or_default(), window, scx)
                });
            }
            if let Some(f) = &self.remarks {
                f.update(cx, |s, scx| s.set_value(p.name, window, scx));
            }
            if let Some(f) = &self.timeout {
                f.update(cx, |s, scx| s.set_value(p.timeout.to_string(), window, scx));
            }
            if let Some(f) = &self.group {
                f.update(cx, |s, scx| s.set_value(p.group.unwrap_or_default(), window, scx));
            }
        }
        let port = extract_port(&self.gui.config.socks_listen);
        if let Some(f) = &self.proxy_port {
            f.update(cx, |s, scx| s.set_value(port.to_string(), window, scx));
        }
        self.close_to_tray = self.gui.runtime.close_to_tray;

        if let Some(sel) = &self.protocol_select {
            sel.update(cx, |st, scx| {
                st.set_selected_index(Some(IndexPath::new(self.protocol)), window, scx)
            });
        }
        if let Some(sel) = &self.method_select {
            sel.update(cx, |st, scx| {
                st.set_selected_index(Some(IndexPath::new(self.method)), window, scx)
            });
        }
        cx.notify();
    }

    /// When called from the protocol subscription handler, `protocol_override`
    /// supplies the newly-selected protocol index so that `save_fields` writes
    /// the correct value to the profile even though the `SelectState` widget
    /// may not have updated its internal state yet at the time of the
    /// `Confirm` event.
    fn save_fields_with_protocol(&mut self, cx: &mut Context<Self>, protocol_override: Option<usize>) {
        let server = self
            .server
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let port = self
            .port
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let password = self
            .password
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let uuid = self
            .uuid
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let sni = self
            .sni
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let plugin = self
            .plugin
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let plugin_opts = self
            .plugin_opts
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let plugin_args = self
            .plugin_args
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let remarks = self
            .remarks
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let timeout = self
            .timeout
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let group = self
            .group
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let proxy_port = self
            .proxy_port
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();

        let protocol = protocol_override.unwrap_or_else(|| {
            self.protocol_select
                .as_ref()
                .and_then(|sel| sel.read(cx).selected_index(cx).map(|ip| ip.row))
                .unwrap_or(self.protocol)
        });
        let method = self
            .method_select
            .as_ref()
            .and_then(|sel| sel.read(cx).selected_index(cx).map(|ip| ip.row))
            .unwrap_or(self.method);

        let mut invalid: Vec<&str> = Vec::new();
        let server_port = match port.trim().parse::<u16>() {
            Ok(v) => v,
            Err(_) => {
                invalid.push("server port");
                443
            }
        };
        let timeout_v = match timeout.trim().parse::<u32>() {
            Ok(v) => v,
            Err(_) => {
                invalid.push("timeout");
                5
            }
        };
        let proxy_port_v = match proxy_port.trim().parse::<u16>() {
            Ok(v) => v,
            Err(_) => {
                invalid.push("proxy port");
                1080
            }
        };
        let plugin_args_v = if self.need_plugin_arg {
            non_empty_text(&plugin_args)
        } else {
            None
        };

        {
            let g = &mut self.gui;
            g.normalize_selected_index();
            if let Some(p) = g.selected_profile_mut() {
                p.protocol = ProxyProtocol::from_index(protocol as u32);
                p.server = server.trim().to_string();
                p.server_port = server_port;
                p.password = password;
                p.uuid = uuid.trim().to_string();
                p.sni = non_empty_text(&sni);
                p.allow_insecure = self.allow_insecure;
                p.method = SS_METHODS
                    .get(method)
                    .copied()
                    .unwrap_or("chacha20-ietf-poly1305")
                    .to_string();
                p.plugin = non_empty_text(&plugin);
                p.plugin_opts = non_empty_text(&plugin_opts);
                p.plugin_args = plugin_args_v;
                let remarks_trim = remarks.trim().to_string();
                p.name = if remarks_trim.is_empty() {
                    "New Server".to_string()
                } else {
                    remarks_trim
                };
                p.timeout = timeout_v;
                p.group = non_empty_text(&group);
            }
            let (addr, _) = crate::util::split_host_port(&g.config.socks_listen);
            g.config.socks_listen = crate::util::format_host_port(addr, proxy_port_v);
            g.runtime.close_to_tray = self.close_to_tray;
        }

        if !invalid.is_empty() {
            self.set_status(&format!("Status: invalid {}", invalid.join(", ")), cx);
        }
    }

    /// Convenience wrapper — saves fields without a protocol override.
    fn save_fields(&mut self, cx: &mut Context<Self>) {
        self.save_fields_with_protocol(cx, None);
    }

    // ── Button handlers ───────────────────────────────────────────────────

    fn add_clicked(&mut self, cx: &mut Context<Self>) {
        let n = self.gui.profiles.profiles.len() + 1;
        let p = ProxyProfile {
            name: t!("misc.new_server", n = n).to_string(),
            ..Default::default()
        };
        self.gui.profiles.profiles.push(p);
        self.gui.runtime.selected_profile = self.gui.profiles.profiles.len() - 1;
        self.sync_tray_servers();
        self.pending_reload = true;
        cx.notify();
    }

    fn delete_clicked(&mut self, cx: &mut Context<Self>) {
        if self.gui.profiles.profiles.len() > 1 {
            let idx = self.gui.runtime.selected_profile;
            self.gui.profiles.profiles.remove(idx);
            self.gui.normalize_selected_index();
            self.sync_tray_servers();
            self.pending_reload = true;
            cx.notify();
        }
    }

    fn duplicate_clicked(&mut self, cx: &mut Context<Self>) {
        let idx = self.gui.runtime.selected_profile;
        if let Some(p) = self.gui.profiles.profiles.get(idx).cloned() {
            self.gui.profiles.profiles.insert(idx + 1, p);
            self.gui.runtime.selected_profile = idx + 1;
            self.sync_tray_servers();
            self.pending_reload = true;
            cx.notify();
        }
    }

    fn move_up_clicked(&mut self, cx: &mut Context<Self>) {
        let idx = self.gui.runtime.selected_profile;
        if idx > 0 {
            self.gui.profiles.profiles.swap(idx, idx - 1);
            self.gui.runtime.selected_profile = idx - 1;
            self.sync_tray_servers();
            self.pending_reload = true;
            cx.notify();
        }
    }

    fn move_down_clicked(&mut self, cx: &mut Context<Self>) {
        let idx = self.gui.runtime.selected_profile;
        if idx + 1 < self.gui.profiles.profiles.len() {
            self.gui.profiles.profiles.swap(idx, idx + 1);
            self.gui.runtime.selected_profile = idx + 1;
            self.sync_tray_servers();
            self.pending_reload = true;
            cx.notify();
        }
    }

    /// Persist config to disk and record the timestamp so the file-watcher
    /// debounce can ignore these self-inflicted writes.
    fn flush_and_record(&mut self) -> anyhow::Result<()> {
        let result = self.gui.flush();
        self.last_flush_at = std::time::Instant::now();
        result
    }

    fn start_selected(&mut self, cx: &mut Context<Self>) {
        self.save_fields(cx);
        if let Err(err) = self.flush_and_record() {
            self.set_status(&t!("status.save_failed", err = err.to_string()), cx);
            return;
        }
        let profile = match self.gui.selected_profile().cloned() {
            Some(p) => p,
            None => {
                self.set_status(&t!("status.no_server"), cx);
                return;
            }
        };
        let config_snap = self.gui.config.clone();
        match self.gui.core_manager.start_profile(&config_snap, &profile) {
            Ok(()) => {
                self.set_status(
                    &t!(
                        "status.running",
                        proto = profile.protocol.label(),
                        name = profile.display_name()
                    ),
                    cx,
                );
                self.gui.runtime.was_running = true;
                let _ = self.flush_and_record();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = true;
                    ts.active_server_name = profile.display_name();
                }
            }
            Err(err) => self.set_status(&t!("status.start_failed", err = err.to_string()), cx),
        }
    }

    fn stop_core(&mut self, cx: &mut Context<Self>) {
        match self.gui.core_manager.stop() {
            Ok(()) => {
                self.set_status(&t!("status.stopped"), cx);
                self.gui.runtime.was_running = false;
                let _ = self.flush_and_record();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = false;
                    ts.active_server_name = String::new();
                }
            }
            Err(err) => self.set_status(&t!("status.stop_failed", err = err.to_string()), cx),
        }
    }

    fn import_link(&mut self, input: &str, cx: &mut Context<Self>) {
        match link::import_share_link(input.trim()) {
            Ok(imported) => {
                let idx = self.gui.runtime.selected_profile;
                if let Some(p) = self.gui.profiles.profiles.get_mut(idx) {
                    imported.apply_to(p);
                }
                self.sync_tray_servers();
                self.pending_reload = true;
                self.set_status(&t!("status.imported"), cx);
            }
            Err(err) => self.set_status(&t!("status.import_failed", err = err.to_string()), cx),
        }
    }

    fn export_link(&mut self, cx: &mut Context<Self>) {
        let url = match self.gui.selected_profile() {
            Some(p) => link::export_share_link(p),
            None => {
                self.set_status(&t!("status.no_server_selected"), cx);
                return;
            }
        };
        match url {
            Ok(url) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(url));
                self.set_status(&t!("status.url_copied"), cx);
            }
            Err(err) => self.set_status(&t!("status.export_failed", err = err.to_string()), cx),
        }
    }

    fn ok_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_fields(cx);
        match self.flush_and_record() {
            Ok(()) => {
                Self::suppress_quit(cx, true);
                window.remove_window();
            }
            Err(err) => self.set_status(&t!("status.save_failed", err = err.to_string()), cx),
        }
    }

    fn cancel_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_reload = true;
        Self::suppress_quit(cx, true);
        window.remove_window();
    }

    /// Apply a new `AppConfig` snapshot from a dialog (PAC settings) and make
    /// it take effect (persist + restart/update the local PAC server).
    pub(crate) fn apply_pac_config(
        &mut self,
        cfg: crate::config::AppConfig,
        cx: &mut Context<Self>,
    ) {
        let force_restart = self.gui.config.pac_listen != cfg.pac_listen;
        self.gui.config = cfg;
        let _ = self.flush_and_record();
        let _ = restart_pac_server(&mut self.gui, force_restart);
        if let Ok(mut ts) = self.tray_shared.lock() {
            ts.system_proxy_mode = self.gui.config.system_proxy_mode;
        }
        cx.notify();
    }

    /// Trigger an immediate PAC rule download (used by the PAC dialog's
    /// "Update Now" button).
    pub(crate) fn update_pac_rules_now(&mut self, cx: &mut Context<Self>) {
        self.start_pac_download(cx);
    }

    /// Apply a new `RuntimeState` snapshot from a dialog (startup settings).
    pub(crate) fn apply_runtime_state(
        &mut self,
        state: crate::config::RuntimeState,
        cx: &mut Context<Self>,
    ) {
        self.gui.runtime = state;
        let _ = apply_autostart(&self.gui.runtime);
        let _ = self.flush_and_record();
        cx.notify();
    }

    /// Snapshot of the current app configuration (for dialogs).
    pub(crate) fn config_snapshot(&self) -> crate::config::AppConfig {
        self.gui.config.clone()
    }

    /// Snapshot of the current runtime state (for dialogs).
    pub(crate) fn runtime_snapshot(&self) -> crate::config::RuntimeState {
        self.gui.runtime.clone()
    }

    fn apply_clicked(&mut self, cx: &mut Context<Self>) {
        self.save_fields(cx);
        match self.flush_and_record() {
            Ok(()) => {
                let _ = system_proxy::apply_system_proxy(&self.gui.config);
                self.set_status(&t!("status.saved"), cx);
            }
            Err(err) => self.set_status(&t!("status.save_failed", err = err.to_string()), cx),
        }
    }

    fn suppress_quit(cx: &mut Context<Self>, val: bool) {
        cx.spawn(async move |_this, cx| {
            let _ = cx.update(|app| {
                let g = app.default_global::<AppRoot>();
                g.suppress_quit = val;
            });
        })
        .detach();
    }

    fn toggle_show_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_password = !self.show_password;
        if let Some(p) = &self.password {
            p.update(cx, |s, scx| s.set_masked(!self.show_password, window, scx));
        }
        cx.notify();
    }

    fn toggle_allow_insecure(&mut self, cx: &mut Context<Self>) {
        self.allow_insecure = !self.allow_insecure;
        cx.notify();
    }

    fn toggle_need_plugin_arg(&mut self, cx: &mut Context<Self>) {
        self.need_plugin_arg = !self.need_plugin_arg;
        cx.notify();
    }

    fn toggle_close_to_tray(&mut self, cx: &mut Context<Self>) {
        self.close_to_tray = !self.close_to_tray;
        cx.notify();
    }

    fn select_server(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.gui.profiles.profiles.len() {
            self.gui.runtime.selected_profile = idx;
        }
        self.sync_tray_servers();
        self.pending_reload = true;
        cx.notify();
    }

    fn sync_tray_servers(&mut self) {
        if let Ok(mut ts) = self.tray_shared.lock() {
            ts.server_names = self
                .gui
                .profiles
                .profiles
                .iter()
                .map(|p| p.display_name())
                .collect();
            ts.active_server_idx = self.gui.runtime.selected_profile;
        }
    }

    // ── PAC download (spawns a background thread) ─────────────────────────

    fn start_pac_download(&mut self, cx: &mut Context<Self>) {
        if self.gui.pac_update_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        self.gui.pac_update_rx = Some(rx);
        let data_dir = self.gui.storage.paths().config_dir.clone();
        let direct_url = self.gui.config.pac_direct_url.clone();
        let proxy_url = self.gui.config.pac_proxy_url.clone();
        std::thread::spawn(move || {
            let _ = tx.send(pac::download_rules(&data_dir, &direct_url, &proxy_url).map(|_| ()));
        });
        self.set_status(&t!("status.pac_downloading"), cx);
    }

    // ── Startup connection ────────────────────────────────────────────────

    fn apply_startup_connection(&mut self, cx: &mut Context<Self>) {
        let should_start = match self.gui.runtime.startup_connection_state {
            StartupConnectionState::On => true,
            StartupConnectionState::LastState => self.gui.runtime.was_running,
            StartupConnectionState::Off => false,
        };
        if should_start {
            if let Some(profile) = self.gui.selected_profile().cloned() {
                let config = self.gui.config.clone();
                match self.gui.core_manager.start_profile(&config, &profile) {
                    Ok(()) => {
                        let name = profile.display_name();
                        self.set_status(
                            &t!(
                                "status.running",
                                proto = profile.protocol.label(),
                                name = name.clone()
                            ),
                            cx,
                        );
                        self.gui.runtime.was_running = true;
                        let _ = self.flush_and_record();
                        if let Ok(mut ts) = self.tray_shared.lock() {
                            ts.is_running = true;
                            ts.active_server_name = name;
                        }
                    }
                    Err(err) => {
                        self.set_status(&t!("status.start_failed", err = err.to_string()), cx);
                    }
                }
            }
        }
        cx.notify();
    }

    // ── Tray events ───────────────────────────────────────────────────────

    fn handle_tray_event(&mut self, ev: TrayEvent, cx: &mut Context<Self>) {
        match ev {
            TrayEvent::ShowEditServers => {
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(open_main_window);
                })
                .detach();
            }
            TrayEvent::ShowPacSettings => {
                let this = cx.weak_entity();
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| crate::pac_dialog::open(&this, app));
                })
                .detach();
            }
            TrayEvent::ShowStartupSettings => {
                let this = cx.weak_entity();
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| crate::startup_dialog::open(&this, app));
                })
                .detach();
            }
            TrayEvent::SetSystemProxy(mode) => {
                self.gui.config.system_proxy_mode = mode;
                let _ = self.flush_and_record();
                let snap = self.gui.config.clone();
                let _ = system_proxy::apply_system_proxy(&snap);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.system_proxy_mode = mode;
                }
                self.set_status(&t!("status.system_proxy", mode = mode.label()), cx);
            }
            TrayEvent::SetPacRuleMode(mode) => {
                self.gui.config.pac_rule_mode = mode;
                let _ = self.flush_and_record();
                let _ = restart_pac_server(&mut self.gui, false);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.pac_rule_mode = mode;
                }
                self.set_status(&t!("status.pac_rule", mode = mode.label()), cx);
            }
            TrayEvent::UpdatePacRules => {
                self.start_pac_download(cx);
            }
            TrayEvent::SelectServer(idx) => {
                self.select_server(idx, cx);
            }
            TrayEvent::ImportFromClipboard => {
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(open_main_window);
                })
                .detach();
            }
            TrayEvent::ToggleProxy => {
                if self.gui.core_manager.is_running() {
                    self.stop_core(cx);
                } else {
                    self.start_selected(cx);
                }
            }
            TrayEvent::QuitApp => {
                let _ = self.gui.core_manager.stop();
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| app.quit());
                })
                .detach();
            }
        }
    }

    // ── Config hot-reload ──────────────────────────────────────────────

    fn spawn_config_watcher(&mut self, dir: std::path::PathBuf) {
        use notify::Watcher;
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        }) {
            Ok(mut watcher) => {
                if watcher
                    .watch(&dir, notify::RecursiveMode::Recursive)
                    .is_ok()
                {
                    self.config_watcher = Some(watcher);
                    self.config_reload_rx = Some(rx);
                } else {
                    tracing::warn!("failed to watch {:?} for config hot-reload", dir);
                }
            }
            Err(e) => tracing::warn!("config hot-reload disabled: {e}"),
        }
    }

    fn reload_config_from_disk(&mut self, cx: &mut Context<Self>) {
        let storage = self.gui.storage.clone();
        match (storage.load_profiles(), storage.load_runtime_state()) {
            (Ok(profiles), Ok(runtime)) => {
                self.gui.profiles = profiles;
                self.gui.runtime = runtime;
                self.close_to_tray = self.gui.runtime.close_to_tray;
                self.gui.normalize_selected_index();
                self.pending_reload = true;
                cx.notify();
                tracing::info!("config reloaded from disk");
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!("config reload from disk failed: {e}");
            }
        }
    }

    // ── Periodic poll (runs every 300 ms from the spawned task) ───────────

    fn poll(&mut self, cx: &mut Context<Self>) {
        // ── Config hot-reload (debounced: ignore self-inflicted writes) ──
        if self.config_reload_rx.is_some() {
            // Drain ALL pending events from the watcher channel.
            let mut has_event = false;
            while self
                .config_reload_rx
                .as_ref()
                .map(|rx| rx.try_recv().is_ok())
                .unwrap_or(false)
            {
                has_event = true;
            }
            // Only reload if the event is NOT caused by our own flush().
            // The watcher may fire multiple times per flush (3 files written);
            // we debounce by requiring >1 s since the last flush.
            if has_event && self.last_flush_at.elapsed() > std::time::Duration::from_secs(1) {
                self.reload_config_from_disk(cx);
            }
        }

        loop {
            match self.tray_rx.try_recv() {
                Ok(ev) => self.handle_tray_event(ev, cx),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        crate::tray::poll(
            self.gui._tray_service.as_mut(),
            &self.tray_shared,
            &self.tray_tx,
        );

        // Poll PAC download completion.
        if let Some(rx) = &self.gui.pac_update_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.gui.pac_update_rx = None;
                    let _ = restart_pac_server(&mut self.gui, false);
                    self.set_status(&t!("status.pac_updated"), cx);
                }
                Ok(Err(e)) => {
                    self.gui.pac_update_rx = None;
                    self.set_status(&t!("status.pac_download_failed", err = e.to_string()), cx);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.gui.pac_update_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Poll the core process status.
        match self.gui.core_manager.poll() {
            Ok(Some(exit)) => {
                self.set_status(&t!("status.core_exited", code = exit.to_string()), cx);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = false;
                    ts.active_server_name = String::new();
                }
            }
            Ok(None) if self.gui.core_manager.is_running() => {
                let proto = self
                    .gui
                    .core_manager
                    .current_protocol()
                    .unwrap_or(ProxyProtocol::Juicity)
                    .label();
                let name = self
                    .gui
                    .selected_profile()
                    .map(|p| p.display_name())
                    .unwrap_or_default();
                self.set_status(&t!("status.running", proto = proto, name = name), cx);
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = true;
                    ts.active_server_name = name;
                }
            }
            Err(err) => {
                self.set_status(&t!("status.poll_error", err = err.to_string()), cx);
            }
            _ => {}
        }
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.inputs_inited {
            self.init_inputs(window, cx);
            self.inputs_inited = true;
        }
        if self.pending_reload {
            self.load_fields(window, cx);
            self.pending_reload = false;
        }
        if self.protocol_changed {
            self.load_fields(window, cx);
            self.protocol_changed = false;
        }

        let this = cx.weak_entity();
        let is_juicity = self.protocol == 0;

        let selected_profile = self.gui.runtime.selected_profile;
        let server_rows = self.gui.profiles.profiles.iter().enumerate().map({
            let this = this.clone();
            move |(i, p)| {
                let selected = i == selected_profile;
                let this = this.clone();
                let name = p.display_name();
                div()
                    .id(("server-row", i))
                    .px_2()
                    .py_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(rgb(0xddf4ff)).text_color(rgb(0x0969da)))
                    .hover(|s| {
                        s.bg(if selected {
                            rgb(0xddf4ff)
                        } else {
                            rgb(0xf0f3f6)
                        })
                    })
                    .on_click(move |_e, _w, cx| {
                        this.update(cx, |view, cx| view.select_server(i, cx)).ok();
                    })
                    .child(name)
            }
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf6f8fa))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .size_full()
                    // ── Left panel: server list ───────────────────────────
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(210.))
                            .flex_none()
                            .h_full()
                            .bg(rgb(0xffffff))
                            .border_r_1()
                            .border_color(rgb(0xd0d7de))
                            .child(
                                div()
                                    .id("server-list")
                                    .flex_grow()
                                    .overflow_y_scroll()
                                    .children(server_rows),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_1()
                                    .p_1()
                                    .child(btn(
                                        "add-btn",
                                        t!("btn.add").to_string(),
                                        false,
                                        with_view(&this, AppView::add_clicked),
                                    ))
                                    .child(btn(
                                        "del-btn",
                                        t!("btn.delete").to_string(),
                                        false,
                                        with_view(&this, AppView::delete_clicked),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .child(btn(
                                        "dup-btn",
                                        t!("btn.duplicate").to_string(),
                                        false,
                                        with_view(&this, AppView::duplicate_clicked),
                                    ))
                                    .child(btn(
                                        "up-btn",
                                        t!("btn.up").to_string(),
                                        false,
                                        with_view(&this, AppView::move_up_clicked),
                                    ))
                                    .child(btn(
                                        "dn-btn",
                                        t!("btn.down").to_string(),
                                        false,
                                        with_view(&this, AppView::move_down_clicked),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .child(btn(
                                        "import-btn",
                                        t!("btn.import_url").to_string(),
                                        false,
                                        {
                                            let this = this.clone();
                                            move |_e, _w, cx| {
                                                if let Some(text) =
                                                    cx.read_from_clipboard().and_then(|item| item.text())
                                                {
                                                    this.update(cx, |view, cx| view.import_link(&text, cx))
                                                        .ok();
                                                }
                                            }
                                        },
                                    ))
                                    .child(btn(
                                        "export-btn",
                                        t!("btn.export_url").to_string(),
                                        false,
                                        with_view(&this, AppView::export_link),
                                    )),
                            ),
                    )
                    // ── Right panel: editor ──────────────────────────────
                    .child(
                        div()
                            .id("editor-scroll")
                            .flex_grow()
                            .h_full()
                            .overflow_y_scroll()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .mb_1()
                                    .child(t!("field.server_hdr").to_string()),
                            )
                            .child(widgets::field_row(
                                t!("field.protocol").to_string(),
                                Select::new(self.protocol_select.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                t!("field.server_ip").to_string(),
                                Input::new(self.server.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                t!("field.server_port").to_string(),
                                Input::new(self.port.as_ref().unwrap()),
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
                                            .child(t!("field.password").to_string()),
                                    )
                                    .child(Input::new(self.password.as_ref().unwrap()))
                                    .child(chk(
                                        "show-pwd-check",
                                        t!("field.show_password").to_string(),
                                        self.show_password,
                                        {
                                            let this = this.clone();
                                            move |_checked, window, cx| {
                                                let _ = this.update(cx, |view, vcx| {
                                                    view.toggle_show_password(window, vcx)
                                                });
                                            }
                                        },
                                    )),
                            )
                            .when(is_juicity, |el| {
                                el.child(separator())
                                    .child(widgets::field_row(
                                        t!("field.uuid").to_string(),
                                        Input::new(self.uuid.as_ref().unwrap()),
                                    ))
                                    .child(widgets::field_row(
                                        t!("field.sni").to_string(),
                                        Input::new(self.sni.as_ref().unwrap()),
                                    ))
                                    .child(div().pl(px(138.)).child(chk(
                                        "allow-insecure-check",
                                        t!("field.allow_insecure").to_string(),
                                        self.allow_insecure,
                                        {
                                            let this = this.clone();
                                            move |_checked, _window, cx| {
                                                let _ = this.update(cx, |view, cx| {
                                                    view.toggle_allow_insecure(cx)
                                                });
                                            }
                                        },
                                    )))
                            })
                            .when(!is_juicity, |el| {
                                el.child(separator())
                                    .child(widgets::field_row(
                                        t!("field.encryption").to_string(),
                                        Select::new(self.method_select.as_ref().unwrap()),
                                    ))
                                    .child(widgets::field_row(
                                        t!("field.plugin_program").to_string(),
                                        Input::new(self.plugin.as_ref().unwrap()),
                                    ))
                                    .child(widgets::field_row(
                                        t!("field.plugin_options").to_string(),
                                        Input::new(self.plugin_opts.as_ref().unwrap()),
                                    ))
                                    .child(div().pl(px(138.)).child(chk(
                                        "need-plugin-arg-check",
                                        t!("field.need_plugin_arg").to_string(),
                                        self.need_plugin_arg,
                                        {
                                            let this = this.clone();
                                            move |_checked, _window, cx| {
                                                let _ = this.update(cx, |view, cx| {
                                                    view.toggle_need_plugin_arg(cx)
                                                });
                                            }
                                        },
                                    )))
                                    .when(self.need_plugin_arg, |el| {
                                        el.child(widgets::field_row(
                                            t!("field.plugin_args").to_string(),
                                            Input::new(self.plugin_args.as_ref().unwrap()),
                                        ))
                                    })
                            })
                            .child(separator())
                            .child(widgets::field_row(
                                t!("field.remarks").to_string(),
                                Input::new(self.remarks.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                t!("field.timeout").to_string(),
                                Input::new(self.timeout.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                t!("field.group").to_string(),
                                Input::new(self.group.as_ref().unwrap()),
                            )),
                    ),
            )
            // ── Status bar ────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(0xd0d7de))
                    .bg(rgb(0xffffff))
                    .child(
                        div()
                            .flex_grow()
                            .text_sm()
                            .text_color(rgb(0x57606a))
                            .child(self.status.clone()),
                    )
                    .child(btn(
                        "start-btn",
                        t!("btn.start").to_string(),
                        false,
                        with_view(&this, AppView::start_selected),
                    ))
                    .child(btn(
                        "stop-btn",
                        t!("btn.stop").to_string(),
                        false,
                        with_view(&this, AppView::stop_core),
                    )),
            )
            // ── Bottom bar ────────────────────────────────────────────────
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(0xd0d7de))
                    .bg(rgb(0xffffff))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x57606a))
                            .child(t!("field.proxy_port").to_string()),
                    )
                    .child(div().w(px(90.)).child(Input::new(self.proxy_port.as_ref().unwrap())))
                    .child(chk(
                        "close-to-tray-check",
                        t!("field.close_to_tray").to_string(),
                        self.close_to_tray,
                        {
                            let this = this.clone();
                            move |_checked, _window, cx| {
                                let _ = this.update(cx, |view, cx| view.toggle_close_to_tray(cx));
                            }
                        },
                    ))
                    .child(btn(
                        "pac-settings-btn",
                        t!("btn.pac_settings").to_string(),
                        false,
                        {
                            let this = this.clone();
                            move |_e, _w, cx| {
                                crate::pac_dialog::open(&this, cx);
                            }
                        },
                    ))
                    .child(div().flex_grow())
                    .child(btn(
                        "ok-btn",
                        t!("btn.ok").to_string(),
                        true,
                        {
                            let this = this.clone();
                            move |_e, window, cx| {
                                this.update(cx, |view, cx| view.ok_clicked(window, cx))
                                    .ok();
                            }
                        },
                    ))
                    .child(btn(
                        "cancel-btn",
                        t!("btn.cancel").to_string(),
                        false,
                        {
                            let this = this.clone();
                            move |_e, window, cx| {
                                this.update(cx, |view, cx| view.cancel_clicked(window, cx))
                                    .ok();
                            }
                        },
                    ))
                    .child(btn(
                        "apply-btn",
                        t!("btn.apply").to_string(),
                        false,
                        with_view(&this, AppView::apply_clicked),
                    )),
            )
    }
}

/// Build a click handler that routes to a `&mut self` view method.
fn with_view<F>(
    this: &WeakEntity<AppView>,
    f: F,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static
where
    F: Fn(&mut AppView, &mut Context<AppView>) + 'static,
{
    let this = this.clone();
    move |_e, _w, cx| {
        let _ = this.update(cx, |view, cx| f(view, cx));
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

/// Apply or remove system auto-start for the application.
#[allow(unused_variables)]
fn apply_autostart(state: &RuntimeState) -> anyhow::Result<()> {
    fn autostart_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(&home).join(".config/autostart")
    }

    if !state.auto_start {
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

        let exe =
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("juicity-gui"));

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
        tracing::info!(
            "autostart desktop file created at {}",
            desktop_file.display()
        );
    }

    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    gpui::Application::new().run(|cx: &mut App| {
        crate::icon::install();
        gpui_component::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let view = cx.new(AppView::new);
        cx.default_global::<AppRoot>().view = Some(view.clone());

        let _ = cx.on_window_closed({
            let view = view.downgrade();
            move |cx| {
                // Main-window close is handled by `on_window_should_close` in
                // `open_main_window`.  This observer is a safety net: if the
                // flag was NOT set (e.g. the window was removed programmatically
                // without going through the close-request path), handle it here.
                let already_closed = cx.default_global::<AppRoot>().main_window_closed;
                if already_closed {
                    return;
                }
                let close_to_tray = view
                    .update(cx, |v, _| v.gui.runtime.close_to_tray)
                    .unwrap_or(false);
                let g = cx.default_global::<AppRoot>();
                g.main_window_closed = true;
                g.main_window = None;
                let suppress = g.suppress_quit;
                g.suppress_quit = false;
                if !suppress && !close_to_tray {
                    cx.quit();
                }
            }
        })
        .detach();

        let hide = view.read(cx).gui.runtime.hide_window_on_startup;
        if hide {
            // Treat the never-shown main window as already closed so that
            // dialog windows (opened from the tray) can be closed freely.
            cx.default_global::<AppRoot>().main_window_closed = true;
        } else {
            open_main_window(cx);
        }

        let _ = view.update(cx, |view, cx| view.apply_startup_connection(cx));
        cx.activate(true);
    });
    Ok(())
}
