//! Debounced config persistence with own-write detection.

use crate::config::Storage;
use crate::state::GuiState;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Delay between the last change and the write.
pub const DEBOUNCE: Duration = Duration::from_millis(400);
/// Delay before retrying a failed write (e.g. a full disk).
pub const RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigFile {
    App,
    Profiles,
    Runtime,
}

impl ConfigFile {
    const ALL: [Self; 3] = [Self::App, Self::Profiles, Self::Runtime];

    fn index(self) -> usize {
        self as usize
    }

    fn path(self, storage: &Storage) -> PathBuf {
        let paths = storage.paths();
        match self {
            Self::App => paths.app_json.clone(),
            Self::Profiles => paths.profiles_json.clone(),
            Self::Runtime => paths.runtime_json.clone(),
        }
    }

    fn bytes(self, gui: &GuiState) -> anyhow::Result<Vec<u8>> {
        Ok(match self {
            Self::App => serde_json::to_vec_pretty(&gui.config)?,
            Self::Profiles => serde_json::to_vec_pretty(&gui.profiles)?,
            Self::Runtime => serde_json::to_vec_pretty(&gui.runtime)?,
        })
    }

    fn load(self, gui: &mut GuiState, bytes: &[u8]) -> anyhow::Result<()> {
        match self {
            Self::App => gui.config = serde_json::from_slice(bytes)?,
            Self::Profiles => gui.profiles = serde_json::from_slice(bytes)?,
            Self::Runtime => gui.runtime = serde_json::from_slice(bytes)?,
        }
        Ok(())
    }
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

pub struct Persist {
    dirty: [bool; 3],
    due: Option<Instant>,
    /// Hash of the bytes last written, read or skipped per file.
    known: [Option<u64>; 3],
}

impl Persist {
    /// Start from the exact bytes `GuiState` parsed, so a write that lands
    /// between loading and this call is still seen as external.
    pub fn new(loaded: &[Option<Vec<u8>>; 3]) -> Self {
        Self {
            dirty: [false; 3],
            due: None,
            known: loaded.each_ref().map(|b| b.as_deref().map(content_hash)),
        }
    }

    pub fn mark(&mut self, file: ConfigFile, now: Instant) {
        self.dirty[file.index()] = true;
        self.due = Some(now + DEBOUNCE);
    }

    #[cfg(test)]
    pub fn is_dirty(&self, file: ConfigFile) -> bool {
        self.dirty[file.index()]
    }

    /// Time left until the pending save; zero when it is due.
    pub fn delay(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Write every dirty file. Failed files stay dirty and a retry is
    /// scheduled after [`RETRY`]; the first error is returned after trying
    /// all of them.
    pub fn flush(&mut self, gui: &GuiState, now: Instant) -> anyhow::Result<()> {
        self.due = None;
        let mut result = Ok(());
        for file in ConfigFile::ALL {
            if !self.dirty[file.index()] {
                continue;
            }
            let written = file.bytes(gui).and_then(|bytes| {
                gui.storage
                    .write_atomic(&file.path(&gui.storage), &bytes)
                    .map(|()| content_hash(&bytes))
            });
            match written {
                Ok(hash) => {
                    self.dirty[file.index()] = false;
                    self.known[file.index()] = Some(hash);
                }
                Err(err) => {
                    self.due = Some(now + RETRY);
                    if result.is_ok() {
                        result = Err(err);
                    }
                }
            }
        }
        result
    }

    /// Reload clean files whose content changed on disk. Dirty files keep
    /// the local value and are written on the next flush.
    pub fn reload_changed(&mut self, gui: &mut GuiState, now: Instant) -> Vec<ConfigFile> {
        let mut reloaded = Vec::new();
        for file in ConfigFile::ALL {
            let Ok(bytes) = std::fs::read(file.path(&gui.storage)) else {
                continue;
            };
            let hash = content_hash(&bytes);
            if self.known[file.index()] == Some(hash) {
                continue;
            }
            self.known[file.index()] = Some(hash);
            if self.dirty[file.index()] {
                tracing::warn!(
                    "{file:?} config was edited externally; keeping unsaved local changes"
                );
                if self.due.is_none() {
                    self.due = Some(now + DEBOUNCE);
                }
                continue;
            }
            match file.load(gui, &bytes) {
                Ok(()) => reloaded.push(file),
                Err(err) => tracing::warn!("ignoring invalid external edit of {file:?}: {err:#}"),
            }
        }
        reloaded
    }
}
