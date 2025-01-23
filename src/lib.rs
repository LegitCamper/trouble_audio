#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]

// #[allow(dead_code)]
// pub mod ascs;
// pub mod bap;
#[allow(dead_code)]
pub mod generic_audio;
#[allow(dead_code)]
pub mod pacs;

pub type ContentControlID = u8;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct CodecdId(u64);

impl Default for CodecdId {
    fn default() -> Self {
        Self(0x000000000D)
    }
}
