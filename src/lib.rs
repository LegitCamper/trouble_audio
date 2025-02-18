#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]
#[cfg(feature = "defmt")]
use defmt::*;

use embassy_futures::select::select;
use trouble_host::{
    gatt::{GattClient, GattData, GattEvent, ReadEvent, WriteEvent},
    prelude::{AttErrorCode, AttributeServer, AttributeTable, GattValue},
    Controller,
};

#[allow(dead_code)]
pub mod ascs;
mod server;
pub use server::*;
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

pub async fn run_client<C: Controller, const L2CAP_MTU: usize>(
    client: &GattClient<'_, C, 10, L2CAP_MTU>,
) {
    select(client.task(), async {
        // pacs::sink_client(&client)
    })
    .await;
}
