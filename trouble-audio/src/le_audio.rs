//! A ready-made construction + event loop for the common case: one physical device acting as an
//! LE Audio unicast sink/source peripheral (GATT *server* for PACS and ASCS). Pick your PACS
//! capabilities and ASE endpoints with [`PeripheralConfig`], then hand it to [`run_peripheral`] -
//! it owns advertising, accepting the connection, and running [`Server::handle`] (which itself
//! drives the ASE Control Point state machine) for you.
//!
//! The complementary GATT *client* role (an initiator/hub discovering and controlling a remote
//! peripheral's PACS/ASCS) is set up with [`LeAudioClient::discover`] - lighter-weight, since
//! what to *do* with the discovered characteristics is inherently application logic that can't be
//! automated away the way the peripheral's event loop can.

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{info, warn, Debug2Format};

use crate::{
    ascs::{AscsClient, AseType},
    generic_audio::AudioLocation,
    pacs::{AudioContexts, PacsClient, PAC},
    Server, ServerBuilder,
};

/// Everything needed to build the GATT side of an LE Audio unicast sink/source peripheral.
///
/// `sink_*`/`source_*` mirror the optional PACS characteristics: enable whichever direction(s)
/// this device supports (a sink-only device, e.g. earbuds, only needs `sink_*`).
pub struct PeripheralConfig<'a> {
    pub device_name: &'a [u8],
    pub appearance: BluetoothUuid16,
    pub sink_pac: Option<PAC>,
    pub sink_audio_locations: Option<AudioLocation>,
    pub source_pac: Option<PAC>,
    pub source_audio_locations: Option<AudioLocation>,
    pub supported_audio_contexts: AudioContexts,
    pub available_audio_contexts: AudioContexts,
}

/// Runs an LE Audio unicast peripheral forever: advertises, accepts one connection at a time,
/// and services GATT reads/writes (including driving the ASE Control Point state machine) until
/// disconnected, then advertises again.
///
/// `MAX_ASES` bounds the number of ASE endpoints `ases` describes; `CONNECTIONS_MAX` is the
/// number of simultaneous BLE connections the underlying stack allows (this crate's `AscsServer`
/// itself only tracks one connection's ASE state at a time - see its docs).
pub async fn run_peripheral<
    C: Controller,
    M: RawMutex,
    const MAX_ASES: usize,
    const CONNECTIONS_MAX: usize,
    const L2CAP_CHANNELS_MAX: usize,
>(
    controller: C,
    address: Address,
    io_capabilities: IoCapabilities,
    config: PeripheralConfig<'_>,
    ases: Vec<AseType, MAX_ASES>,
) -> ! {
    let mut resources: HostResources<C, DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .set_io_capabilities(io_capabilities)
        .build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let mut sink_audio_locations_store = [0u8; 90];
    let mut source_audio_locations_store = [0u8; 90];
    let PeripheralConfig {
        device_name,
        appearance,
        sink_pac,
        sink_audio_locations,
        source_pac,
        source_audio_locations,
        supported_audio_contexts,
        available_audio_contexts,
    } = config;

    // Built once and reused across reconnects, matching `Peripheral`'s own lifetime - rebuilding
    // the GATT table on every connection isn't necessary and doesn't lifetime-check cleanly
    // against a long-lived `Peripheral` anyway.
    let server = ServerBuilder::<MAX_ASES, CONNECTIONS_MAX, M>::new(device_name, &appearance)
        .add_pacs(
            sink_pac.as_ref(),
            sink_audio_locations
                .as_ref()
                .map(|loc| (loc, &mut sink_audio_locations_store[..])),
            source_pac.as_ref(),
            source_audio_locations
                .as_ref()
                .map(|loc| (loc, &mut source_audio_locations_store[..])),
            &supported_audio_contexts,
            &available_audio_contexts,
        )
        .add_ascs(ases)
        .build();

    select(
        async {
            loop {
                if let Err(_e) = runner.run().await {
                    #[cfg(feature = "log")]
                    log::warn!("[le_audio] host runner error: {:?}", _e);
                    #[cfg(feature = "defmt")]
                    warn!("[le_audio] host runner error: {}", Debug2Format(&_e));
                }
            }
        },
        async {
            loop {
                match advertise(device_name, &mut peripheral, &server).await {
                    Ok(conn) => {
                        #[cfg(feature = "log")]
                        log::info!("[le_audio] connected");
                        #[cfg(feature = "defmt")]
                        info!("[le_audio] connected");
                        loop {
                            match conn.next().await {
                                GattConnectionEvent::Disconnected { reason: _reason } => {
                                    #[cfg(feature = "log")]
                                    log::info!("[le_audio] disconnected: {:?}", _reason);
                                    #[cfg(feature = "defmt")]
                                    info!("[le_audio] disconnected: {}", Debug2Format(&_reason));
                                    break;
                                }
                                GattConnectionEvent::Gatt { event } => {
                                    server.handle(&conn, event).await;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_e) => {
                        #[cfg(feature = "log")]
                        log::warn!("[le_audio] advertise error: {:?}", _e);
                        #[cfg(feature = "defmt")]
                        warn!("[le_audio] advertise error: {}", Debug2Format(&_e));
                        continue;
                    }
                }
            }
        },
    )
    .await;

    unreachable!("both branches above loop forever")
}

async fn advertise<
    'values,
    'server,
    C: Controller,
    const MAX_ASES: usize,
    const CONNECTIONS_MAX: usize,
    M: RawMutex,
>(
    name: &'values [u8],
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values, MAX_ASES, CONNECTIONS_MAX, M>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut advertiser_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::IncompleteServiceUuids16(&[
                service::PUBLISHED_AUDIO_CAPABILITIES.to_le_bytes(),
                service::AUDIO_STREAM_CONTROL.to_le_bytes(),
            ]),
            AdStructure::CompleteLocalName(name),
        ],
        &mut advertiser_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    #[cfg(feature = "log")]
    log::info!("[le_audio] advertising");
    #[cfg(feature = "defmt")]
    info!("[le_audio] advertising");
    let conn = advertiser
        .accept()
        .await?
        .with_attribute_server(&server.server)?;
    Ok(conn)
}

/// The discovered GATT client side of an LE Audio unicast peripheral: PACS to read its
/// capabilities, ASCS to control its ASEs. Bundles the two discovery calls that would otherwise
/// need repeating by hand; driving actual audio session logic (config codec, enable, etc.) from
/// here is up to the caller, since it depends on what the application is trying to do.
pub struct LeAudioClient {
    pub pacs: PacsClient,
    pub ascs: AscsClient,
}

impl LeAudioClient {
    /// Discovers PACS and ASCS on an already-connected `GattClient`. The returned client's
    /// background task (`client.task()`) must still be run concurrently by the caller (e.g. via
    /// `embassy_futures::select` alongside whatever uses `pacs`/`ascs`) for GATT requests to
    /// complete at all.
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Self {
        let pacs = PacsClient::new(client).await;
        let ascs = AscsClient::new(client).await;
        Self { pacs, ascs }
    }
}
