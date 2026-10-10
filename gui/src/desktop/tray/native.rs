//! Tray icon for Windows and macOS via `tray-icon`. Created and updated on
//! the main thread while the event loop runs.

use super::{
    escape_mnemonic, pac_index, proxy_index, Sink, TrayEvent, TrayMenu, APP_ID, PAC_RULES,
    PROXY_MODES,
};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct NativeTray {
    icon: TrayIcon,
}

impl NativeTray {
    pub fn start(menu: &TrayMenu, sink: Sink) -> Option<Self> {
        let menu_sink = Sink::clone(&sink);
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some(event) = TrayEvent::from_menu_id(&event.id.0) {
                menu_sink(event);
            }
        }));
        // A left click toggles the window on Windows; macOS opens the menu.
        #[cfg(target_os = "windows")]
        tray_icon::TrayIconEvent::set_event_handler(Some(
            move |event: tray_icon::TrayIconEvent| {
                if let tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } = event
                {
                    sink(TrayEvent::ToggleWindow);
                }
            },
        ));
        #[cfg(not(target_os = "windows"))]
        drop(sink);

        let builder = TrayIconBuilder::new()
            .with_id(APP_ID)
            .with_icon(icon()?)
            .with_tooltip(tooltip(menu))
            .with_menu(Box::new(build_menu(menu)));
        #[cfg(target_os = "windows")]
        let builder = builder.with_menu_on_left_click(false);
        match builder.build() {
            Ok(icon) => Some(Self { icon }),
            Err(err) => {
                tracing::warn!("tray icon creation failed: {err}");
                None
            }
        }
    }

    pub fn update(&self, menu: &TrayMenu) {
        self.icon.set_menu(Some(Box::new(build_menu(menu))));
        if let Err(err) = self.icon.set_tooltip(Some(tooltip(menu))) {
            tracing::debug!("tray tooltip update failed: {err}");
        }
    }
}

fn tooltip(menu: &TrayMenu) -> String {
    format!("juicity\n{}", menu.tooltip())
}

/// The 32 px PNG rendered by build.rs.
fn icon() -> Option<Icon> {
    const PNG_32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/32.png"));
    let image = match image::load_from_memory(PNG_32) {
        Ok(image) => image.into_rgba8(),
        Err(err) => {
            tracing::warn!("tray icon image is invalid: {err}");
            return None;
        }
    };
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height)
        .map_err(|err| tracing::warn!("tray icon image is invalid: {err}"))
        .ok()
}

fn item(label: &str, event: TrayEvent) -> MenuItem {
    MenuItem::with_id(event.menu_id(), escape_mnemonic(label, '&'), true, None)
}

fn check(label: &str, event: TrayEvent, checked: bool) -> CheckMenuItem {
    CheckMenuItem::with_id(
        event.menu_id(),
        escape_mnemonic(label, '&'),
        true,
        checked,
        None,
    )
}

fn submenu(label: &str, items: &[&dyn IsMenuItem]) -> Submenu {
    let label = escape_mnemonic(label, '&');
    Submenu::with_items(&label, true, items).unwrap_or_else(|err| {
        tracing::warn!("tray submenu failed: {err}");
        Submenu::new(&label, true)
    })
}

/// The menu of §7.17, with check items standing in for radio groups.
fn build_menu(menu: &TrayMenu) -> Menu {
    let model = &menu.model;
    let text = &menu.labels;
    let proxy_labels = [&text.off, &text.pac, &text.global];
    let proxy_items: Vec<CheckMenuItem> = PROXY_MODES
        .iter()
        .zip(proxy_labels)
        .enumerate()
        .map(|(i, (mode, label))| {
            check(
                label,
                TrayEvent::SetProxyMode(*mode),
                i == proxy_index(model.proxy_mode),
            )
        })
        .collect();
    let rule_labels = [&text.bypass_china, &text.gfw_list];
    let rule_items: Vec<CheckMenuItem> = PAC_RULES
        .iter()
        .zip(rule_labels)
        .enumerate()
        .map(|(i, (rule, label))| {
            check(
                label,
                TrayEvent::SetPacRule(*rule),
                i == pac_index(model.pac_rule),
            )
        })
        .collect();
    let proxy_separator = PredefinedMenuItem::separator();
    let update_rules = item(&text.update_rules, TrayEvent::UpdateRules);
    let mut proxy: Vec<&dyn IsMenuItem> = Vec::new();
    proxy.extend(proxy_items.iter().map(|i| i as &dyn IsMenuItem));
    proxy.push(&proxy_separator);
    proxy.extend(rule_items.iter().map(|i| i as &dyn IsMenuItem));
    proxy.push(&update_rules);
    let proxy = submenu(&text.system_proxy, &proxy);

    let node_items: Vec<CheckMenuItem> = model
        .nodes
        .iter()
        .enumerate()
        .map(|(i, name)| check(name, TrayEvent::SelectNode(i), model.active == Some(i)))
        .collect();
    let node_separator = PredefinedMenuItem::separator();
    let import = item(&text.import_clipboard, TrayEvent::ImportClipboard);
    let mut nodes: Vec<&dyn IsMenuItem> = node_items.iter().map(|i| i as &dyn IsMenuItem).collect();
    if !node_items.is_empty() {
        nodes.push(&node_separator);
    }
    nodes.push(&import);
    let nodes = submenu(&text.nodes, &nodes);

    let status = MenuItem::new(escape_mnemonic(&text.status, '&'), false, None);
    let connect = item(&text.connect, TrayEvent::ToggleConnection);
    let open = item(&text.open, TrayEvent::Open);
    let edit_nodes = item(&text.edit_nodes, TrayEvent::ShowNodes);
    let logs = item(&text.logs, TrayEvent::ShowLogs);
    let settings = item(&text.settings, TrayEvent::ShowSettings);
    let about = item(&text.about, TrayEvent::ShowAbout);
    let quit = item(&text.quit, TrayEvent::Quit);
    let (sep1, sep2) = (
        PredefinedMenuItem::separator(),
        PredefinedMenuItem::separator(),
    );
    let root = Menu::new();
    let entries: [&dyn IsMenuItem; 12] = [
        &status,
        &connect,
        &proxy,
        &nodes,
        &sep1,
        &open,
        &edit_nodes,
        &logs,
        &settings,
        &about,
        &sep2,
        &quit,
    ];
    if let Err(err) = root.append_items(&entries) {
        tracing::warn!("tray menu failed: {err}");
    }
    root
}

/// The macOS application menu. It replaces Slint's default one, whose Quit
/// item sends `terminate:` and so skips the save prompt and the shutdown
/// that restores the system proxy (`ui::select_backend` turns the default
/// off). Quit here is a plain item with the tray's Quit id and Cmd-Q, so it
/// reaches the UI thread through the [`MenuEvent`] handler set in
/// [`NativeTray::start`] and runs the same request as tray Quit.
#[cfg(target_os = "macos")]
pub struct AppMenu {
    // Kept alive while it is the main menu.
    _menu: Menu,
    quit: MenuItem,
}

#[cfg(target_os = "macos")]
impl AppMenu {
    /// Install the menu on NSApp. Must run on the main thread.
    pub fn install(quit_label: &str) -> Option<Self> {
        use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};
        let quit = MenuItem::with_id(
            super::APP_MENU_QUIT.menu_id(),
            escape_mnemonic(quit_label, '&'),
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyQ)),
        );
        // Slint's default app menu, except for Quit. macOS titles the first
        // submenu with the app name.
        let app = Submenu::new("App", true);
        let menu = Menu::new();
        let built = menu.append(&app).and_then(|_| {
            app.append_items(&[
                &PredefinedMenuItem::about(None, None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &quit,
            ])
        });
        if let Err(err) = built {
            tracing::warn!("application menu failed: {err}");
            return None;
        }
        menu.init_for_nsapp();
        Some(Self { _menu: menu, quit })
    }

    pub fn set_quit_label(&self, label: &str) {
        self.quit.set_text(escape_mnemonic(label, '&'));
    }
}
