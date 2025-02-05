#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]

#[cfg(feature = "defmt")]
use defmt::*;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::{NoopRawMutex, RawMutex};
use heapless::Vec;
use trouble_host::{
    gap::CentralConfig,
    gatt::{GattClient, GattEvent},
    prelude::{
        appearance, gatt_server, AdStructure, AddrKind, Address, Advertisement, AttributeServer,
        AttributeTable, BdAddr, Central, ConnectConfig, Connection, ConnectionEvent, GapConfig,
        Peripheral, PeripheralConfig, ScanConfig, Service, BR_EDR_NOT_SUPPORTED,
        LE_GENERAL_DISCOVERABLE,
    },
    BleHostError, Controller,
};

#[allow(dead_code)]
pub mod ascs;
// pub mod bap;
#[allow(dead_code)]
pub mod generic_audio;
#[allow(dead_code)]
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

const MAX: usize = 3;

pub fn create_server<'a, const ATT_MTU: usize, M: RawMutex>(
    name_id: &'a [u8; 2],
    appearance: &'a [u8; 2],
) -> AttributeServer<'a, M, MAX> {
    let mut table: AttributeTable<'_, M, MAX> = AttributeTable::new();
    let mut svc = table.add_service(Service::new(0x1800u16));
    let _ = svc.add_characteristic_ro(0x2a00u16, name_id);
    let _ = svc.add_characteristic_ro(0x2a01u16, appearance);
    svc.build();

    // Generic attribute service (mandatory)
    table.add_service(Service::new(0x1801u16));

    let mut s1: Vec<u8, 200> = Vec::new();
    let mut s2: Vec<u8, 200> = Vec::new();
    pacs::PacsServer::<ATT_MTU>::new(
        &mut table,
        None,
        None,
        None,
        None,
        (Default::default(), s1.as_mut_slice()),
        (Default::default(), s2.as_mut_slice()),
    );

    AttributeServer::<M, MAX>::new(table)
}

// // TODO: Create macro and to derive traits for each different service
// // This will work for v0.0.1
// #[gatt_server]
// // Ideally this could be implemented by the user and include other services like battery etc.
// // Until that can easily be done it will be implemented here, for easy event processing
// pub struct LeAudioServer {
//     // pub pacs: pacs::PACS,
//     // pub cats: CATS<5>,
//     // pacs: pacs::PacsSource,
// }

// pub async fn run_client<C: Controller, const L2CAP_MTU: usize>(
//     client: &GattClient<'_, C, 10, L2CAP_MTU>,
// ) {
//     select(client.task(), async {
//         // pacs::sink_client(&client)
//     })
//     .await;
// }

// pub async fn run_server(server: &LeAudioServer<'_>, conn: &Connection<'_>) {
//     loop {
//         match conn.next().await {
//             ConnectionEvent::Disconnected { reason } => {
//                 #[cfg(feature = "defmt")]
//                 info!("[gatt] disconnected: {:?}", reason);
//                 break;
//             }
//             ConnectionEvent::Gatt { data } => match data.process(&server).await {
//                 Ok(data) => {
//                     if let Some(event) = data {
//                         if let Some(resp) = pacs::try_handle_event(&server.pacs, &event) {
//                             if let Err(err) = resp {
//                                 event.reject(err).unwrap()
//                             } else {
//                                 event.accept().unwrap()
//                             };
//                         } else {
//                             #[cfg(feature = "defmt")]
//                             warn!("There was no known handler to handle this event")
//                         }
//                     }
//                 }
//                 Err(e) => {
//                     #[cfg(feature = "defmt")]
//                     warn!("[gatt] error processing event: {:?}", e);
//                 }
//             },
//         }
//     }
// }
