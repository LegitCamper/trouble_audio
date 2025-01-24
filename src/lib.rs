#![cfg_attr(not(test), no_std, no_main)]
// #![warn(missing_docs)]

#[cfg(feature = "defmt")]
use defmt::*;

use trouble_host::{
    gap::CentralConfig,
    prelude::{
        appearance, gatt_server, AdStructure, Address, Advertisement, Central, ConnectConfig,
        Connection, ConnectionEvent, GapConfig, Peripheral, PeripheralConfig, ScanConfig,
        BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE,
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

#[derive(Clone, Copy)]
pub enum DeviceRole {
    Central(Address),
    Peripheral,
}

pub async fn create_run<'a, C: Controller>(
    role: DeviceRole,
    name: &'static str,
    central: &mut Central<'a, C>,
    peripheral: &mut Peripheral<'a, C>,
) {
    let server = create_server(name, role).await.unwrap();

    loop {
        let conn = match role {
            DeviceRole::Central(target) => {
                let config = ConnectConfig {
                    connect_params: Default::default(),
                    scan_config: ScanConfig {
                        filter_accept_list: &[(target.kind, &target.addr)],
                        ..Default::default()
                    },
                };
                central.connect(&config).await
            }
            DeviceRole::Peripheral => advertise(name, peripheral).await,
        };

        match conn {
            Ok(conn) => {
                // set up tasks when the connection is established, so they don't run when no one is connected.
                run_server(&server, &conn).await;
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

async fn advertise<'a, C: Controller>(
    name: &'a str,
    peripheral: &mut Peripheral<'a, C>,
) -> Result<Connection<'a>, BleHostError<C::Error>> {
    let mut advertiser_data = [0; 31];
    AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids16(&[pacs::PACS_UUID]),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut advertiser_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..],
                scan_data: &[],
            },
        )
        .await?;

    #[cfg(feature = "defmt")]
    info!("[adv] advertising");
    let conn = advertiser.accept().await?;
    #[cfg(feature = "defmt")]
    info!("[adv] connection established");
    Ok(conn)
}

async fn create_server(
    name: &'static str,
    role: DeviceRole,
) -> Result<LEAudioGattServer<'static>, &'static str> {
    #[cfg(feature = "defmt")]
    info!("Starting LE Audio GATT server");

    let config = match role {
        DeviceRole::Central(_) => GapConfig::Central(CentralConfig {
            name,
            appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
        }),
        DeviceRole::Peripheral => GapConfig::Peripheral(PeripheralConfig {
            name,
            appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
        }),
    };
    LEAudioGattServer::new_with_config(config)
}

async fn run_server(server: &LEAudioGattServer<'_>, conn: &Connection<'_>) {
    loop {
        match conn.next().await {
            ConnectionEvent::Disconnected { reason: _reason } => {
                #[cfg(feature = "defmt")]
                info!("[gatt] disconnected: {:?}", _reason);
                break;
            }
            ConnectionEvent::Gatt { data } => {
                // other services will follow
                pacs::pacs_gatt(server, data).await
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
