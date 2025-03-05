#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]
#![feature(generic_const_exprs)]

#[allow(dead_code)]
pub mod ascs;
mod server;
pub use server::*;
mod client;
pub use client::*;
// pub mod bap;
pub mod generic_audio;
pub mod pacs;

pub type ContentControlID = u8;

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CodecId(u64);

impl Default for CodecId {
    fn default() -> Self {
        Self(0x000000000D)
    }
}
