use crate::config::{PacRuleMode, SystemProxyMode};
use rust_i18n::t;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// Events sent from the tray menu to the GTK main loop.
#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    ShowEditServers,
    SetSystemProxy(SystemProxyMode),
    SetPacRuleMode(PacRuleMode),
    UpdatePacRules,
    SelectServer(usize),
    ImportFromClipboard,
    ToggleProxy,
    QuitApp,
}

/// State shared from the GTK app to the tray so the menu can display checkmarks.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TraySharedState {
    pub system_proxy_mode: SystemProxyMode,
    pub pac_rule_mode: PacRuleMode,
    pub server_names: Vec<String>,
    pub active_server_idx: usize,
    pub is_running: bool,
    /// Name of the active profile (used in tray status label).
    pub active_server_name: String,
}

#[derive(Debug)]
pub struct TrayService {
    /// Linux: keeps background ksni thread alive.
    #[cfg(target_os = "linux")]
    _join: Option<std::thread::JoinHandle<()>>,
    /// Windows/macOS: keeps TrayIcon alive on the main thread.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    _tray: Option<Box<dyn std::any::Any>>,
}

/// Start the system tray in a background thread.
pub fn start(event_tx: Sender<TrayEvent>, shared: Arc<Mutex<TraySharedState>>) -> TrayService {
    #[cfg(target_os = "linux")]
    {
        let join = std::thread::spawn(move || {
            use ksni::TrayMethods;

            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!("tray runtime init failed: {err}");
                    return;
                }
            };

            rt.block_on(async move {
                let tray = LinuxTray { event_tx, shared };
                match tray.spawn().await {
                    Ok(_handle) => std::future::pending::<()>().await,
                    Err(err) => tracing::warn!("tray spawn failed: {err}"),
                }
            });
        });
        return TrayService { _join: Some(join) };
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        return start_native(event_tx, shared);
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (event_tx, shared);
        tracing::info!("system tray is not supported on this platform");
        TrayService {}
    }
}

// ── Linux implementation via ksni ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
struct LinuxTray {
    event_tx: Sender<TrayEvent>,
    shared: Arc<Mutex<TraySharedState>>,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for LinuxTray {
    fn id(&self) -> String {
        "io.juicity.gui".to_string()
    }

    fn title(&self) -> String {
        t!("tray.title").to_string()
    }

    fn icon_name(&self) -> String {
        // Return empty — we supply the icon via icon_pixmap() directly so the
        // tray host does not need to search the icon theme.
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Raw ARGB32 big-endian pixel data embedded at compile time.
        const D16: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray_16_argb.raw"));
        const D32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray_32_argb.raw"));
        const D48: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray_48_argb.raw"));
        vec![
            ksni::Icon { width: 16, height: 16, data: D16.to_vec() },
            ksni::Icon { width: 32, height: 32, data: D32.to_vec() },
            ksni::Icon { width: 48, height: 48, data: D48.to_vec() },
        ]
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let state = self.shared.lock().ok()
            .map(|s| s.clone())
            .unwrap_or_default();

        // ── System Proxy sub-menu ─────────────────────────────────────────
        let proxy_mode = state.system_proxy_mode;
        let proxy_items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: format!(
                    "{} {}",
                    if proxy_mode == SystemProxyMode::Disable { "●" } else { "○" },
                    t!("proxy.disable")
                ),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::SetSystemProxy(SystemProxyMode::Disable));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!(
                    "{} {}",
                    if proxy_mode == SystemProxyMode::Pac { "●" } else { "○" },
                    t!("proxy.pac")
                ),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::SetSystemProxy(SystemProxyMode::Pac));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!(
                    "{} {}",
                    if proxy_mode == SystemProxyMode::Global { "●" } else { "○" },
                    t!("proxy.global")
                ),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::SetSystemProxy(SystemProxyMode::Global));
                }),
                ..Default::default()
            }
            .into(),
        ];

        // ── PAC Rules sub-menu ────────────────────────────────────────────
        let pac_mode = state.pac_rule_mode;
        let pac_items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: format!(
                    "{} {}",
                    if pac_mode == PacRuleMode::BypassChina { "●" } else { "○" },
                    t!("pac.bypass_china")
                ),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::SetPacRuleMode(PacRuleMode::BypassChina));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!(
                    "{} {}",
                    if pac_mode == PacRuleMode::ProxyGfw { "●" } else { "○" },
                    t!("pac.gfw_only")
                ),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::SetPacRuleMode(PacRuleMode::ProxyGfw));
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: t!("tray.update_rules").to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::UpdatePacRules);
                }),
                ..Default::default()
            }
            .into(),
        ];

        // ── Servers sub-menu ──────────────────────────────────────────────
        let mut server_items: Vec<ksni::MenuItem<Self>> = state
            .server_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let active = i == state.active_server_idx && state.is_running;
                let ev = TrayEvent::SelectServer(i);
                StandardItem {
                    label: format!("{} {}", if active { "▶" } else { "  " }, name),
                    activate: Box::new(move |this: &mut Self| {
                        let _ = this.event_tx.send(ev);
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        server_items.push(ksni::MenuItem::Separator);
        server_items.push(
            StandardItem {
                label: t!("tray.edit_servers").to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::ShowEditServers);
                }),
                ..Default::default()
            }
            .into(),
        );
        server_items.push(
            StandardItem {
                label: t!("tray.import_clipboard").to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::ImportFromClipboard);
                }),
                ..Default::default()
            }
            .into(),
        );

        // Parent labels show the current active value as a hint.
        let proxy_parent_label = format!(
            "{}: {}",
            t!("tray.system_proxy"),
            proxy_mode.label()
        );
        let active_server_hint = if state.is_running {
            state.server_names
                .get(state.active_server_idx)
                .map(|n| format!(": {n}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let servers_parent_label = format!("{}{}", t!("tray.servers"), active_server_hint);

        // ── Status label (disabled, non-clickable) ────────────────────────
        let status_label = if state.is_running {
            t!("tray.status_running", name = state.active_server_name).to_string()
        } else {
            t!("tray.status_stopped").to_string()
        };
        let toggle_label = if state.is_running {
            t!("tray.stop_proxy").to_string()
        } else {
            t!("tray.start_proxy").to_string()
        };

        vec![
            StandardItem {
                label: status_label,
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: toggle_label,
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::ToggleProxy);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: proxy_parent_label,
                submenu: proxy_items,
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: t!("tray.pac_rules").to_string(),
                submenu: pac_items,
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: servers_parent_label,
                submenu: server_items,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: t!("tray.quit").to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(TrayEvent::QuitApp);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

// ── Windows / macOS implementation via tray-icon + muda ───────────────────────

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn start_native(
    event_tx: Sender<TrayEvent>,
    shared: Arc<Mutex<TraySharedState>>,
) -> TrayService {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuId},
        Icon, TrayIconBuilder,
    };

    // Decode embedded 32 px PNG → RGBA bytes for the tray icon.
    const PNG_32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/32.png"));
    let tray_icon = {
        let img = image::load_from_memory(PNG_32)
            .expect("embedded tray icon PNG is valid")
            .into_rgba8();
        let (w, h) = img.dimensions();
        Icon::from_rgba(img.into_raw(), w, h).expect("tray icon RGBA is valid")
    };

    // Build initial menu.
    let init_state = shared.lock().unwrap().clone();
    let (menu, id_map) = build_native_menu(&init_state);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(tray_icon)
        .with_tooltip("Juicity")
        .build()
        .map_err(|e| tracing::warn!("tray icon creation failed: {e}"))
        .ok();

    let tray = Rc::new(RefCell::new(tray));
    let id_map: Rc<RefCell<HashMap<MenuId, TrayEvent>>> = Rc::new(RefCell::new(id_map));
    let last_state: Rc<RefCell<TraySharedState>> = Rc::new(RefCell::new(init_state));

    let tray_c = tray.clone();
    let id_map_c = id_map.clone();
    let last_state_c = last_state.clone();
    let shared_c = shared.clone();
    let event_tx_c = event_tx;

    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        // Rebuild menu when shared state changes.
        let current = shared_c.lock().unwrap().clone();
        if *last_state_c.borrow() != current {
            let (new_menu, new_ids) = build_native_menu(&current);
            if let Some(t) = tray_c.borrow_mut().as_mut() {
                let _ = t.set_menu(Some(Box::new(new_menu)));
            }
            *id_map_c.borrow_mut() = new_ids;
            *last_state_c.borrow_mut() = current;
        }

        // Dispatch pending menu events.
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if let Some(action) = id_map_c.borrow().get(&ev.id).copied() {
                let _ = event_tx_c.send(action);
            }
        }

        gtk4::glib::ControlFlow::Continue
    });

    TrayService { _tray: Some(Box::new(tray)) }
}

/// Build a `muda` context menu from the current tray state.
/// Returns the menu and a map from `MenuId` → `TrayEvent`.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn build_native_menu(
    state: &TraySharedState,
) -> (
    tray_icon::menu::Menu,
    std::collections::HashMap<tray_icon::menu::MenuId, TrayEvent>,
) {
    use crate::config::{PacRuleMode, SystemProxyMode};
    use std::collections::HashMap;
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let mut ids: HashMap<tray_icon::menu::MenuId, TrayEvent> = HashMap::new();

    // ── Status header (non-clickable) ─────────────────────────────────────
    let status_text = if state.is_running {
        t!("tray.status_running", name = state.active_server_name).to_string()
    } else {
        t!("tray.status_stopped").to_string()
    };
    let status_item = MenuItem::new(&status_text, false, None);

    // ── Toggle start/stop ─────────────────────────────────────────────────
    let toggle_text = if state.is_running {
        t!("tray.stop_proxy").to_string()
    } else {
        t!("tray.start_proxy").to_string()
    };
    let toggle = MenuItem::new(&toggle_text, true, None);
    ids.insert(toggle.id().clone(), TrayEvent::ToggleProxy);

    // ── System Proxy submenu ──────────────────────────────────────────────
    let pm = state.system_proxy_mode;
    let mk_proxy = |mode: SystemProxyMode, label: &str| -> MenuItem {
        let item = MenuItem::new(
            &format!("{} {}", if pm == mode { "●" } else { "○" }, label),
            true,
            None,
        );
        item
    };
    let p_disable = mk_proxy(SystemProxyMode::Disable, &t!("proxy.disable"));
    ids.insert(p_disable.id().clone(), TrayEvent::SetSystemProxy(SystemProxyMode::Disable));
    let p_pac = mk_proxy(SystemProxyMode::Pac, &t!("proxy.pac"));
    ids.insert(p_pac.id().clone(), TrayEvent::SetSystemProxy(SystemProxyMode::Pac));
    let p_global = mk_proxy(SystemProxyMode::Global, &t!("proxy.global"));
    ids.insert(p_global.id().clone(), TrayEvent::SetSystemProxy(SystemProxyMode::Global));

    let proxy_label = format!("{}: {}", t!("tray.system_proxy"), pm.label());
    let proxy_sub = Submenu::with_items(&proxy_label, true, &[&p_disable, &p_pac, &p_global])
        .expect("proxy submenu");

    // ── PAC Rules submenu ─────────────────────────────────────────────────
    let rm = state.pac_rule_mode;
    let r_bypass = MenuItem::new(
        &format!("{} {}", if rm == PacRuleMode::BypassChina { "●" } else { "○" }, t!("pac.bypass_china")),
        true,
        None,
    );
    ids.insert(r_bypass.id().clone(), TrayEvent::SetPacRuleMode(PacRuleMode::BypassChina));
    let r_gfw = MenuItem::new(
        &format!("{} {}", if rm == PacRuleMode::ProxyGfw { "●" } else { "○" }, t!("pac.gfw_only")),
        true,
        None,
    );
    ids.insert(r_gfw.id().clone(), TrayEvent::SetPacRuleMode(PacRuleMode::ProxyGfw));
    let update_rules = MenuItem::new(&t!("tray.update_rules").to_string(), true, None);
    ids.insert(update_rules.id().clone(), TrayEvent::UpdatePacRules);

    let pac_sub = Submenu::with_items(
        &t!("tray.pac_rules").to_string(),
        true,
        &[&r_bypass, &r_gfw, &PredefinedMenuItem::separator(), &update_rules],
    )
    .expect("pac submenu");

    // ── Servers submenu ───────────────────────────────────────────────────
    let server_items: Vec<MenuItem> = state
        .server_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let active = i == state.active_server_idx && state.is_running;
            let item = MenuItem::new(
                &format!("{} {}", if active { "▶" } else { "  " }, name),
                true,
                None,
            );
            ids.insert(item.id().clone(), TrayEvent::SelectServer(i));
            item
        })
        .collect();

    let edit_servers = MenuItem::new(&t!("tray.edit_servers").to_string(), true, None);
    ids.insert(edit_servers.id().clone(), TrayEvent::ShowEditServers);
    let import_clip = MenuItem::new(&t!("tray.import_clipboard").to_string(), true, None);
    ids.insert(import_clip.id().clone(), TrayEvent::ImportFromClipboard);

    let active_hint = if state.is_running {
        state
            .server_names
            .get(state.active_server_idx)
            .map(|n| format!(": {n}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let servers_label = format!("{}{}", t!("tray.servers"), active_hint);

    let mut srv_refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
        server_items.iter().map(|i| i as &dyn tray_icon::menu::IsMenuItem).collect();
    let sep = PredefinedMenuItem::separator();
    srv_refs.push(&sep);
    srv_refs.push(&edit_servers);
    srv_refs.push(&import_clip);
    let servers_sub =
        Submenu::with_items(&servers_label, true, &srv_refs).expect("servers submenu");

    // ── Quit ─────────────────────────────────────────────────────────────
    let quit = MenuItem::new(&t!("tray.quit").to_string(), true, None);
    ids.insert(quit.id().clone(), TrayEvent::QuitApp);

    // ── Assemble root menu ────────────────────────────────────────────────
    let menu = Menu::new();
    menu.append_items(&[
        &status_item,
        &toggle,
        &PredefinedMenuItem::separator(),
        &proxy_sub,
        &pac_sub,
        &servers_sub,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .expect("menu append");

    (menu, ids)
}
