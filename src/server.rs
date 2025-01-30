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

/// LE Audio GATT Server
#[cfg(feature = "server")]
#[gatt_server]
pub struct LEAudioGattServer {
    pacs: pacs::Pacs,
}

#[cfg(feature = "server")]
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
                            pacs::pacs_server(&server, data).await
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
