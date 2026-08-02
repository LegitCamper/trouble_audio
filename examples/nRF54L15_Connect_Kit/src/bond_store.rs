//! Wires `trouble_audio_example_apps::bond_store::EncodedBondStore` to this board's on-chip
//! flash/RRAM (`memory.x`'s reserved `BOND_STORAGE` region) - see that module for why the actual
//! encode/decode/logging logic lives there rather than here, shared with every other platform
//! example. This only supplies how to read and write raw bytes on RRAM. Shared between
//! `bin/sink.rs` and `bin/source.rs`.

use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_nrf::nvmc::Nvmc;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use trouble_audio_example_apps::bond_store::EncodedBondStore;

/// Offset of `memory.x`'s `BOND_STORAGE` region into flash - keep in sync if that region ever
/// moves (nothing here can catch a drift automatically, since the two files aren't cross-checked
/// by anything at build time).
const BOND_STORAGE_OFFSET: u32 = 0x0017C000;

/// Fixed record slot size, `Nvmc::WRITE_SIZE`(16)-aligned and comfortably larger than
/// `trouble_audio_example_apps::bond_store`'s own 96-byte encode buffer plus this module's 2-byte
/// length prefix - the two numbers aren't otherwise related, this just needs to be `>=` it.
const RECORD_SIZE: usize = 112;

/// Builds a [`trouble_audio_example_apps::sink::BondStore`] backed by this board's RRAM, so a
/// peer that's already bonded doesn't need to re-pair after every reflash/restart. `flash` must
/// outlive the returned value - construct it once in `main` and keep it alive for the rest of the
/// program, same as `cis_manager`/`led`.
///
/// Only one bond is ever kept, matching `EncodedBondStore`/`BondStore`'s own model (this crate's
/// `AscsServer`/`run_peripheral` track a single active connection) - a new peer pairing simply
/// overwrites whatever was here, so there's nothing to do if a different phone pairs later than
/// the one that's currently stored: the old key is just gone, no explicit "forget" step needed.
pub fn rram_bond_store<'a>(flash: &'a RefCell<Nvmc<'_>>) -> EncodedBondStore<impl Fn() -> Option<Vec<u8>> + 'a, impl Fn(&[u8]) + 'a> {
    EncodedBondStore::new(
        move || {
            let mut buf = [0u8; RECORD_SIZE];
            if flash.borrow_mut().read(BOND_STORAGE_OFFSET, &mut buf).is_err() {
                defmt::warn!("[bond_store] failed to read bond storage");
                return None;
            }

            // Erased RRAM/flash reads back as all-0xFF; an all-0xFF (or otherwise nonsensical)
            // length means nothing has ever been saved here.
            let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
            if len == 0 || len > RECORD_SIZE - 2 {
                return None;
            }
            Some(buf[2..2 + len].to_vec())
        },
        move |encoded: &[u8]| {
            if encoded.len() > RECORD_SIZE - 2 {
                defmt::warn!("[bond_store] encoded bond ({} bytes) doesn't fit the record slot, not persisting", encoded.len());
                return;
            }
            let mut buf = [0u8; RECORD_SIZE];
            buf[0..2].copy_from_slice(&(encoded.len() as u16).to_le_bytes());
            buf[2..2 + encoded.len()].copy_from_slice(encoded);

            let mut flash = flash.borrow_mut();
            // RRAM can be rewritten in place, but `NorFlash`'s contract still requires erasing
            // (resetting to 0xFF) before a write can set any bit back from 0 to 1 - e.g. a
            // shorter new record following a longer old one would otherwise leave stale trailing
            // bytes from the previous save.
            if flash
                .erase(BOND_STORAGE_OFFSET, BOND_STORAGE_OFFSET + embassy_nrf::nvmc::PAGE_SIZE as u32)
                .is_err()
            {
                defmt::warn!("[bond_store] failed to erase bond storage page");
                return;
            }
            match flash.write(BOND_STORAGE_OFFSET, &buf) {
                Ok(()) => defmt::info!("[bond_store] saved bond"),
                Err(_e) => defmt::warn!("[bond_store] failed to write bond record"),
            }
        },
    )
}
