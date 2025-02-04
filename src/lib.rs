#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]

#[cfg(feature = "defmt")]
use defmt::*;

use embassy_futures::select::select;
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

// TODO: Create macro and to derive traits for each different service
// This will work for v0.0.1
#[gatt_server]
// Ideally this could be implemented by the user and include other services like battery etc.
// Until that can easily be done it will be implemented here, for easy event processing
pub struct LeAudioServer {
    pacs: pacs::PacsSink,
    // pacs: pacs::PacsSource,
}

pub async fn run_client<C: Controller, const L2CAP_MTU: usize>(
    client: &GattClient<'_, C, 10, L2CAP_MTU>,
) {
    select(client.task(), async {
        // pacs::sink_client(&client)
    })
    .await;
}

pub async fn run_server(server: &LeAudioServer<'_>, conn: &Connection<'_>) {
    loop {
        match conn.next().await {
            ConnectionEvent::Disconnected { reason } => {
                #[cfg(feature = "defmt")]
                info!("[gatt] disconnected: {:?}", reason);
                break;
            }
            ConnectionEvent::Gatt { data } => match data.process(&server).await {
                Ok(data) => {
                    #[cfg(feature = "defmt")]
                    info!("[gatt] Got event");

                    if let Some(event) = data {
                        if pacs::try_handle_event(&server.pacs, &event) {
                            #[cfg(feature = "defmt")]
                            info!("pacs handled this event");
                        } else {
                            #[cfg(feature = "defmt")]
                            warn!("There was no known handler to handle this event")
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
