#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]

#[cfg(feature = "defmt")]
use defmt::*;

use trouble_host::{
    gap::CentralConfig,
    prelude::{
        appearance, gatt_server, AdStructure, AddrKind, Address, Advertisement, BdAddr, Central,
        ConnectConfig, Connection, ConnectionEvent, GapConfig, Peripheral, PeripheralConfig,
        ScanConfig, BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE,
    },
    BleHostError, Controller,
};

// #[allow(dead_code)]
// pub mod ascs;
// pub mod bap;
#[allow(dead_code)]
pub mod generic_audio;
#[allow(dead_code)]
pub mod pacs;

pub type ContentControlID = u8;

/// LE Audio GATT Server
#[gatt_server]
pub struct LEAudioGattServer {
    pacs: pacs::PublishedAudioCapabilitiesService,
}

pub async fn run_server<'a, C: Controller>(
    name: &'static str,
    central: &mut Central<'a, C>,
    target_list: &'a [(AddrKind, &'a BdAddr)],
) {
    #[cfg(feature = "defmt")]
    info!("Starting LE Audio GATT server");

    let server = LEAudioGattServer::new_with_config(GapConfig::Central(CentralConfig {
        name,
        appearance: &appearance::audio_source::GENERIC_AUDIO_SOURCE,
    }))
    .unwrap();

    loop {
        let config = ConnectConfig {
            connect_params: Default::default(),
            scan_config: ScanConfig {
                filter_accept_list: target_list,
                ..Default::default()
            },
        };
        #[cfg(feature = "defmt")]
        info!("Trying to connect to: {:?}", target_list);
        let conn = central.connect(&config).await;

        match conn {
            Ok(conn) => {
                #[cfg(feature = "defmt")]
                info!("Found peripheral(s), Starting gatt server");

                loop {
                    match conn.next().await {
                        ConnectionEvent::Disconnected { reason: _reason } => {
                            #[cfg(feature = "defmt")]
                            info!("[gatt] disconnected: {:?}", _reason);
                            break;
                        }
                        ConnectionEvent::Gatt { data } => {
                            // other services will follow
                            pacs::pacs_gatt(&server, data).await
                        }
                    }
                }
                #[cfg(feature = "defmt")]
                info!("[gatt] task finished");
            }
            Err(_e) => {
                #[cfg(feature = "defmt")]
                let err = defmt::Debug2Format(&_e);
                #[cfg(feature = "defmt")]
                defmt::panic!("[adv] error: {:?}", err);
            }
        }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug)]
#[allow(dead_code)]
pub struct CodecdId(u64);

impl Default for CodecdId {
    fn default() -> Self {
        Self(0x000000000D)
    }
}
