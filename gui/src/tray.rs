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
#[derive(Debug, Default, Clone)]
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
    _join: Option<std::thread::JoinHandle<()>>,
}

impl TrayService {
    fn new(join: Option<std::thread::JoinHandle<()>>) -> Self {
        Self { _join: join }
    }
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
        return TrayService::new(Some(join));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (event_tx, shared);
        tracing::info!("system tray is only implemented for Linux in this build");
        TrayService::new(None)
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
