//! Persists a single bond to a file, so reconnecting after restarting this process doesn't fail
//! authentication (see `trouble_audio_example_apps::sink::BondStore`). Actual encode/decode is
//! shared with every other platform example via
//! `trouble_audio_example_apps::bond_store::EncodedBondStore` - this only supplies how to read
//! and write raw bytes on a real filesystem.

use std::path::{Path, PathBuf};

use trouble_audio_example_apps::bond_store::EncodedBondStore;
pub use trouble_audio_example_apps::sink::{BondInformation, BondStore};

/// A [`BondStore`] backed by a file at `path`.
pub fn file_bond_store(path: impl Into<PathBuf>) -> EncodedBondStore<impl Fn() -> Option<Vec<u8>>, impl Fn(&[u8])> {
    let path = path.into();
    let load_path = path.clone();
    EncodedBondStore::new(
        move || std::fs::read(&load_path).ok(),
        move |bytes: &[u8]| match std::fs::write(&path, bytes) {
            Ok(()) => log::info!("[bond_store] saved bond to {}", path.display()),
            Err(e) => log::warn!("[bond_store] failed to write {}: {e}", path.display()),
        },
    )
}

/// The default sink-role bond store: one file under this platform's state directory.
pub fn sink_bond_store() -> EncodedBondStore<impl Fn() -> Option<Vec<u8>>, impl Fn(&[u8])> {
    file_bond_store(state_dir().join("trouble_audio_bond"))
}

/// The default source-role bond store - a separate file from the sink's, so both can coexist on
/// the same machine without clobbering each other's bonds.
pub fn source_bond_store() -> EncodedBondStore<impl Fn() -> Option<Vec<u8>>, impl Fn(&[u8])> {
    file_bond_store(state_dir().join("trouble_audio_source_bond"))
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
}
