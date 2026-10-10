//! System tray for the Slint frontend: ksni on Linux, tray-icon on Windows
//! and macOS.
//!
//! The UI thread builds a [`TrayMenu`] (controller model plus translated
//! labels) and pushes it with [`Tray::update`], which never blocks. Clicks
//! come back as [`TrayEvent`]s through a [`Sink`] that posts them to the UI
//! thread and never waits for a reply.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native;

use crate::config::{PacRuleMode, SystemProxyMode};
use std::sync::Arc;

/// Tray id and theme icon name; matches the `.desktop` file.
pub const APP_ID: &str = "io.juicity.gui";

/// The event behind Quit in the macOS application menu. It shares the tray's
/// menu event handler, so it runs the same request as tray Quit.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const APP_MENU_QUIT: TrayEvent = TrayEvent::Quit;

/// What the tray shows, built by the controller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayModel {
    pub connected: bool,
    pub active_name: String,
    pub proxy_mode: SystemProxyMode,
    pub pac_rule: PacRuleMode,
    /// Display names of all nodes, in list order.
    pub nodes: Vec<String>,
    /// Index of the active node in `nodes`.
    pub active: Option<usize>,
}

/// Translated tray strings, read from the `TrayText` global.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayLabels {
    /// Connection status line (also the first tooltip line).
    pub status: String,
    /// System proxy status (second tooltip line).
    pub proxy_status: String,
    pub connect: String,
    pub system_proxy: String,
    pub off: String,
    pub pac: String,
    pub global: String,
    pub bypass_china: String,
    pub gfw_list: String,
    pub update_rules: String,
    pub nodes: String,
    pub import_clipboard: String,
    pub open: String,
    pub edit_nodes: String,
    pub logs: String,
    pub settings: String,
    pub about: String,
    pub quit: String,
}

/// One complete tray state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayMenu {
    pub model: TrayModel,
    pub labels: TrayLabels,
}

impl TrayMenu {
    /// Tooltip body: status lines below the "Juicity GUI" title.
    pub fn tooltip(&self) -> String {
        format!("{}\n{}", self.labels.status, self.labels.proxy_status)
    }
}

/// A tray click, handled on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click on the icon: show or hide the window.
    ToggleWindow,
    ToggleConnection,
    SetProxyMode(SystemProxyMode),
    SetPacRule(PacRuleMode),
    UpdateRules,
    SelectNode(usize),
    ImportClipboard,
    Open,
    ShowNodes,
    ShowLogs,
    ShowSettings,
    ShowAbout,
    Quit,
}

// Used by the native menu; tested on every platform.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
impl TrayEvent {
    /// Menu item id for the native (Windows/macOS) menu.
    pub fn menu_id(self) -> String {
        match self {
            Self::ToggleWindow => "window".into(),
            Self::ToggleConnection => "connection".into(),
            Self::SetProxyMode(mode) => format!("proxy:{}", proxy_index(mode)),
            Self::SetPacRule(rule) => format!("pac:{}", pac_index(rule)),
            Self::UpdateRules => "rules".into(),
            Self::SelectNode(index) => format!("node:{index}"),
            Self::ImportClipboard => "import".into(),
            Self::Open => "open".into(),
            Self::ShowNodes => "nodes".into(),
            Self::ShowLogs => "logs".into(),
            Self::ShowSettings => "settings".into(),
            Self::ShowAbout => "about".into(),
            Self::Quit => "quit".into(),
        }
    }

    /// Inverse of [`Self::menu_id`]; `None` for ids the tray did not create.
    pub fn from_menu_id(id: &str) -> Option<Self> {
        let indexed = |prefix: &str| id.strip_prefix(prefix)?.parse::<usize>().ok();
        Some(match id {
            "window" => Self::ToggleWindow,
            "connection" => Self::ToggleConnection,
            "rules" => Self::UpdateRules,
            "import" => Self::ImportClipboard,
            "open" => Self::Open,
            "nodes" => Self::ShowNodes,
            "logs" => Self::ShowLogs,
            "settings" => Self::ShowSettings,
            "about" => Self::ShowAbout,
            "quit" => Self::Quit,
            _ => {
                if let Some(i) = indexed("proxy:") {
                    Self::SetProxyMode(*PROXY_MODES.get(i)?)
                } else if let Some(i) = indexed("pac:") {
                    Self::SetPacRule(*PAC_RULES.get(i)?)
                } else {
                    Self::SelectNode(indexed("node:")?)
                }
            }
        })
    }
}

/// Delivers tray events to the UI thread without waiting.
pub type Sink = Arc<dyn Fn(TrayEvent) + Send + Sync>;

/// System proxy radio items, in menu order.
pub const PROXY_MODES: [SystemProxyMode; 3] = [
    SystemProxyMode::Disable,
    SystemProxyMode::Pac,
    SystemProxyMode::Global,
];

/// PAC rule radio items, in menu order.
pub const PAC_RULES: [PacRuleMode; 2] = [PacRuleMode::BypassChina, PacRuleMode::ProxyGfw];

pub fn proxy_index(mode: SystemProxyMode) -> usize {
    PROXY_MODES.iter().position(|m| *m == mode).unwrap_or(0)
}

pub fn pac_index(rule: PacRuleMode) -> usize {
    PAC_RULES.iter().position(|r| *r == rule).unwrap_or(0)
}

/// Double the access-key marker so menus show a label (e.g. a node name)
/// verbatim: `_` for D-Bus menus, `&` for Windows and macOS.
pub fn escape_mnemonic(label: &str, marker: char) -> String {
    let mut out = String::with_capacity(label.len());
    for c in label.chars() {
        out.push(c);
        if c == marker {
            out.push(marker);
        }
    }
    out
}

/// A running tray icon. Dropping it removes the icon.
pub struct Tray {
    #[cfg(target_os = "linux")]
    inner: linux::LinuxTray,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    inner: Option<native::NativeTray>,
    #[cfg(target_os = "macos")]
    app_menu: Option<native::AppMenu>,
}

impl Tray {
    /// Create the tray. On Windows and macOS this must run on the main
    /// thread with the event loop running.
    pub fn start(menu: TrayMenu, sink: Sink) -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                inner: linux::LinuxTray::start(menu, sink),
            }
        }
        #[cfg(target_os = "windows")]
        {
            Self {
                inner: native::NativeTray::start(&menu, sink),
            }
        }
        // After the tray, which sets the menu event handler both menus use.
        #[cfg(target_os = "macos")]
        {
            let inner = native::NativeTray::start(&menu, sink);
            Self {
                inner,
                app_menu: native::AppMenu::install(&menu.labels.quit),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            let _ = (menu, sink);
            tracing::info!("system tray is not supported on this platform");
            Self {}
        }
    }

    /// Whether an icon is shown, so a hidden window can be reopened.
    pub fn is_available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.inner.is_available()
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            self.inner.is_some()
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            false
        }
    }

    /// Show a new state. Never blocks.
    pub fn update(&self, menu: TrayMenu) {
        #[cfg(target_os = "linux")]
        self.inner.update(menu);
        #[cfg(target_os = "macos")]
        if let Some(app_menu) = &self.app_menu {
            app_menu.set_quit_label(&menu.labels.quit);
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if let Some(inner) = &self.inner {
            inner.update(&menu);
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        let _ = menu;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_indices_follow_menu_order() {
        assert_eq!(proxy_index(SystemProxyMode::Disable), 0);
        assert_eq!(proxy_index(SystemProxyMode::Global), 2);
        assert_eq!(pac_index(PacRuleMode::ProxyGfw), 1);
        for (i, mode) in PROXY_MODES.iter().enumerate() {
            assert_eq!(proxy_index(*mode), i);
        }
    }

    #[test]
    fn mnemonic_markers_are_doubled() {
        assert_eq!(escape_mnemonic("tokyo_01", '_'), "tokyo__01");
        assert_eq!(escape_mnemonic("A&B", '&'), "A&&B");
        assert_eq!(escape_mnemonic("A&B", '_'), "A&B");
        assert_eq!(escape_mnemonic("", '&'), "");
    }

    #[test]
    fn menu_ids_round_trip() {
        let events = [
            TrayEvent::ToggleWindow,
            TrayEvent::ToggleConnection,
            TrayEvent::SetProxyMode(SystemProxyMode::Pac),
            TrayEvent::SetPacRule(PacRuleMode::ProxyGfw),
            TrayEvent::UpdateRules,
            TrayEvent::SelectNode(12),
            TrayEvent::ImportClipboard,
            TrayEvent::Open,
            TrayEvent::ShowNodes,
            TrayEvent::ShowLogs,
            TrayEvent::ShowSettings,
            TrayEvent::ShowAbout,
            TrayEvent::Quit,
        ];
        for event in events {
            assert_eq!(TrayEvent::from_menu_id(&event.menu_id()), Some(event));
        }
        assert_eq!(TrayEvent::from_menu_id("proxy:3"), None);
        assert_eq!(TrayEvent::from_menu_id("node:x"), None);
        assert_eq!(TrayEvent::from_menu_id("status"), None);
    }

    #[test]
    fn tooltip_has_both_status_lines() {
        let menu = TrayMenu {
            labels: TrayLabels {
                status: "Connected · Tokyo".into(),
                proxy_status: "System proxy · PAC".into(),
                ..TrayLabels::default()
            },
            ..TrayMenu::default()
        };
        assert_eq!(menu.tooltip(), "Connected · Tokyo\nSystem proxy · PAC");
    }
}
