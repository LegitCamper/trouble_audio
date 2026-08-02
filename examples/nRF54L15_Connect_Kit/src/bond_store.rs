//! Wires `trouble_audio_example_apps::bond_store::EncodedBondStore` to this board's on-chip
//! flash/RRAM (`memory.x`'s `BOND_STORAGE` region) - just the raw-bytes read/write, encode/decode
//! lives in that shared module. Used by both `bin/sink.rs` and `bin/source.rs`.

use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_nrf::nvmc::Nvmc;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use trouble_audio_example_apps::bond_store::EncodedBondStore;

/// Offset of `memory.x`'s `BOND_STORAGE` region - keep in sync if that region ever moves.
const BOND_STORAGE_OFFSET: u32 = 0x0017C000;

/// Record slot size, `Nvmc::WRITE_SIZE`(16)-aligned and comfortably bigger than a 2-byte length
/// prefix plus `EncodedBondStore`'s 96-byte encode buffer.
const RECORD_SIZE: usize = 112;

/// Builds a [`trouble_audio_example_apps::sink::BondStore`] backed by this board's RRAM, so a
/// peer that's already bonded doesn't need to re-pair after every reflash. `flash` must outlive
/// the returned value - construct it once in `main`, same as `cis_manager`/`led`.
///
/// Only one bond is kept - a new peer pairing just overwrites the slot.
pub fn rram_bond_store<'a>(flash: &'a RefCell<Nvmc<'_>>) -> EncodedBondStore<impl Fn() -> Option<Vec<u8>> + 'a, impl Fn(&[u8]) + 'a> {
    EncodedBondStore::new(
        move || {
            let mut buf = [0u8; RECORD_SIZE];
            if flash.borrow_mut().read(BOND_STORAGE_OFFSET, &mut buf).is_err() {
                defmt::warn!("[bond_store] failed to read bond storage");
                return None;
            }

            // Erased RRAM reads back as all-0xFF, so a nonsensical length means nothing's saved.
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
            // `NorFlash` requires erasing before a write can set a bit back from 0 to 1.
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
