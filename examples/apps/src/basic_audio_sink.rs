use bt_hci::AsHciBytes;
#[cfg(feature = "defmt")]
use defmt::{Debug2Format, error, info};

use embassy_futures::{join::join, select::select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use heapless::Vec;
use static_cell::StaticCell;
use trouble_audio::{
    MAX_SERVICES,
    ascs::{Ase, AseType},
    generic_audio::AudioLocation,
    pacs::{AudioContexts, PAC, PACRecord},
};
use trouble_host::prelude::*;

/// Max number of connections
const CONNECTIONS_MAX: usize = 1;

/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC

pub async fn run<C, const L2CAP_MTU: usize>(mut controller: C) -> !
where
    C: Controller,
{
    // Using a fixed "random" address can be useful for testing. In real scenarios, one would
    // use e.g. the MAC 6 byte array as the address (how to get that varies by the platform).
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);
    #[cfg(feature = "defmt")]
    info!("Our address = {:?}", address);

    let mut resources: HostResources<CONNECTIONS_MAX, L2CAP_CHANNELS_MAX, L2CAP_MTU> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        mut peripheral,
        mut runner,
        ..
    } = stack.build();

    // NOTE: Modify this to match the address of the peripheral you want to connect to.
    // Currently, it matches the address used by the peripheral examples
    let target: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);

    let config = ConnectConfig {
        connect_params: Default::default(),
        scan_config: ScanConfig {
            filter_accept_list: &[(target.kind, &target.addr)],
            ..Default::default()
        },
    };

    let sink_pac = PAC::default();
    let sink_audio_locations = AudioLocation::all();
    static sink_audio_locations_store: StaticCell<[u8; 90]> = StaticCell::new();
    let supported_audio_contexts = AudioContexts::default();
    let available_audio_contexts = AudioContexts::default();

    loop {
        select(runner.run(), async {
            loop {
                let mut ases = Vec::new();
                ases.push(AseType::Sink(Ase::new(0)));

                match advertise::<C>("Ble Audio Sink", &mut peripheral).await {
                    Ok(conn) => {
                        #[cfg(feature = "defmt")]
                        info!("[adv] connection established");
                        let server =
                            trouble_audio::ServerBuilder::<L2CAP_MTU, 1, 1, NoopRawMutex>::new(
                                b"Ble Audio Sink Example",
                                &appearance::audio_sink::GENERIC_AUDIO_SINK,
                            )
                            .add_pacs(
                                Some(&sink_pac),
                                Some((
                                    &sink_audio_locations,
                                    sink_audio_locations_store.init([0; 90]),
                                )),
                                None,
                                None,
                                &supported_audio_contexts,
                                &available_audio_contexts,
                            )
                            .add_ascs(ases)
                            .build();
                        loop {
                            match conn.next().await {
                                ConnectionEvent::Disconnected { reason: _reason } => {
                                    #[cfg(feature = "defmt")]
                                    info!("[gatt] disconnected: {:?}", _reason);
                                    break;
                                }
                                ConnectionEvent::Gatt { data } => server.process(data).await,
                            }
                        }
                    }
                    Err(e) => {
                        #[cfg(feature = "defmt")]
                        let e = Debug2Format(&e);
                        #[cfg(feature = "defmt")]
                        error!("[adv] error: {:?}", e);
                    }
                }
            }
        })
        .await;
        #[cfg(feature = "defmt")]
        info!("Exiting Bluetooth");
    }
}

/// Create an advertiser
async fn advertise<'a, C: Controller>(
    name: &'a str,
    peripheral: &mut Peripheral<'a, C>,
) -> Result<Connection<'a>, BleHostError<C::Error>> {
    let mut advertiser_data = [0; 31];
    AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids16(&[
                service::PUBLISHED_AUDIO_CAPABILITIES.into(),
                service::AUDIO_STREAM_CONTROL.into(),
            ]),
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
