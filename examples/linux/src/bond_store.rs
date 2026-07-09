//! Persists a single bond to a JSON file, so reconnecting after restarting this process doesn't
//! fail authentication (see `trouble_audio_example_apps::sink::BondStore`).

use std::path::{Path, PathBuf};

use trouble_audio_example_apps::sink::{BondInformation, BondStore};

pub struct FileBondStore {
    path: PathBuf,
}

impl FileBondStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl BondStore for FileBondStore {
    fn load(&self) -> Option<BondInformation> {
        let data = std::fs::read(&self.path).ok()?;
        match serde_json::from_slice(&data) {
            Ok(bond) => Some(bond),
            Err(e) => {
                log::warn!("[bond_store] ignoring unreadable bond file {}: {e}", self.path.display());
                None
            }
        }
    }

    fn save(&self, bond: &BondInformation) {
        match serde_json::to_vec_pretty(bond) {
            Ok(data) => match std::fs::write(&self.path, data) {
                Ok(()) => log::info!("[bond_store] saved bond to {}", self.path.display()),
                Err(e) => log::warn!("[bond_store] failed to write {}: {e}", self.path.display()),
            },
            Err(e) => log::warn!("[bond_store] failed to serialize bond: {e}"),
        }
    }
}

impl Default for FileBondStore {
    fn default() -> Self {
        Self::new(default_path())
    }
}

fn default_path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("trouble_audio_bond.json")
}
