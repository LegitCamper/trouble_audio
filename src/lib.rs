#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]

#[cfg(feature = "defmt")]
use defmt::*;

use trouble_host::{
    gap::CentralConfig,
    gatt::GattClient,
    prelude::{
        appearance, gatt_server, AdStructure, AddrKind, Address, Advertisement, BdAddr, Central,
        ConnectConfig, Connection, ConnectionEvent, GapConfig, Peripheral, PeripheralConfig,
        ScanConfig, BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE,
    },
    BleHostError, Controller,
};

#[allow(dead_code)]
pub mod ascs;
pub mod client;
pub mod server;
// pub mod bap;
#[allow(dead_code)]
pub mod generic_audio;
#[allow(dead_code)]
pub mod pacs;

pub type ContentControlID = u8;

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug)]
#[allow(dead_code)]
pub struct CodecId(u64);

impl Default for CodecId {
    fn default() -> Self {
        Self(0x000000000D)
    }
}
