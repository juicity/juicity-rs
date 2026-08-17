//! Application state layer for the Juicity GUI.
//!
//! Holds the persistent [`GuiState`] (config/profiles/runtime + the proxy core
//! manager) and the helpers that tie them together. Keeping this separate from
//! `app.rs` (which owns the GPUI rendering + view logic) mirrors the `core`/`ui`
//! split used by larger GPUI apps and makes the state testable on its own.

use crate::config::{
    AppConfig, ProfileStore, ProxyProfile, RuntimeState, Storage,
};
use crate::core::CoreManager;
use crate::pac;
use crate::tray::TrayService;
use std::sync::mpsc::Receiver;

pub struct GuiState {
    pub storage: Storage,
    pub config: AppConfig,
    pub profiles: ProfileStore,
    pub runtime: RuntimeState,
    pub core_manager: CoreManager,
    pub pac_server: Option<pac::PacServer>,
    pub pac_update_rx: Option<Receiver<anyhow::Result<()>>>,
    pub _tray_service: Option<TrayService>,
}

impl GuiState {
    pub fn new() -> anyhow::Result<Self> {
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

    pub fn flush(&self) -> anyhow::Result<()> {
        self.storage.save_app_config(&self.config)?;
        self.storage.save_profiles(&self.profiles)?;
        self.storage.save_runtime_state(&self.runtime)?;
        Ok(())
    }

    pub fn selected_profile(&self) -> Option<&ProxyProfile> {
        self.profiles.profiles.get(self.runtime.selected_profile)
    }

    pub fn selected_profile_mut(&mut self) -> Option<&mut ProxyProfile> {
        self.profiles
            .profiles
            .get_mut(self.runtime.selected_profile)
    }

    pub fn normalize_selected_index(&mut self) {
        if self.profiles.profiles.is_empty() {
            self.profiles.profiles.push(ProxyProfile::default());
        }
        if self.runtime.selected_profile >= self.profiles.profiles.len() {
            self.runtime.selected_profile = self.profiles.profiles.len().saturating_sub(1);
        }
    }
}

/// Restart or update the PAC server with fresh rules from disk.
///
/// If `force_restart` is `true` (e.g. the listen address changed), a new
/// server is started even if one already exists.  Otherwise the existing
/// server is updated in-place, or a new one is started if none exists.
pub fn restart_pac_server(state: &mut GuiState, force_restart: bool) -> anyhow::Result<()> {
    let (direct, proxy) = pac::load_rules(&state.storage.paths().config_dir);
    let content = pac::generate_pac(
        state.config.pac_rule_mode,
        &state.config.socks_listen,
        &direct,
        &proxy,
    );
    if force_restart || state.pac_server.is_none() {
        state.pac_server = Some(pac::start(&state.config.pac_listen, content)?);
    } else if let Some(srv) = &state.pac_server {
        srv.update(content);
    }
    Ok(())
}

pub fn extract_port(addr: &str) -> u16 {
    addr.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1080)
}

pub fn non_empty_text(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
