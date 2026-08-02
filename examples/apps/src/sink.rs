//! A ready-made construction + event loop for the common case: one physical device acting as an
//! LE Audio unicast sink peripheral (GATT *server* for PACS and ASCS). Pick your PACS
//! capabilities and ASE endpoints with [`PeripheralConfig`], then hand it to [`run_peripheral`] -
//! it owns advertising, accepting the connection, and running `Server::handle` (which itself
//! drives the ASE Control Point state machine) for you.
//!
//! This lives here rather than in `trouble_audio` itself since it's opinionated glue code that
//! only really proves out the sink role - `PeripheralConfig` has `source_*` fields, but nothing
//! actually exercises a real audio *source* (mic) end to end yet. The GATT *client* discovery
//! helper for the complementary hub/central role (`trouble_audio::LeAudioClient`) is role-agnostic
//! and stays in the core crate.

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_futures::select::{Either, select, select4};
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_audio::cis;
use trouble_audio::prelude::*;
pub use trouble_host::prelude::BondInformation;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{Debug2Format, info, warn};

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
/// this device supports (a sink-only device, e.g. earbuds, only needs `sink_*`). `Clone` so a
/// caller can build one `PeripheralConfig` and reuse it every time [`run_peripheral`] needs to be
/// (re)started, rather than reconstructing the whole thing by hand each time.
#[derive(Clone)]
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

/// Backing storage for [`PeripheralConfig`]'s variable-length characteristics (PAC records, audio
/// locations, available audio contexts). Kept separate from `PeripheralConfig` itself since it's
/// pure scratch space with no meaningful value of its own - declare one alongside your config and
/// pass both to [`PeripheralConfig::build_server`].
pub struct PeripheralConfigStorage {
    sink_pac: [u8; 90],
    source_pac: [u8; 90],
    sink_audio_locations: [u8; 90],
    source_audio_locations: [u8; 90],
    available_audio_contexts: [u8; 90],
}

impl Default for PeripheralConfigStorage {
    fn default() -> Self {
        Self {
            sink_pac: [0; 90],
            source_pac: [0; 90],
            sink_audio_locations: [0; 90],
            source_audio_locations: [0; 90],
            available_audio_contexts: [0; 90],
        }
    }
}

impl<'a> PeripheralConfig<'a> {
    /// Builds the GATT server this config describes. `self` and `storage` are borrowed for as
    /// long as the returned `Server` is used, so both need to live at least that long - in
    /// practice that means declaring them right next to each other and keeping them both around
    /// for the life of the peripheral (see [`run_peripheral`]).
    pub fn build_server<'s, const MAX_ASES: usize, const CONNECTIONS_MAX: usize, M: RawMutex>(
        &'s self,
        storage: &'s mut PeripheralConfigStorage,
        ases: Vec<AseType, MAX_ASES>,
        cis_manager: &'s CisManager<M, MAX_ASES>,
    ) -> Server<'s, MAX_ASES, CONNECTIONS_MAX, M> {
        ServerBuilder::<MAX_ASES, CONNECTIONS_MAX, M>::new(self.device_name, &self.appearance)
            .add_pacs(
                self.sink_pac
                    .as_ref()
                    .map(|pac| (pac, &mut storage.sink_pac[..])),
                self.sink_audio_locations
                    .as_ref()
                    .map(|loc| (loc, &mut storage.sink_audio_locations[..])),
                self.source_pac
                    .as_ref()
                    .map(|pac| (pac, &mut storage.source_pac[..])),
                self.source_audio_locations
                    .as_ref()
                    .map(|loc| (loc, &mut storage.source_audio_locations[..])),
                &self.supported_audio_contexts,
                &self.available_audio_contexts,
                &mut storage.available_audio_contexts[..],
            )
            .add_ascs(ases)
            .add_cis_manager(cis_manager)
            .build()
    }
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
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
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
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    // Computed once and reused across reconnects, same as `server` below - the advertising
    // payload (name + service UUIDs) never changes for the life of this call, so there's no
    // reason to re-encode it on every single reconnect.
    let mut advertiser_data = [0; 31];
    let adv_data_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::IncompleteServiceUuids16(&[
                service::PUBLISHED_AUDIO_CAPABILITIES.to_le_bytes(),
                service::AUDIO_STREAM_CONTROL.to_le_bytes(),
                service::COMMON_AUDIO.to_le_bytes(),
            ]),
            AdStructure::CompleteLocalName(config.device_name),
        ],
        &mut advertiser_data[..],
    )
    .expect("static advertising data always fits in 31 bytes");
    let advertiser_data = &advertiser_data[..adv_data_len];

    // Built once and reused across reconnects, matching `Peripheral`'s own lifetime - rebuilding
    // the GATT table on every connection isn't necessary and doesn't lifetime-check cleanly
    // against a long-lived `Peripheral` anyway.
    let mut storage = PeripheralConfigStorage::default();
    let server =
        config.build_server::<MAX_ASES, CONNECTIONS_MAX, M>(&mut storage, ases, cis_manager);

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
        cis::drive_cis(&stack, cis_manager),
        async {
            cis::enable_cis_host_support(&stack).await;
            core::future::pending::<()>().await
        },
        async {
            loop {
                match advertise(advertiser_data, &mut peripheral, &server).await {
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
    adv_data: &[u8],
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values, MAX_ASES, CONNECTIONS_MAX, M>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data,
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
