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

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_futures::select::{select, select4, Either};
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::prelude::*;
pub use trouble_host::prelude::BondInformation;

#[cfg(feature = "defmt")]
use defmt::{info, warn, Debug2Format};

use crate::{
    ascs::{AscsClient, AseType},
    cis::{self, CisManager},
    generic_audio::AudioLocation,
    iso::{LeAcceptCisRequest, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetupIsoDataPath},
    pacs::{AudioContexts, PacsClient, PAC},
    Server, ServerBuilder,
};

/// LE feature bit for "Connected Isochronous Stream (Host Support)" (Core 6, Vol 6, Part B,
/// Section 4.6) - set via `HCI_LE_Set_Host_Feature` to opt in to using CIS. Without it, a peer's
/// link-layer feature exchange sees CIS as unsupported and never attempts `LE_Create_CIS`.
const CIS_HOST_SUPPORT_FEATURE_BIT: u8 = 32;

/// Persists bond information across process restarts.
///
/// This crate's `AscsServer`/`run_peripheral` model a single active connection, so a single
/// stored bond (the most recent) is enough. Without this, a peer that bonds once and then
/// reconnects after this process restarts will have its reconnection fail authentication: the
/// peer still remembers the old Long Term Key, but a fresh, empty security manager doesn't.
pub trait BondStore {
    /// Called once at startup to seed the security manager with a previously saved bond, if any.
    fn load(&self) -> Option<BondInformation>;
    /// Called whenever a new bond is created (see `GattConnectionEvent::PairingComplete`).
    fn save(&self, bond: &BondInformation);
}

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
/// and services GATT reads/writes (including driving the ASE Control Point state machine and,
/// via `cis_manager`, real CIS/ISO setup) until disconnected, then advertises again.
///
/// `MAX_ASES` bounds the number of ASE endpoints `ases` describes; `CONNECTIONS_MAX` is the
/// number of simultaneous BLE connections the underlying stack allows (this crate's `AscsServer`
/// itself only tracks one connection's ASE state at a time - see its docs).
///
/// `cis_manager` is caller-owned (rather than built internally) so the caller can concurrently
/// drain [`CisManager::receive_pcm`] for decoded audio - this function never returns.
///
/// `bond_store`, if given, is used to persist bonds across restarts of this process (see
/// [`BondStore`]) - pass `None` if you don't need reconnects to survive a restart.
pub async fn run_peripheral<
    C: Controller
        + ControllerCmdAsync<LeAcceptCisRequest>
        + ControllerCmdSync<LeRejectCisRequest>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>,
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
    cis_manager: &CisManager<M, MAX_ASES>,
    bond_store: Option<&dyn BondStore>,
) -> ! {
    let mut resources: HostResources<C, DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .set_io_capabilities(io_capabilities)
        .build();
    if let Some(store) = bond_store {
        if let Some(bond) = store.load() {
            let _ = stack.add_bond_information(bond);
        }
    }
    let runner = stack.runner();
    let peripheral = stack.peripheral();

    let mut sink_pac_store = [0u8; 90];
    let mut source_pac_store = [0u8; 90];
    let mut sink_audio_locations_store = [0u8; 90];
    let mut source_audio_locations_store = [0u8; 90];
    let mut available_audio_contexts_store = [0u8; 90];
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
            sink_pac.as_ref().map(|pac| (pac, &mut sink_pac_store[..])),
            sink_audio_locations
                .as_ref()
                .map(|loc| (loc, &mut sink_audio_locations_store[..])),
            source_pac.as_ref().map(|pac| (pac, &mut source_pac_store[..])),
            source_audio_locations
                .as_ref()
                .map(|loc| (loc, &mut source_audio_locations_store[..])),
            &supported_audio_contexts,
            &available_audio_contexts,
            &mut available_audio_contexts_store[..],
        )
        .add_ascs(ases)
        .add_cis_manager(cis_manager)
        .build();

    run_event_loop(&stack, runner, peripheral, cis_manager, &server, device_name, bond_store).await
}

/// The forever event loop shared by [`run_peripheral`] and [`crate::scenario::Scenario::run`]:
/// drives the BLE host's RX runner (with `cis_manager` as the `EventHandler`), the CIS/ISO HCI
/// command side (`cis::drive_cis`), the one-shot CIS host-feature opt-in, and the advertise/
/// accept/dispatch loop against `server`. Generic over an already-built [`Server`] - it doesn't
/// care which optional GATT services that `Server` has, only that PACS/ASCS (mandatory for any
/// LE Audio peripheral) are present and `cis_manager` matches its ASE Control Point driving.
pub(crate) async fn run_event_loop<
    'values,
    C: Controller
        + ControllerCmdAsync<LeAcceptCisRequest>
        + ControllerCmdSync<LeRejectCisRequest>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>,
    M: RawMutex,
    const MAX_ASES: usize,
    const CONNECTIONS_MAX: usize,
>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    mut runner: Runner<'_, C, DefaultPacketPool>,
    mut peripheral: Peripheral<'values, C, DefaultPacketPool>,
    cis_manager: &CisManager<M, MAX_ASES>,
    server: &Server<'values, MAX_ASES, CONNECTIONS_MAX, M>,
    device_name: &'values [u8],
    bond_store: Option<&dyn BondStore>,
) -> ! {
    select4(
        async {
            loop {
                if let Err(_e) = runner.run_with_handler(cis_manager).await {
                    #[cfg(feature = "log")]
                    log::warn!("[le_audio] host runner error: {:?}", _e);
                    #[cfg(feature = "defmt")]
                    warn!("[le_audio] host runner error: {}", Debug2Format(&_e));
                }
            }
        },
        cis::drive_cis(stack, cis_manager),
        async {
            // Its own branch rather than sitting in front of the advertise loop below: doing these
            // HCI round trips there previously delayed `peripheral.advertise()` long enough to
            // lose a race against a startup resolving-list sync, which then got rejected with
            // "Command Disallowed" for running after advertising had already started.
            #[cfg(any(feature = "log", feature = "defmt"))]
            if let Ok(_features) = stack.command(LeReadLocalSupportedFeatures::new()).await {
                #[cfg(feature = "log")]
                log::info!(
                    "[le_audio] controller CIS support: peripheral={} central={}",
                    _features.supports_connected_isochronous_stream_peripheral(),
                    _features.supports_connected_isochronous_stream_central()
                );
                #[cfg(feature = "defmt")]
                info!(
                    "[le_audio] controller CIS support: peripheral={} central={}",
                    _features.supports_connected_isochronous_stream_peripheral(),
                    _features.supports_connected_isochronous_stream_central()
                );
            }
            match stack
                .command(LeSetHostFeature::new(CIS_HOST_SUPPORT_FEATURE_BIT, 1))
                .await
            {
                Ok(_) => {
                    #[cfg(feature = "log")]
                    log::info!("[le_audio] enabled Isochronous Channels (Host Support)");
                    #[cfg(feature = "defmt")]
                    info!("[le_audio] enabled Isochronous Channels (Host Support)");
                }
                Err(_e) => {
                    #[cfg(feature = "log")]
                    log::warn!("[le_audio] LE Set Host Feature (CIS) failed: {:?}", _e);
                    #[cfg(feature = "defmt")]
                    warn!("[le_audio] LE Set Host Feature (CIS) failed");
                }
            }
            core::future::pending::<()>().await
        },
        async {
            loop {
                match advertise(device_name, &mut peripheral, server).await {
                    Ok(conn) => {
                        #[cfg(feature = "log")]
                        log::info!("[le_audio] connected");
                        #[cfg(feature = "defmt")]
                        info!("[le_audio] connected");
                        // A connection is not bondable by default, and must be marked as such
                        // before pairing starts - otherwise `PairingComplete` always reports
                        // `bond: None` (a temporary key only), even if the peer requests
                        // bonding, and `bond_store`/the resolving-list update below never run.
                        let _ = conn.raw().set_bondable(true);
                        loop {
                            let event = match select(conn.next(), cis_manager.next_streaming_ase()).await {
                                Either::First(event) => event,
                                Either::Second(ase_id) => {
                                    server.notify_ase_streaming(&conn, ase_id).await;
                                    continue;
                                }
                            };
                            match event {
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
                                GattConnectionEvent::PairingComplete { security_level: _sl, bond: Some(bond) } => {
                                    #[cfg(feature = "log")]
                                    log::info!(
                                        "[le_audio] pairing complete, security_level={:?}, bond identity={:?}",
                                        _sl, bond.identity
                                    );
                                    #[cfg(feature = "defmt")]
                                    info!("[le_audio] pairing complete, security_level={}", Debug2Format(&_sl));
                                    // The pairing flow itself already stores `bond` for LTK
                                    // lookup, but only `Stack::add_bond_information` also queues
                                    // the controller's resolving list to be updated with the
                                    // peer's IRK - without it, a peer using a rotating private
                                    // address (the common case) can pair successfully but then
                                    // never be recognized on its next reconnect, since the
                                    // controller has no way to resolve the new address back to
                                    // this bond.
                                    let _ = stack.add_bond_information(bond.clone());
                                    if let Some(store) = bond_store {
                                        store.save(&bond);
                                    }
                                }
                                GattConnectionEvent::PairingComplete { security_level: _sl, bond: None } => {
                                    #[cfg(feature = "log")]
                                    log::warn!(
                                        "[le_audio] pairing complete but NOT bonded (security_level={:?}) - reconnects will need to re-pair",
                                        _sl
                                    );
                                    #[cfg(feature = "defmt")]
                                    warn!("[le_audio] pairing complete but NOT bonded");
                                }
                                GattConnectionEvent::PairingFailed(_e) => {
                                    #[cfg(feature = "log")]
                                    log::warn!("[le_audio] pairing failed: {:?}", _e);
                                    #[cfg(feature = "defmt")]
                                    warn!("[le_audio] pairing failed");
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
                service::COMMON_AUDIO.to_le_bytes(),
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
