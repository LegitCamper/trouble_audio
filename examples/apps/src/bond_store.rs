//! A [`BondStore`] that persists via arbitrary byte-level storage, so each platform example only
//! plugs in *how* to read/write raw bytes, not encode/decode - see `examples/linux/src/bond_store.rs`
//! and `examples/nRF54L15_Connect_Kit/src/bond_store.rs` for the two current backends.
//!
//! Encodes with `postcard`, not JSON, so it also compiles for `no_std` embedded targets.

use alloc::vec::Vec;

use crate::sink::{BondInformation, BondStore};

#[cfg(feature = "defmt")]
use defmt::{warn, Debug2Format};

/// Comfortable headroom over a postcard-encoded [`BondInformation`] (well under half this even
/// with postcard's varint overhead). Bounds only the encoded payload - backends may reserve more
/// for their own framing (e.g. a length prefix).
const ENCODE_BUF_SIZE: usize = 96;

/// A [`BondStore`] that encodes/decodes with `postcard` and defers actual persistence to two
/// closures: `load_bytes` returns whatever `save_bytes` was last called with (or `None` if
/// nothing's been saved, or storage was never written), `save_bytes` receives the bytes to
/// persist. Neither closure needs to know anything about [`BondInformation`] itself.
pub struct EncodedBondStore<Load, Save>
where
    Load: Fn() -> Option<Vec<u8>>,
    Save: Fn(&[u8]),
{
    load_bytes: Load,
    save_bytes: Save,
}

impl<Load, Save> EncodedBondStore<Load, Save>
where
    Load: Fn() -> Option<Vec<u8>>,
    Save: Fn(&[u8]),
{
    pub fn new(load_bytes: Load, save_bytes: Save) -> Self {
        Self { load_bytes, save_bytes }
    }
}

impl<Load, Save> BondStore for EncodedBondStore<Load, Save>
where
    Load: Fn() -> Option<Vec<u8>>,
    Save: Fn(&[u8]),
{
    fn load(&self) -> Option<BondInformation> {
        let bytes = (self.load_bytes)()?;
        match postcard::from_bytes(&bytes) {
            Ok(bond) => Some(bond),
            Err(_e) => {
                #[cfg(feature = "log")]
                log::warn!("[bond_store] ignoring unreadable bond data: {_e:?}");
                #[cfg(feature = "defmt")]
                warn!("[bond_store] ignoring unreadable bond data: {}", Debug2Format(&_e));
                None
            }
        }
    }

    fn save(&self, bond: &BondInformation) {
        let mut buf = [0u8; ENCODE_BUF_SIZE];
        match postcard::to_slice(bond, &mut buf) {
            Ok(encoded) => (self.save_bytes)(encoded),
            Err(_e) => {
                #[cfg(feature = "log")]
                log::warn!("[bond_store] failed to encode bond, not persisting: {_e:?}");
                #[cfg(feature = "defmt")]
                warn!("[bond_store] failed to encode bond, not persisting: {}", Debug2Format(&_e));
            }
        }
    }
}
