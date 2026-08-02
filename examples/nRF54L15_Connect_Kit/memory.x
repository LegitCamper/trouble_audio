MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 1520K
  /* Reserved for `bond_store::rram_bond_store`'s BOND_STORAGE_OFFSET - keep in sync if this moves. */
  BOND_STORAGE : ORIGIN = 0x0017C000, LENGTH = 4K
  RAM : ORIGIN = 0x20000000, LENGTH = 256K
}
