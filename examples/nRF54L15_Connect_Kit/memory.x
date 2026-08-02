MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 1520K
  /* Reserved for `bond_store::RramBondStore` (see src/lib.rs) - one RRAMC-emulated page (4096
     bytes; `embassy_nrf::nvmc::Nvmc`'s `NorFlash::ERASE_SIZE`), holding a single persisted BLE
     bond so a peer doesn't need to re-pair after every reflash/restart. Never referenced by the
     linker script itself (no SECTIONS entry), only by its ORIGIN address below, read directly by
     `bond_store::BOND_STORAGE_OFFSET` - keep that constant in sync if this ever moves. */
  BOND_STORAGE : ORIGIN = 0x0017C000, LENGTH = 4K
  RAM : ORIGIN = 0x20000000, LENGTH = 256K
}
