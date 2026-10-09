use crate::config::{
    method_to_index, AppConfig, ProfileStore, ProxyProfile, ProxyProtocol, RuntimeState,
    StartupConnectionState, SystemProxyMode, SS_METHODS,
};
use crate::link;
use crate::pac;
use crate::state::{extract_port, non_empty_text, restart_pac_server, GuiState};
use crate::system_proxy;
use crate::system_theme;
use crate::tray::{TrayEvent, TraySharedState};
use crate::validate::RequiredField;
use crate::widgets;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::IndexPath;
use gpui_kit::prelude::*;
use gpui_kit::{
    actions, div, point, px, size, AnyWindowHandle, App, Bounds, ClickEvent, Context, ElementId,
    Entity, FontWeight, Global, KeyBinding, SharedString, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions,
};
use rust_i18n::t;
use std::sync::{Arc, Mutex};
use std::time::Duration;

actions!(app, [Quit]);

/// The user's answer to the unsaved-changes prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SaveChoice {
    /// Save the pending edits.
    Save,
    /// Throw the pending edits away.
    Discard,
    /// Keep editing.
    Cancel,
}

/// What to do once the unsaved-changes prompt has been answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PendingClose {
    /// Close the main window, which may simply hide it to the tray.
    Window,
    /// Quit the application.
    Quit,
}

/// App-wide registry that survives the main window being closed to the tray.
#[derive(Default)]
struct AppRoot {
    view: Option<Entity<AppView>>,
    main_window: Option<WindowHandle<gpui_kit::base::Root>>,
    /// Set right before the main window is closed via OK/Cancel so the
    /// `on_window_closed` handler keeps the app alive in the tray.
    suppress_quit: bool,
    /// Whether the main window has already been closed (to the tray).  Once
    /// set, closing *dialog* windows never quits the application; only the
    /// first close of the main window is able to trigger the quit path.
    main_window_closed: bool,
}

impl Global for AppRoot {}

/// Invisible window that is never closed.
///
/// gpui ends the event loop as soon as the last window is closed (Windows
/// posts `WM_QUIT`, X11/Wayland stop the loop), which would make "close to
/// tray" quit the whole app. Keeping this window alive lets the main window
/// and the dialogs be closed and reopened freely.
struct AnchorView;

impl Render for AnchorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn open_anchor_window(cx: &mut App) {
    let options = WindowOptions {
        // `show: false` is honored on Windows/macOS; elsewhere the window is
        // simply a 1x1 transparent undecorated surface parked off-screen.
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(-32000.), px(-32000.)),
            size(px(1.), px(1.)),
        ))),
        titlebar: None,
        focus: false,
        show: false,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("io.juicity.gui".to_string()),
        ..Default::default()
    };
    if let Err(err) = cx.open_window(options, |window, cx| {
        // Follow the system light/dark preference, including later changes.
        window
            .observe_window_appearance(|window, cx| crate::system_theme::sync(window, cx))
            .detach();
        cx.new(|_| AnchorView)
    }) {
        tracing::warn!("failed to open the background anchor window: {err}");
    }
}

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
                    let view_entity = cx.default_global::<AppRoot>().view.clone();

                    // Unsaved edits: ask before the window goes away, then
                    // close again once the prompt is answered.
                    let unsaved = view_entity
                        .as_ref()
                        .is_some_and(|v| v.read(cx).has_unsaved_changes());
                    if unsaved {
                        if let Some(view) = view_entity {
                            cx.spawn(async move |cx| {
                                let _ = cx.update(|app| {
                                    let _ = view.update(app, |view, cx| {
                                        view.request_close(PendingClose::Window, cx)
                                    });
                                });
                            })
                            .detach();
                        }
                        return false;
                    }

                    // Read close_to_tray before mutating the global.
                    let view_entity = cx
                        .default_global::<AppRoot>()
                        .view
                        .clone();
                    let close_to_tray = view_entity
                        .as_ref()
                        .map(|v| v.read(cx).can_hide_to_tray())
                        .unwrap_or(false);
                    {
                        let g = cx.default_global::<AppRoot>();
                        g.main_window_closed = true;
                        g.main_window = None;
                        let suppress = g.suppress_quit;
                        g.suppress_quit = false;
                        tracing::debug!(suppress, close_to_tray, "main window close requested");
                        if !suppress && !close_to_tray {
                            cx.quit();
                        }
                    }
                    true
                });

                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
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
    tray_service: Option<crate::tray::TrayService>,

    // ── Editor text fields (gpui-kit InputState; built lazily on first render) ──
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
    /// Whether the "running" status has already been shown for the current
    /// core run (see `poll`).
    announced_running: bool,
    // ── Config hot-reload ───────────────────────────────────────────────
    #[allow(dead_code)]
    config_watcher: Option<notify::RecommendedWatcher>,
    config_reload_rx: Option<std::sync::mpsc::Receiver<()>>,
    /// Timestamp of the last `flush()` call – used to ignore self-inflicted
    /// watcher events that would otherwise cause an infinite reload loop.
    last_flush_at: std::time::Instant,
    /// Settings as of the last save.  Compared against the live state to tell
    /// whether the editor holds changes the user has not saved yet.
    saved_profiles: ProfileStore,
    saved_config: AppConfig,
    saved_close_to_tray: bool,
    /// What to do after the unsaved-changes prompt is answered; `Some` only
    /// while that prompt is open.
    pending_close: Option<PendingClose>,
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
        let tray_service = Some(crate::tray::start(
            tray_tx.clone(),
            Arc::clone(&tray_shared),
        ));

        let protocol_options: Vec<SharedString> = vec![
            t!("protocol.juicity").to_string().into(),
            t!("protocol.shadowsocks").to_string().into(),
        ];
        let method_options: Vec<SharedString> =
            SS_METHODS.iter().map(|s| SharedString::from(*s)).collect();

        let close_to_tray = gui.runtime.close_to_tray;
        let saved_profiles = gui.profiles.clone();
        let saved_config = gui.config.clone();
        let mut view = Self {
            gui,
            tray_tx,
            tray_rx,
            tray_shared,
            tray_service,
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
            close_to_tray,
            inputs_inited: false,
            pending_reload: false,
            protocol_changed: false,
            status: t!("status.stopped").to_string(),
            announced_running: false,
            config_watcher: None,
            config_reload_rx: None,
            last_flush_at: std::time::Instant::now(),
            saved_profiles,
            saved_config,
            saved_close_to_tray: close_to_tray,
            pending_close: None,
        };

        // ── Config hot-reload watcher ──
        view.spawn_config_watcher(view.gui.storage.paths().config_dir.clone());

        // Stop the core and restore the system proxy however the app quits.
        cx.on_app_quit(|view, _cx| {
            view.shutdown();
            async {}
        })
        .detach();

        // ── Periodic poll loop: tray events + PAC + core status ────────────
        cx.spawn(async move |this, cx| {
            let mut timer = cx.background_executor().timer(Duration::from_millis(300));
            loop {
                timer.await;
                if this.update(cx, |view, cx| view.poll(cx)).is_err() {
                    break;
                }
                timer = cx.background_executor().timer(Duration::from_millis(300));
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

        // The subscription callback already holds the `AppView` lease, so it
        // must use `view` directly; updating the same entity through a weak
        // handle here would be a double lease and the change would be lost.
        cx.subscribe(&protocol_select, |view, _state, event, cx| {
            if let SelectEvent::Confirm(Some(value)) = event {
                let new_protocol = view
                    .protocol_options
                    .iter()
                    .position(|o| o == value)
                    .unwrap_or(0);
                // Pass the new protocol index so save_fields writes the
                // correct value even though SelectState may not have
                // updated yet at Confirm-event time.
                if view.save_fields_with_protocol(cx, Some(new_protocol)) {
                    // In-memory edit only; the user still has to save.
                    view.protocol_changed = true;
                } else if let Some(p) = view.gui.selected_profile_mut() {
                    // Invalid fields: keep what the user typed and only
                    // switch the protocol.
                    p.protocol = ProxyProtocol::from_index(new_protocol as u32);
                }
                view.protocol = new_protocol;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&method_select, |view, _state, event, cx| {
            if let SelectEvent::Confirm(Some(value)) = event {
                view.method = view
                    .method_options
                    .iter()
                    .position(|o| o == value)
                    .unwrap_or(0);
                // In-memory edit only; the user still has to save.
                view.save_fields(cx);
                cx.notify();
            }
        })
        .detach();

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
        let port = extract_port(&self.gui.config.mixed_listen);
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
    ///
    /// Returns `false` (leaving the profile untouched) when a numeric field is
    /// invalid; the status bar then names the offending fields.
    fn save_fields_with_protocol(
        &mut self,
        cx: &mut Context<Self>,
        protocol_override: Option<usize>,
    ) -> bool {
        // The inputs are built lazily on first render; saving before that would
        // overwrite the profile with empty values.
        if !self.inputs_inited {
            return true;
        }
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

        let parse_port = |text: &str| text.trim().parse::<u16>().ok().filter(|p| *p > 0);
        let server_port = parse_port(&port);
        let timeout_v = timeout.trim().parse::<u32>().ok();
        let proxy_port_v = parse_port(&proxy_port);
        let (Some(server_port), Some(timeout_v), Some(proxy_port_v)) =
            (server_port, timeout_v, proxy_port_v)
        else {
            let mut invalid: Vec<String> = Vec::new();
            if server_port.is_none() {
                invalid.push(t!("field.server_port").to_string());
            }
            if timeout_v.is_none() {
                invalid.push(t!("field.timeout").to_string());
            }
            if proxy_port_v.is_none() {
                invalid.push(t!("field.proxy_port").to_string());
            }
            self.set_status(
                &t!("status.invalid_fields", fields = invalid.join(", ")),
                cx,
            );
            return false;
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
            let (addr, _) = crate::util::split_host_port(&g.config.mixed_listen);
            g.config.mixed_listen = crate::util::format_host_port(addr, proxy_port_v);
            g.runtime.close_to_tray = self.close_to_tray;
        }
        true
    }

    /// The local mixed (SOCKS5 + HTTP) port changed: regenerate the PAC,
    /// re-point the system proxy and restart the core so everything uses the
    /// new port.
    ///
    /// Only reached from [`Self::save_and_apply`]: the change must not take
    /// effect before the user saves it.
    fn apply_listen_side_effects(&mut self, cx: &mut Context<Self>) {
        let _ = restart_pac_server(&mut self.gui, false);
        if self.gui.config.system_proxy_mode != SystemProxyMode::Disable {
            if let Err(err) = self.apply_system_proxy_now() {
                self.set_status(&t!("status.system_proxy_failed", err = err.to_string()), cx);
            }
        }
        if self.gui.core_manager.is_running() {
            self.start_core(cx);
        }
    }

    /// Convenience wrapper — saves fields without a protocol override.
    fn save_fields(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_fields_with_protocol(cx, None)
    }

    // ── Button handlers ───────────────────────────────────────────────────

    fn add_clicked(&mut self, cx: &mut Context<Self>) {
        if !self.save_fields(cx) {
            return;
        }
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
        if !self.save_fields(cx) {
            return;
        }
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
        if !self.save_fields(cx) {
            return;
        }
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
        if !self.save_fields(cx) {
            return;
        }
        let idx = self.gui.runtime.selected_profile;
        if idx + 1 < self.gui.profiles.profiles.len() {
            self.gui.profiles.profiles.swap(idx, idx + 1);
            self.gui.runtime.selected_profile = idx + 1;
            self.sync_tray_servers();
            self.pending_reload = true;
            cx.notify();
        }
    }

    /// Whether closing the main window may keep the app alive in the tray.
    /// Without a working tray icon the window could never be reopened.
    fn can_hide_to_tray(&self) -> bool {
        self.gui.runtime.close_to_tray && self.tray_available()
    }

    fn tray_available(&self) -> bool {
        self.tray_service.as_ref().is_some_and(|t| t.is_available())
    }

    /// Persist config to disk and record the timestamp so the file-watcher
    /// debounce can ignore these self-inflicted writes.
    ///
    /// A successful flush is a save point: everything currently in memory
    /// counts as saved afterwards.
    fn flush_and_record(&mut self) -> anyhow::Result<()> {
        let result = self.gui.flush();
        self.last_flush_at = std::time::Instant::now();
        if result.is_ok() {
            self.mark_saved();
        }
        result
    }

    /// Persist only the runtime state.  Starting or stopping the proxy must
    /// not write edits the user has not saved yet.
    fn flush_runtime(&mut self) {
        if self.gui.flush_runtime().is_ok() {
            self.last_flush_at = std::time::Instant::now();
        }
    }

    /// Whether the editor holds changes that have not been saved yet.
    fn has_unsaved_changes(&self) -> bool {
        self.gui.profiles != self.saved_profiles
            || self.gui.config != self.saved_config
            || self.gui.runtime.close_to_tray != self.saved_close_to_tray
    }

    /// Remember the current settings as the saved state.
    fn mark_saved(&mut self) {
        self.saved_profiles = self.gui.profiles.clone();
        self.saved_config = self.gui.config.clone();
        self.saved_close_to_tray = self.gui.runtime.close_to_tray;
    }

    /// Throw away every edit made since the last save.
    fn discard_changes(&mut self) {
        self.gui.profiles = self.saved_profiles.clone();
        self.gui.config = self.saved_config.clone();
        self.gui.runtime.close_to_tray = self.saved_close_to_tray;
        self.close_to_tray = self.saved_close_to_tray;
        self.gui.normalize_selected_index();
        self.sync_tray_servers();
        self.pending_reload = true;
    }

    fn start_selected(&mut self, cx: &mut Context<Self>) {
        // Starting uses what the editor currently shows, but must not persist
        // it: only OK and Apply save.
        if !self.save_fields(cx) {
            return;
        }
        self.start_core(cx);
    }

    /// Start the core for the selected profile (after checking it is complete)
    /// and update the status bar / tray.
    fn start_core(&mut self, cx: &mut Context<Self>) {
        let profile = match self.gui.selected_profile().cloned() {
            Some(p) => p,
            None => {
                self.set_status(&t!("status.no_server"), cx);
                return;
            }
        };
        let missing: Vec<String> = crate::validate::missing_fields(&profile)
            .into_iter()
            .map(required_field_label)
            .collect();
        if !missing.is_empty() {
            self.set_status(
                &t!("status.incomplete_profile", fields = missing.join(", ")),
                cx,
            );
            return;
        }
        let config_snap = self.gui.config.clone();
        // A new core restarts its byte counters, so start a fresh baseline.
        crate::traffic::monitor().reset();
        match self.gui.core_manager.start_profile(&config_snap, &profile) {
            Ok(()) => {
                self.announced_running = true;
                self.set_status(
                    &t!(
                        "status.running",
                        proto = profile.protocol.label(),
                        name = profile.display_name()
                    ),
                    cx,
                );
                self.gui.runtime.was_running = true;
                self.flush_runtime();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = true;
                    ts.active_server_name = profile.display_name();
                }
            }
            Err(err) => self.set_status(&t!("status.start_failed", err = err.to_string()), cx),
        }
    }

    fn stop_core(&mut self, cx: &mut Context<Self>) {
        crate::traffic::monitor().reset();
        match self.gui.core_manager.stop() {
            Ok(()) => {
                self.announced_running = false;
                self.set_status(&t!("status.stopped"), cx);
                self.gui.runtime.was_running = false;
                self.flush_runtime();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.is_running = false;
                    ts.active_server_name = String::new();
                }
            }
            Err(err) => self.set_status(&t!("status.stop_failed", err = err.to_string()), cx),
        }
    }

    fn import_link(&mut self, input: &str, cx: &mut Context<Self>) {
        if !self.save_fields(cx) {
            return;
        }
        match link::import_share_link(input.trim()) {
            Ok(imported) => {
                // Import as a new server instead of overwriting the selected one.
                let mut profile = ProxyProfile::default();
                imported.apply_to(&mut profile);
                self.gui.profiles.profiles.push(profile);
                self.gui.runtime.selected_profile = self.gui.profiles.profiles.len() - 1;
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
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(url));
                self.set_status(&t!("status.url_copied"), cx);
            }
            Err(err) => self.set_status(&t!("status.export_failed", err = err.to_string()), cx),
        }
    }

    fn ok_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_and_apply(cx) {
            return;
        }
        Self::suppress_quit(cx, self.tray_available());
        window.remove_window();
    }

    fn cancel_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Closing without saving throws the pending edits away.
        if self.has_unsaved_changes() {
            self.discard_changes();
        }
        self.pending_reload = true;
        Self::suppress_quit(cx, self.tray_available());
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
        if !self.save_and_apply(cx) {
            return;
        }
        match self.apply_system_proxy_now() {
            Ok(()) => self.set_status(&t!("status.saved"), cx),
            Err(err) => {
                self.set_status(&t!("status.system_proxy_failed", err = err.to_string()), cx)
            }
        }
    }

    /// Validate the editor, keep its changes and persist them.
    ///
    /// Shared by the OK/Apply buttons and the unsaved-changes prompt; returns
    /// `false` (saving nothing) when a field is invalid.
    fn save_and_apply(&mut self, cx: &mut Context<Self>) -> bool {
        let previous_listen = self.gui.config.mixed_listen.clone();
        if !self.save_fields(cx) {
            return false;
        }
        let listen_changed = self.gui.config.mixed_listen != previous_listen;
        if let Err(err) = self.flush_and_record() {
            self.set_status(&t!("status.save_failed", err = err.to_string()), cx);
            return false;
        }
        if listen_changed {
            self.apply_listen_side_effects(cx);
        }
        true
    }

    /// Ask about unsaved changes before doing `next`.
    fn request_close(&mut self, next: PendingClose, cx: &mut Context<Self>) {
        if self.pending_close.is_some() {
            return;
        }
        if !self.has_unsaved_changes() {
            finish_close(next, cx);
            return;
        }
        self.pending_close = Some(next);
        crate::save_prompt::open(cx.weak_entity(), cx);
    }

    /// Answer to the unsaved-changes prompt, from the prompt window.
    pub(crate) fn resolve_unsaved(&mut self, choice: SaveChoice, cx: &mut Context<Self>) {
        match choice {
            SaveChoice::Save => {
                if !self.save_and_apply(cx) {
                    // Invalid input: keep the editor open so it can be fixed.
                    self.pending_close = None;
                    return;
                }
            }
            SaveChoice::Discard => self.discard_changes(),
            SaveChoice::Cancel => {
                self.pending_close = None;
                cx.notify();
                return;
            }
        }
        let next = self.pending_close.take().unwrap_or(PendingClose::Window);
        finish_close(next, cx);
    }

    fn apply_system_proxy_now(&self) -> anyhow::Result<()> {
        system_proxy::apply_system_proxy(&self.gui.config)
    }

    /// Stop the core and restore the OS proxy settings so quitting never
    /// leaves the machine pointing at a dead local proxy.
    fn shutdown(&mut self) {
        self.gui.core_manager.stop_and_wait();
        if self.gui.config.system_proxy_mode != SystemProxyMode::Disable {
            let mut cfg = self.gui.config.clone();
            cfg.system_proxy_mode = SystemProxyMode::Disable;
            if let Err(err) = system_proxy::apply_system_proxy(&cfg) {
                tracing::warn!("failed to restore system proxy on exit: {err}");
            }
        }
        // Changes the user never confirmed must not be written to disk.
        if self.has_unsaved_changes() {
            self.discard_changes();
        }
        let _ = self.flush_and_record();
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
        // Commit pending edits to the current server before switching away.
        if idx != self.gui.runtime.selected_profile && !self.save_fields(cx) {
            return;
        }
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
            self.start_core(cx);
        }
        // Re-apply the saved system proxy mode; it is reset to "disabled" on exit.
        if self.gui.config.system_proxy_mode != SystemProxyMode::Disable {
            if let Err(err) = self.apply_system_proxy_now() {
                tracing::warn!("failed to apply system proxy at startup: {err}");
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
            TrayEvent::ShowAbout => {
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| crate::about_dialog::open(app));
                })
                .detach();
            }
            TrayEvent::ShowLogs => {
                cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|app| crate::log_dialog::open(app));
                })
                .detach();
            }
            TrayEvent::SetSystemProxy(mode) => {
                self.gui.config.system_proxy_mode = mode;
                let _ = self.flush_and_record();
                if let Ok(mut ts) = self.tray_shared.lock() {
                    ts.system_proxy_mode = mode;
                }
                match self.apply_system_proxy_now() {
                    Ok(()) => self.set_status(&t!("status.system_proxy", mode = mode.label()), cx),
                    Err(err) => self
                        .set_status(&t!("status.system_proxy_failed", err = err.to_string()), cx),
                }
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
                // Like Shadowsocks-Windows: picking a server while connected
                // switches the connection to it.
                if self.gui.core_manager.is_running()
                    && self.gui.runtime.selected_profile == idx
                {
                    self.start_core(cx);
                }
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
                // Ask about unsaved changes first; the core and system proxy
                // are cleaned up by the app-quit hook.
                if self.has_unsaved_changes() {
                    self.request_close(PendingClose::Quit, cx);
                } else {
                    cx.spawn(async move |_this, cx| {
                        let _ = cx.update(|app| app.quit());
                    })
                    .detach();
                }
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
                // Only the reloaded parts become the new baseline; an unsaved
                // edit to the config stays unsaved.
                self.saved_profiles = self.gui.profiles.clone();
                self.saved_close_to_tray = self.gui.runtime.close_to_tray;
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
        // Sample the core's byte counters for the log window's chart.
        crate::traffic::monitor().record(self.gui.core_manager.traffic());
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
            self.tray_service.as_mut(),
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

        // Poll the embedded core status.
        match self.gui.core_manager.poll() {
            Ok(Some(reason)) => {
                self.announced_running = false;
                self.set_status(&t!("status.core_exited", reason = reason), cx);
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
                    .core_manager
                    .current_name()
                    .unwrap_or_default()
                    .to_string();
                // Announce once; re-setting it every tick would wipe transient
                // messages (validation errors, "URL copied", ...) within 300 ms.
                if !self.announced_running {
                    self.announced_running = true;
                    self.set_status(&t!("status.running", proto = proto, name = name.clone()), cx);
                }
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
        let colors = widgets::palette(cx);
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
                    .mx_1()
                    .mb_0p5()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |s| {
                        s.bg(colors.accent).text_color(colors.accent_foreground)
                    })
                    .hover(|s| {
                        s.bg(if selected {
                            colors.accent
                        } else {
                            colors.list_hover
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
            .bg(colors.panel)
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
                            .bg(colors.background)
                            .border_r_1()
                            .border_color(colors.border)
                            .child(
                                div()
                                    .id("server-list")
                                    .flex_grow(1.)
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
                                                let text = cx
                                                    .read_from_clipboard()
                                                    .and_then(|item| item.text())
                                                    .unwrap_or_default();
                                                this.update(cx, |view, cx| view.import_link(&text, cx))
                                                    .ok();
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
                            .flex_grow(1.)
                            .h_full()
                            .overflow_y_scroll()
                            .bg(colors.background)
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
                                colors,
                                t!("field.protocol").to_string(),
                                Select::new(self.protocol_select.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                colors,
                                t!("field.server_ip").to_string(),
                                Input::new(self.server.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                colors,
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
                                            .text_color(colors.muted_foreground)
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
                                el.child(separator(colors))
                                    .child(widgets::field_row(
                                        colors,
                                        t!("field.uuid").to_string(),
                                        Input::new(self.uuid.as_ref().unwrap()),
                                    ))
                                    .child(widgets::field_row(
                                        colors,
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
                                el.child(separator(colors))
                                    .child(widgets::field_row(
                                        colors,
                                        t!("field.encryption").to_string(),
                                        Select::new(self.method_select.as_ref().unwrap()),
                                    ))
                                    .child(widgets::field_row(
                                        colors,
                                        t!("field.plugin_program").to_string(),
                                        Input::new(self.plugin.as_ref().unwrap()),
                                    ))
                                    .child(widgets::field_row(
                                        colors,
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
                                            colors,
                                            t!("field.plugin_args").to_string(),
                                            Input::new(self.plugin_args.as_ref().unwrap()),
                                        ))
                                    })
                            })
                            .child(separator(colors))
                            .child(widgets::field_row(
                                colors,
                                t!("field.remarks").to_string(),
                                Input::new(self.remarks.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                colors,
                                t!("field.timeout").to_string(),
                                Input::new(self.timeout.as_ref().unwrap()),
                            ))
                            .child(widgets::field_row(
                                colors,
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
                    .border_color(colors.border)
                    .bg(colors.panel)
                    .child(
                        div()
                            .flex_grow(1.)
                            .text_sm()
                            .text_color(colors.muted_foreground)
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
                    .border_color(colors.border)
                    .bg(colors.panel)
                    .child(
                        div()
                            .text_sm()
                            .text_color(colors.muted_foreground)
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
                    .child(div().flex_grow(1.))
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

/// Finish a close request once any unsaved changes have been dealt with.
fn finish_close(next: PendingClose, cx: &mut App) {
    match next {
        PendingClose::Quit => cx.quit(),
        // Removing a window runs its window-closed observers synchronously, and
        // the app's observer updates the editor entity.  When this is reached
        // from inside an update of that entity — which is exactly what happens
        // when the prompt answers "don't save" — that would re-enter the entity
        // and panic.  Defer the removal so it runs after the update returns.
        PendingClose::Window => match cx.default_global::<AppRoot>().main_window {
            Some(handle) => {
                cx.spawn(async move |cx| {
                    cx.update(|app| {
                        handle
                            .update(app, |_, window, _| window.remove_window())
                            .ok();
                    });
                })
                .detach();
            }
            None => cx.quit(),
        },
    }
}

/// Localized name of a mandatory profile field.
fn required_field_label(field: RequiredField) -> String {
    match field {
        RequiredField::Server => t!("field.server_ip").to_string(),
        RequiredField::Uuid => t!("field.uuid").to_string(),
        RequiredField::Password => t!("field.password").to_string(),
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

/// Build a gpui-kit `Checkbox`.
fn chk(
    id: impl Into<ElementId>,
    label: impl Into<gpui_kit::component::text::Text>,
    checked: bool,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Checkbox {
    Checkbox::new(id).label(label).checked(checked).on_click(on_click)
}

/// Thin horizontal separator line.
fn separator(colors: widgets::Palette) -> impl IntoElement {
    div().h(px(1.)).w_full().bg(colors.border).my_1()
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
    // The embedded icon doubles as the asset source, so `svg()`/`img()` can
    // resolve it without a file next to the executable.
    let app = gpui_kit::application().with_assets(crate::icon::Assets);
    app.run(|cx: &mut App| {
        crate::icon::install();
        gpui_kit::init(cx);
        // Match the desktop's own accent colour for primary controls and
        // selection highlights.
        system_theme::apply(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| {
            // Do not quit with unsaved edits without asking.
            let view = cx.default_global::<AppRoot>().view.clone();
            match view {
                Some(view) => {
                    let _ = view.update(cx, |view, cx| {
                        view.request_close(PendingClose::Quit, cx)
                    });
                }
                None => cx.quit(),
            }
        });

        open_anchor_window(cx);

        let view = cx.new(AppView::new);
        cx.default_global::<AppRoot>().view = Some(view.clone());

        let _ = cx.on_window_closed({
            let view = view.downgrade();
            move |cx, window_id| {
                // Only the main window's close may quit the application.  Dialog
                // windows (About, PAC, Logs, ...) are opened from the tray while
                // the main window may be open, and closing one must not be
                // mistaken for closing the main window.
                //
                // Main-window close is normally handled by `on_window_should_close`
                // in `open_main_window`, which clears `main_window`; this observer
                // is the safety net for a programmatic removal that skipped it.
                let is_main_window = cx
                    .default_global::<AppRoot>()
                    .main_window
                    .as_ref()
                    .map(|handle| AnyWindowHandle::from(*handle).window_id())
                    .is_some_and(|id| id == window_id);
                if !is_main_window {
                    return;
                }
                let close_to_tray = view
                    .update(cx, |v, _| v.can_hide_to_tray())
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
            // The tray registers asynchronously; if it never shows up the user
            // would have no way to reach the app, so show the window instead.
            let view = view.downgrade();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let _ = cx.update(|app| {
                    let available = view
                        .update(app, |v, _| v.tray_available())
                        .unwrap_or(true);
                    if !available {
                        tracing::warn!("tray icon unavailable; showing the main window");
                        open_main_window(app);
                    }
                });
            })
            .detach();
        } else {
            open_main_window(cx);
        }

        let _ = view.update(cx, |view, cx| view.apply_startup_connection(cx));
        cx.activate(true);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    struct Target;

    impl Render for Target {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// `finish_close` must not remove the window synchronously.
    ///
    /// Removal runs the window-closed observers, which update the editor
    /// entity; doing that from inside an update of that entity panics with
    /// "cannot update ... while it is already being updated".
    #[gpui_kit::test]
    fn finish_close_defers_the_window_removal(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(AppRoot::default());
        });

        let target = cx.update(|cx| cx.new(|_cx| Target));
        cx.update(|cx| {
            let target = target.clone();
            cx.on_window_closed(move |cx, _id| {
                let _ = target.update(cx, |_view, cx| cx.notify());
            })
            .detach();
        });
        let window = cx.add_window({
            let target = target.clone();
            move |window, cx| gpui_kit::base::Root::new(target, window, cx)
        });
        cx.update(|cx| cx.default_global::<AppRoot>().main_window = Some(window));

        // Called the way the prompt does: from inside an update of the entity
        // the window renders.
        cx.update(|app| {
            target.update(app, |_view, cx| finish_close(PendingClose::Window, cx));
        });
        cx.run_until_parked();
    }
}
