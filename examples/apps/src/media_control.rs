//! A minimal BLE peripheral exposing the (Generic) Media Control Service (MCS) - a pure remote
//! control. This device neither sends nor receives any audio itself (no PACS/ASCS/CIS at all,
//! deliberately - e.g. the rp-pico-w's cyw43439 Bluetooth controller doesn't support LE
//! Isochronous Channels (BIS/CIS) and so couldn't stream real LE Audio even if it wanted to);
//! whatever's actually playing audio lives entirely on some other device. A bonded central
//! controls this device's single (simulated) media player over the standard Media Control Point;
//! [`PLAYING`] mirrors the resulting Play/Pause state for whatever local UI the caller wants to
//! drive with it (e.g. an LED). [`BUTTON_PRESSED`] is the mirror image - signal it from a local
//! input (e.g. a physical button) to drive the same Play/Pause toggle from this device's side,
//! kept in sync with any connected central the same way a Media Control Point write would be.
//!
//! Unlike [`crate::sink`], this owns its own advertise/accept/event loop rather than reusing
//! `run_peripheral` - that helper's controller bounds and advertising data are both LE
//! Audio/CIS-specific, neither of which apply here.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::signal::Signal;
use heapless::String as HString;
use trouble_audio::mcs::{
    self, MediaControlPointOpcodesSupported, MediaState, PlayingOrder, PlayingOrdersSupported,
};
use trouble_audio::prelude::*;
use trouble_host::prelude::*;

use crate::sink::BondStore;

#[cfg(feature = "defmt")]
use defmt::{Debug2Format, info, warn};

/// Max number of connections. This example services one central at a time.
const CONNECTIONS_MAX: usize = 1;
/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC
/// This server declares no ASEs at all - MCS doesn't need them.
const MAX_ASES: usize = 0;

/// This device's fixed "random" address.
pub const ADDRESS: [u8; 6] = [0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xfd];

const DEVICE_NAME: &[u8] = b"Ble Media Control";

/// Set (by [`run`]) whenever the media player is left in [`MediaState::Playing`] - by a connected
/// central's Media Control Point write, or by [`BUTTON_PRESSED`] - cleared on
/// [`MediaState::Paused`]/`Inactive`/disconnect. This device has no audio of its own; the only
/// thing actually playing is on whatever other device this remote is controlling.
pub static PLAYING: AtomicBool = AtomicBool::new(false);

/// Signal this (e.g. `.signal(())`) from a local input to toggle Play/Pause from this device's own
/// side - [`run`] reacts the same way whether a central is connected or not, only notifying it in
/// the former case (see the `rp-pico-w` example's `button` module for a BOOTSEL-driven caller).
pub static BUTTON_PRESSED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Backing storage for [`run`]'s GATT table - declared separately so its borrows can outlive the
/// `Server` built from it (see `apps::sink::PeripheralConfigStorage` for the same pattern).
/// The per-characteristic buffers come from the library's own storage types.
struct Storage {
    audio_contexts: AudioContexts,
    playing_orders_supported: PlayingOrdersSupported,
    opcodes_supported: MediaControlPointOpcodesSupported,
    pacs: trouble_audio::pacs::PacsStorage,
    mcs: trouble_audio::mcs::McsStorage,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // This device has no LE Audio streaming capability at all - PACS is only present
            // because `ServerBuilder` requires it.
            audio_contexts: AudioContexts {
                sink_contexts: ContextType::empty(),
                source_contexts: ContextType::empty(),
            },
            playing_orders_supported: PlayingOrdersSupported::InOrderOnce,
            opcodes_supported: MediaControlPointOpcodesSupported::Play
                | MediaControlPointOpcodesSupported::Pause,
            pacs: Default::default(),
            mcs: Default::default(),
        }
    }
}

fn build_server(storage: &mut Storage) -> Server<'_, MAX_ASES, CONNECTIONS_MAX, NoopRawMutex> {
    ServerBuilder::<MAX_ASES, CONNECTIONS_MAX, NoopRawMutex>::new(
        DEVICE_NAME,
        &appearance::human_interface_device::GENERIC_HUMAN_INTERFACE_DEVICE,
    )
    .add_pacs(
        None,
        None,
        None,
        None,
        &storage.audio_contexts,
        &storage.audio_contexts,
        &mut storage.pacs,
    )
    .add_mcs(
        mcs::McsInit {
            media_player_name: HString::try_from("Trouble Media Control").unwrap(),
            // Placeholder track info: this device has no real player behind it, and a real
            // remote would learn these from whatever it's actually controlling.
            track_title: HString::try_from("Unknown Track").unwrap(),
            track_duration: -1, // Unknown.
            playback_speed: 0,
            playing_order: PlayingOrder::InOrderOnce,
            playing_orders_supported: storage.playing_orders_supported,
            media_control_point_opcodes_supported: storage.opcodes_supported,
            content_control_id: 1,
        },
        &storage.playing_orders_supported,
        &storage.opcodes_supported,
        &mut storage.mcs,
    )
    .build()
}

/// Toggles the media player between `Playing`/`Paused` (a local input has no reason to touch
/// `Inactive`) and updates [`PLAYING`] to match. Notifies `conn` of the change if given - `None`
/// while no central is connected yet, since there's nobody to tell.
async fn toggle_play_pause(
    server: &Server<'_, MAX_ASES, CONNECTIONS_MAX, NoopRawMutex>,
    conn: Option<&GattConnection<'_, '_, DefaultPacketPool>>,
) {
    let Some(mcs) = server.mcs() else { return };
    let current = mcs.media_state().get(&server.server).unwrap_or_default();
    let next = if current == MediaState::Playing { MediaState::Paused } else { MediaState::Playing };
    let _ = mcs.media_state().set(&server.server, &next);
    if let Some(conn) = conn {
        let _ = mcs.media_state().notify(conn, &next, true).await;
    }
    PLAYING.store(next == MediaState::Playing, Ordering::Release);
}

/// Runs the media control peripheral forever on the given controller: advertises, accepts one
/// bonded connection at a time, and drives the Media Control Point (via `Server::handle`),
/// updating [`PLAYING`] as the media state changes. Never returns.
///
/// Unlike the LE Audio sink/source examples, `controller` needs no LE Isochronous Channels support
/// at all - this is a plain GATT peripheral.
///
/// `bond_store`, if given, persists bonds across restarts of this process (see [`BondStore`]).
pub async fn run<C: Controller>(controller: C, bond_store: Option<&dyn BondStore>) -> ! {
    let address: Address = Address::random(ADDRESS);
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        // No display/keyboard on a typical remote: JustWorks pairing (encrypted, no MITM
        // protection).
        .set_io_capabilities(IoCapabilities::NoInputNoOutput)
        .build();
    if let Some(store) = bond_store {
        if let Some(bond) = store.load() {
            let _ = stack.add_bond_information(bond);
        }
    }
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let mut advertiser_data = [0; 31];
    let adv_data_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::IncompleteServiceUuids16(&[service::GENERIC_MEDIA_CONTROL.to_le_bytes()]),
            AdStructure::CompleteLocalName(DEVICE_NAME),
        ],
        &mut advertiser_data[..],
    )
    .expect("static advertising data always fits in 31 bytes");
    let advertiser_data = &advertiser_data[..adv_data_len];

    let mut storage = Storage::default();
    let server = build_server(&mut storage);
    // A player with nothing loaded is `Inactive`, and Play/Pause are rejected while inactive -
    // this simulated remote always has a track "loaded", just not playing yet.
    let _ = server
        .mcs()
        .unwrap()
        .media_state()
        .set(&server.server, &MediaState::Paused);

    select(
        async {
            loop {
                if let Err(_e) = runner.run().await {
                    #[cfg(feature = "log")]
                    log::warn!("[media_control] host runner error: {:?}", _e);
                    #[cfg(feature = "defmt")]
                    warn!("[media_control] host runner error: {}", Debug2Format(&_e));
                }
            }
        },
        async {
            loop {
                let conn = loop {
                    match select(advertise(advertiser_data, &mut peripheral, &server), BUTTON_PRESSED.wait()).await {
                        Either::First(Ok(conn)) => break conn,
                        Either::First(Err(_e)) => {
                            #[cfg(feature = "log")]
                            log::warn!("[media_control] advertise error: {:?}", _e);
                            #[cfg(feature = "defmt")]
                            warn!("[media_control] advertise error: {}", Debug2Format(&_e));
                        }
                        // Not connected yet, so nobody to notify - just flip the local state and
                        // keep (re)advertising.
                        Either::Second(()) => toggle_play_pause(&server, None).await,
                    }
                };
                #[cfg(feature = "log")]
                log::info!("[media_control] connected");
                #[cfg(feature = "defmt")]
                info!("[media_control] connected");
                let _ = conn.raw().set_bondable(true);
                loop {
                    let event = match select(conn.next(), BUTTON_PRESSED.wait()).await {
                        Either::First(event) => event,
                        Either::Second(()) => {
                            toggle_play_pause(&server, Some(&conn)).await;
                            continue;
                        }
                    };
                    match event {
                        GattConnectionEvent::Disconnected { reason: _reason } => {
                            #[cfg(feature = "log")]
                            log::info!("[media_control] disconnected: {:?}", _reason);
                            #[cfg(feature = "defmt")]
                            info!("[media_control] disconnected: {}", Debug2Format(&_reason));
                            break;
                        }
                        GattConnectionEvent::Gatt { event } => {
                            server.handle(&conn, event).await;
                            let is_playing = server
                                .mcs()
                                .and_then(|mcs| mcs.media_state().get(&server.server).ok())
                                .is_some_and(|state| state == MediaState::Playing);
                            PLAYING.store(is_playing, Ordering::Release);
                        }
                        GattConnectionEvent::PairingComplete { security_level: _sl, bond: Some(bond) } => {
                            #[cfg(feature = "log")]
                            log::info!("[media_control] pairing complete, security_level={:?}", _sl);
                            #[cfg(feature = "defmt")]
                            info!("[media_control] pairing complete, security_level={}", Debug2Format(&_sl));
                            let _ = stack.add_bond_information(bond.clone());
                            if let Some(store) = bond_store {
                                store.save(&bond);
                            }
                        }
                        GattConnectionEvent::PairingComplete { security_level: _sl, bond: None } => {
                            #[cfg(feature = "log")]
                            log::warn!("[media_control] pairing complete but NOT bonded (security_level={:?})", _sl);
                            #[cfg(feature = "defmt")]
                            warn!("[media_control] pairing complete but NOT bonded");
                        }
                        GattConnectionEvent::PairingFailed(_e) => {
                            #[cfg(feature = "log")]
                            log::warn!("[media_control] pairing failed: {:?}", _e);
                            #[cfg(feature = "defmt")]
                            warn!("[media_control] pairing failed");
                        }
                        _ => {}
                    }
                }
                // A disconnect implicitly pauses playback - nothing is left to drive Play/Pause.
                PLAYING.store(false, Ordering::Release);
            }
        },
    )
    .await;

    unreachable!("both branches above loop forever")
}

async fn advertise<'values, 'server, C: Controller>(
    adv_data: &[u8],
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values, MAX_ASES, CONNECTIONS_MAX, NoopRawMutex>,
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
    log::info!("[media_control] advertising");
    #[cfg(feature = "defmt")]
    info!("[media_control] advertising");
    Ok(advertiser
        .accept()
        .await?
        .with_attribute_server(&server.server)?)
}
