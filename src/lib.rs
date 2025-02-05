#![feature(generic_const_exprs)]
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
        appearance, gatt_server, AdStructure, AddrKind, Address, Advertisement, AttErrorCode,
        AttributeServer, AttributeTable, BdAddr, Central, ConnectConfig, Connection,
        ConnectionEvent, GapConfig, Peripheral, PeripheralConfig, ScanConfig, Service,
        BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE,
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

const MAX_SERVICES: usize = 3;

pub async fn run_server<'a, const ATT_MTU: usize, M: RawMutex>(
    conn: &Connection<'_>,
    name_id: &str,
    appearance: &'a [u8; 2],
    storage: &'a mut [u8; MAX_SERVICES * ATT_MTU],
) {
    #[cfg(feature = "defmt")]
    info!("[gatt] Starting LE Audio Server");

    let mut table: AttributeTable<'_, M, MAX_SERVICES> = AttributeTable::new();
    let mut svc = table.add_service(Service::new(0x1800u16));
    let _ = svc.add_characteristic_ro(0x2a00u16, &name_id);
    let _ = svc.add_characteristic_ro(0x2a01u16, appearance);
    svc.build();

    // Generic attribute service (mandatory)
    table.add_service(Service::new(0x1801u16));

    let mut storages = storage.chunks_exact_mut(ATT_MTU);
    let pacs = pacs::PacsServer::<ATT_MTU>::new(
        &mut table,
        None,
        None,
        None,
        None,
        (Default::default(), storages.next().unwrap()),
        (Default::default(), storages.next().unwrap()),
    );

    let server = AttributeServer::<M, MAX_SERVICES>::new(table);

    loop {
        match conn.next().await {
            ConnectionEvent::Disconnected { reason } => {
                #[cfg(feature = "defmt")]
                info!("[gatt] disconnected: {:?}", reason);
                break;
            }
            ConnectionEvent::Gatt { data } => match data.process(&server).await {
                Ok(data) => {
                    if let Some(event) = data {
                        if let Some(resp) = pacs.handle(&event) {
                            if let Err(err) = resp {
                                event.reject(err).unwrap().send().await
                            } else {
                                event.accept().unwrap().send().await
                            };
                        } else {
                            #[cfg(feature = "defmt")]
                            warn!("[gatt] There was no known handler to handle this event");
                            event
                                .reject(AttErrorCode::INVALID_HANDLE)
                                .unwrap()
                                .send()
                                .await;
                        }
                    }
                }
                Err(e) => {
                    #[cfg(feature = "defmt")]
                    warn!("[gatt] error processing event: {:?}", e);
                }
            },
        }
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
