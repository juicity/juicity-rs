//! Application state layer for the Juicity GUI.
//!
//! Holds the persistent [`GuiState`] (config/profiles/runtime + the proxy core
//! manager) and the helpers that tie them together. Keeping this separate from
//! `app.rs` (which owns the GPUI rendering + view logic) mirrors the `core`/`ui`
//! split used by larger GPUI apps and makes the state testable on its own.

use crate::config::{AppConfig, ProfileStore, ProxyProfile, RuntimeState, Storage};
use crate::core::CoreManager;
use crate::pac;
use std::path::Path;
use std::sync::mpsc::Receiver;

pub struct GuiState {
    pub storage: Storage,
    pub config: AppConfig,
    pub profiles: ProfileStore,
    pub runtime: RuntimeState,
    pub core_manager: CoreManager,
    pub pac_server: Option<pac::PacServer>,
    pub pac_update_rx: Option<Receiver<anyhow::Result<()>>>,
}

impl GuiState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::load(Storage::new()?))
    }

    /// Load the three config files from `storage`, recovering from corrupt ones.
    pub fn load(storage: Storage) -> Self {
        Self::load_tracked(storage).0
    }

    /// Like [`GuiState::load`], also returning the bytes parsed for
    /// app.json, profiles.json and runtime.json (`None` when missing or
    /// recovered from corruption).
    pub fn load_tracked(storage: Storage) -> (Self, [Option<Vec<u8>>; 3]) {
        let paths = storage.paths().clone();
        let (config, app): (AppConfig, _) =
            load_or_recover(&paths.app_json, || storage.load_with_bytes(&paths.app_json));
        let (mut profiles, profiles_bytes): (ProfileStore, _) =
            load_or_recover(&paths.profiles_json, || {
                storage.load_with_bytes(&paths.profiles_json)
            });
        let (mut runtime, runtime_bytes): (RuntimeState, _) =
            load_or_recover(&paths.runtime_json, || {
                storage.load_with_bytes(&paths.runtime_json)
            });

        if profiles.profiles.is_empty() {
            profiles.profiles.push(ProxyProfile::default());
            runtime.selected_profile = 0;
        }

        let state = Self {
            storage,
            config,
            profiles,
            runtime,
            core_manager: CoreManager::new(),
            pac_server: None,
            pac_update_rx: None,
        };
        (state, [app, profiles_bytes, runtime_bytes])
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        self.storage.save_app_config(&self.config)?;
        self.storage.save_profiles(&self.profiles)?;
        self.storage.save_runtime_state(&self.runtime)?;
        Ok(())
    }

    /// Persist only the runtime state, leaving edits the user has not saved
    /// yet in memory.  Used when the proxy is started or stopped.
    pub fn flush_runtime(&self) -> anyhow::Result<()> {
        self.storage.save_runtime_state(&self.runtime)
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

/// Load a config file; if it is unreadable or corrupt, move it aside as
/// `<name>.json.bad` and fall back to defaults instead of failing to start.
fn load_or_recover<T: Default>(path: &Path, load: impl FnOnce() -> anyhow::Result<T>) -> T {
    match load() {
        Ok(value) => value,
        Err(err) => {
            let backup = path.with_extension("json.bad");
            tracing::warn!(
                "{} is invalid ({err:#}); moving it to {} and using defaults",
                path.display(),
                backup.display()
            );
            let _ = std::fs::rename(path, &backup);
            T::default()
        }
    }
}

/// Restart or update the PAC server with fresh rules from disk.
///
/// If `force_restart` is `true` (e.g. the listen address changed), a new
/// server is started even if one already exists, and the old one is shut
/// down; if binding fails, a server on another address keeps running.  Otherwise the existing
/// server is updated in-place, or a new one is started if none exists.
pub fn restart_pac_server(state: &mut GuiState, force_restart: bool) -> anyhow::Result<()> {
    let (direct, proxy) = pac::load_rules(&state.storage.paths().config_dir);
    let content = pac::generate_pac(
        state.config.pac_rule_mode,
        &state.config.mixed_listen,
        &direct,
        &proxy,
    );
    if force_restart || state.pac_server.is_none() {
        let listen = &state.config.pac_listen;
        // The same address must be released before it can be bound again;
        // a different one is bound first so a failure keeps the old server.
        if state
            .pac_server
            .as_ref()
            .is_some_and(|server| server.listen() == listen)
        {
            if let Some(mut old) = state.pac_server.take() {
                old.shutdown();
            }
        }
        let server = pac::start(listen, content)?;
        if let Some(mut old) = state.pac_server.replace(server) {
            old.shutdown();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_file_is_moved_aside_and_defaults_used() {
        let dir =
            std::env::temp_dir().join(format!("juicity-gui-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.json");
        std::fs::write(&path, "{ not json").unwrap();

        let store: crate::config::ProfileStore =
            load_or_recover(&path, || anyhow::bail!("invalid json"));

        assert!(store.profiles.is_empty());
        assert!(!path.exists());
        assert!(dir.join("profiles.json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
