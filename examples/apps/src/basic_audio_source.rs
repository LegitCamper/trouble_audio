//! A minimal LE Audio unicast source central: connects to the stereo sink this session's
//! [`crate::basic_audio_sink`] describes, negotiates both its Sink ASEs, and streams a generated
//! test tone down to it over CIS/ISO. The mirror image of `basic_audio_sink`/[`crate::sink`]
//! (peripheral, GATT server) - this is a GATT client instead, using `trouble_audio::{ase_client,
//! cig}` (the central-side ASE Control Point driver and CIG/CIS establishment manager) rather than
//! `AscsServer`/`CisManager`.

use alloc::vec::Vec as AVec;

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_futures::select::{select, select4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use heapless::Vec as HVec;
use trouble_audio::ascs::{Ase, AscsClient, ConfigQosEntry, TargetLatency};
use trouble_audio::cig::{self, AseQos, CigManager};
use trouble_audio::generic_audio::{
    encode_list, AudioLocation, CodecSpecificConfiguration, ContextType, FrameDuration, Metadata, SamplingFrequency,
};
use trouble_audio::iso::{LeCreateCis, LeRemoveCig, LeRemoveIsoDataPath, LeSetCigParameters, LeSetupIsoDataPath};
use trouble_audio::{ase_client, cis, iso_tx, CodecId};
use trouble_host::prelude::*;

use crate::sink::BondStore;

#[cfg(feature = "defmt")]
use defmt::{info, warn, Debug2Format};

/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC
/// Max number of connections.
const CONNECTIONS_MAX: usize = 1;
/// Max number of GATT services this client caches per connection.
const MAX_SERVICES: usize = 20;

/// Sampling frequency/frame duration/octets to request - matches the only PAC record
/// `basic_audio_sink` advertises support for.
const SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz48000;
const FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;
const OCTETS_PER_FRAME: u16 = 100;
/// 48000 Hz * 10 ms.
const SAMPLES_PER_FRAME: usize = 480;

const CIG_ID: u8 = 1;
/// 10000us (10ms), matching [`FRAME_DURATION`] - SDU interval and codec frame duration must match
/// for unframed PDUs.
const SDU_INTERVAL: [u8; 3] = [0x10, 0x27, 0x00];
const MAX_TRANSPORT_LATENCY_MS: u16 = 20;
/// 40000us (40ms) - a typical LC3 presentation delay.
const PRESENTATION_DELAY: [u8; 3] = [0x40, 0x9c, 0x00];
const RETRANSMISSION_NUMBER: u8 = 2;

/// Runs the audio source central forever on the given controller: connects to `target` (a
/// "random" address, e.g. [`crate::basic_audio_sink::ADDRESS`]), then streams a generated stereo
/// test tone to it. Never returns.
///
/// `bond_store`, if given, persists the bond so that reconnects after a process restart don't need
/// to re-pair (the sink remembers the LTK; without a stored bond on the source side, it won't know
/// to resume encryption rather than pair fresh, which can fail if the peer enforces re-pairing
/// prevention).
pub async fn run<C>(controller: C, our_address: [u8; 6], target: [u8; 6], bond_store: Option<&dyn BondStore>) -> !
where
    C: Controller
        + ControllerCmdSync<LeSetCigParameters>
        + ControllerCmdAsync<LeCreateCis>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeRemoveCig>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>,
{
    let target = Address::random(target);
    let mut resources: HostResources<C, DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(Address::random(our_address))
        // Match the sink's NoInputNoOutput → JustWorks pairing on both sides.
        .set_io_capabilities(IoCapabilities::NoInputNoOutput)
        .build();
    // Seed the security manager with any previously saved bond so that a reconnect after a
    // process restart can resume encryption rather than pair from scratch.
    if let Some(bond) = bond_store.and_then(|s| s.load()) {
        let _ = stack.add_bond_information(bond);
    }
    let mut runner = stack.runner();
    let mut central = stack.central();
    let cig_manager = CigManager::<NoopRawMutex>::new();

    select4(
        async {
            loop {
                if let Err(_e) = runner.run_with_handler(&cig_manager).await {
                    #[cfg(feature = "log")]
                    log::warn!("[le_audio_source] host runner error: {:?}", _e);
                    #[cfg(feature = "defmt")]
                    warn!("[le_audio_source] host runner error: {}", Debug2Format(&_e));
                }
            }
        },
        cig::drive_cig(&stack, &cig_manager),
        // `enable_cis_host_support`'s `LE Set Host Feature` command can lose a race against the
        // controller's startup resolving-list sync ("Command Disallowed") - that sync only runs
        // once the connection state machine goes idle, so retry a few times with a settling delay
        // rather than giving up after one attempt. It doesn't report success/failure, so this
        // just retries unconditionally a bounded number of times.
        async {
            for _ in 0..5 {
                if cis::enable_cis_host_support(&stack).await {
                    break;
                }
                embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
            }
            core::future::pending::<()>().await
        },
        async {
            // Give the controller a moment to finish its own startup housekeeping (the same
            // resolving-list sync above) before the very first attempt, then a real delay between
            // retries - hammering `connect()` in a tight loop never lets that sync run (it only
            // fires once the connection state machine has been idle for a moment), which is what
            // was producing a self-perpetuating stream of instant `Command Disallowed`/`Timeout`
            // failures.
            embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;
            loop {
                connect_and_stream(&stack, &mut central, target, &cig_manager, bond_store).await;
                embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
            }
        },
    )
    .await;

    unreachable!("all branches above loop forever")
}

async fn connect_and_stream<C: Controller>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    central: &mut Central<'_, C, DefaultPacketPool>,
    target: Address,
    cig_manager: &CigManager<NoopRawMutex>,
    bond_store: Option<&dyn BondStore>,
) {
    // Clear stale state from the previous connection.  This also queues a `LeRemoveCig` action
    // for `drive_cig` to execute before the next `LE Set CIG Parameters` so the controller
    // doesn't reject the re-create with `Command Disallowed (0x0C)`.
    cig_manager.reset();

    let config = ConnectConfig {
        connect_params: Default::default(),
        scan_config: ScanConfig {
            filter_accept_list: &[target],
            ..Default::default()
        },
    };

    // `Central::connect` has no timeout of its own - it waits forever for a matching
    // advertisement, indistinguishable from a real hang without one of our own. Retrying in a
    // bounded loop instead gives a "still waiting" heartbeat, so silence past the first one means
    // the peer genuinely isn't reachable (not advertising, wrong address, RF issue, ...) rather
    // than this code being stuck.
    let conn = loop {
        #[cfg(feature = "log")]
        log::info!("[le_audio_source] connecting to {:?}", target);
        #[cfg(feature = "defmt")]
        info!("[le_audio_source] connecting");
        match embassy_time::with_timeout(embassy_time::Duration::from_secs(10), central.connect(&config)).await {
            Ok(Ok(conn)) => break conn,
            Ok(Err(_e)) => {
                #[cfg(feature = "log")]
                log::warn!("[le_audio_source] connect failed: {:?}", _e);
                #[cfg(feature = "defmt")]
                warn!("[le_audio_source] connect failed");
                return;
            }
            Err(_timeout) => {
                #[cfg(feature = "log")]
                log::warn!("[le_audio_source] still waiting for the sink to be reachable...");
                #[cfg(feature = "defmt")]
                warn!("[le_audio_source] still waiting for the sink to be reachable...");
            }
        }
    };

    // The sink's ASE/ASE Control Point characteristics require encryption (Common Audio Profile),
    // so pairing must complete before any of them can be read or written.
    let _ = conn.set_bondable(true);
    if conn.request_security().is_err() {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] request_security failed");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] request_security failed");
        return;
    }

    // Wait for the link to become encrypted, handling bond events along the way.
    //
    // For fresh pairings (LESC):    PairingComplete fires first (with bond), then Encrypted.
    // For bonded reconnects:        only Encrypted fires (with the stored bond).
    // For legacy pairing:           Encrypted fires first (STK, bond=None), then PairingComplete.
    //
    // We always break on Encrypted.  For the legacy-pairing case PairingComplete may arrive
    // after we've already broken out; in practice both sides use LESC (NoInputNoOutput +
    // modern controllers), so PairingComplete reliably precedes Encrypted.
    let mut pending_bond: Option<BondInformation> = None;
    loop {
        match conn.next().await {
            ConnectionEvent::PairingComplete { bond: Some(bond), .. } => {
                // Keep the bond info; we'll persist it once Encrypted confirms the link is up.
                let _ = stack.add_bond_information(bond.clone());
                pending_bond = Some(bond);
            }
            ConnectionEvent::Encrypted { bond: Some(bond), .. } => {
                // Bonded reconnect: encryption resumed with a stored LTK.
                let _ = stack.add_bond_information(bond.clone());
                if let Some(store) = bond_store {
                    store.save(&bond);
                }
                break;
            }
            ConnectionEvent::Encrypted { bond: None, .. } => {
                // Fresh pairing: bond info already captured in PairingComplete above (LESC), or
                // will arrive in a subsequent PairingComplete (Legacy - saved on next reconnect).
                if let Some(bond) = pending_bond.take() {
                    if let Some(store) = bond_store {
                        store.save(&bond);
                    }
                }
                break;
            }
            ConnectionEvent::Disconnected { .. } => return,
            _ => {}
        }
    }
    #[cfg(feature = "log")]
    log::info!("[le_audio_source] connected and encrypted");
    #[cfg(feature = "defmt")]
    info!("[le_audio_source] connected and encrypted");
    cig_manager.set_acl_handle(conn.handle());

    let client = match GattClient::<C, DefaultPacketPool, MAX_SERVICES>::new(stack, &conn).await {
        Ok(client) => client,
        Err(_e) => {
            #[cfg(feature = "log")]
            log::warn!("[le_audio_source] GATT client setup failed: {:?}", _e);
            #[cfg(feature = "defmt")]
            warn!("[le_audio_source] GATT client setup failed");
            return;
        }
    };

    // Only ASCS is needed here (no PACS-driven capability negotiation, no optional services), so
    // `AscsClient::new` is used directly rather than the heavier `LeAudioClient::discover`.
    select(client.task(), async {
        let ascs = AscsClient::new(&client).await;
        stream_tone(stack, &client, &ascs, cig_manager).await;
    })
    .await;
}

/// Discovers both Sink ASEs' real ASE_IDs, negotiates them (Config Codec -> Config QoS -> queues
/// CIG/CIS creation -> Enable), then plays a generated tone once both are streaming.
async fn stream_tone<C: Controller, const N: usize>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    client: &GattClient<'_, C, DefaultPacketPool, N>,
    ascs: &AscsClient,
    cig_manager: &CigManager<NoopRawMutex>,
) {
    let mut ase_ids = [0u8; 2];
    for (i, characteristic) in ascs.sink_ases().take(2).enumerate() {
        let mut buf = [0u8; 90];
        let Ok(n) = client.read_characteristic(characteristic, &mut buf).await else {
            return;
        };
        let Ok(ase) = Ase::from_gatt(&buf[..n]) else { return };
        ase_ids[i] = ase.id();
    }

    let codec_config = |location: AudioLocation| {
        encode_list(&[
            CodecSpecificConfiguration::SamplingFrequency(SAMPLING_FREQUENCY),
            CodecSpecificConfiguration::FrameDuration(FRAME_DURATION),
            CodecSpecificConfiguration::OctetsPerCodecFrame(OCTETS_PER_FRAME),
            CodecSpecificConfiguration::AudioChannelAllocation(location),
        ])
    };
    let config_codec_entries: AVec<_> = [
        (ase_ids[0], TargetLatency::Lower, PhySet::M1, CodecId::default(), codec_config(AudioLocation::FrontLeft)),
        (ase_ids[1], TargetLatency::Lower, PhySet::M1, CodecId::default(), codec_config(AudioLocation::FrontRight)),
    ]
    .into();
    if ase_client::config_codec(client, ascs, config_codec_entries).await.is_err() {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] Config Codec failed");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] Config Codec failed");
        return;
    }

    // We (the central) choose CIG_ID/CIS_ID/QoS - reused for both this Config QoS write and CIG
    // creation below, via `AseQos`, so they can't drift apart.
    let qos = |cis_id: u8| AseQos {
        cig_id: CIG_ID,
        cis_id,
        sdu_interval: SDU_INTERVAL,
        framing: 0,
        phy: PhySet::M1,
        max_sdu: OCTETS_PER_FRAME,
        retransmission_number: RETRANSMISSION_NUMBER,
        max_transport_latency: MAX_TRANSPORT_LATENCY_MS,
        presentation_delay: PRESENTATION_DELAY,
    };
    let (qos_0, qos_1) = (qos(0), qos(1));
    let config_qos_entries: AVec<ConfigQosEntry> = [
        (
            ase_ids[0],
            qos_0.cig_id,
            qos_0.cis_id,
            qos_0.sdu_interval,
            qos_0.framing,
            qos_0.phy,
            qos_0.max_sdu,
            qos_0.retransmission_number,
            qos_0.max_transport_latency,
            qos_0.presentation_delay,
        ),
        (
            ase_ids[1],
            qos_1.cig_id,
            qos_1.cis_id,
            qos_1.sdu_interval,
            qos_1.framing,
            qos_1.phy,
            qos_1.max_sdu,
            qos_1.retransmission_number,
            qos_1.max_transport_latency,
            qos_1.presentation_delay,
        ),
    ]
    .into();
    if ase_client::config_qos(client, ascs, config_qos_entries).await.is_err() {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] Config QoS failed");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] Config QoS failed");
        return;
    }

    // Both ASEs are QoS-configured - `cig_manager` now has everything it needs to create the CIG
    // and both CIS (carried out by `cig::drive_cig`, running concurrently - see `run`).
    cig_manager.configure(ase_ids[0], qos_0, SAMPLING_FREQUENCY, FRAME_DURATION);
    cig_manager.configure(ase_ids[1], qos_1, SAMPLING_FREQUENCY, FRAME_DURATION);

    let metadata = encode_list(&[Metadata::StreamingAudioContexts(ContextType::Media)]);
    let enable_entries: AVec<_> = [(ase_ids[0], metadata.clone()), (ase_ids[1], metadata)].into();
    if ase_client::enable(client, ascs, enable_entries).await.is_err() {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] Enable failed");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] Enable failed");
        return;
    }

    let mut ready = [false; 2];
    while !(ready[0] && ready[1]) {
        let ase_id = cig_manager.next_ready_ase().await;
        for (slot, &target) in ready.iter_mut().zip(ase_ids.iter()) {
            if ase_id == target {
                *slot = true;
            }
        }
    }
    #[cfg(feature = "log")]
    log::info!("[le_audio_source] both channels streaming");
    #[cfg(feature = "defmt")]
    info!("[le_audio_source] both channels streaming");

    play_tone(stack, cig_manager, ase_ids).await;
}

/// Encodes and sends a generated two-tone stereo signal (440 Hz left, 880 Hz right) forever, paced
/// to the negotiated SDU interval.
async fn play_tone<C: Controller>(stack: &Stack<'_, C, DefaultPacketPool>, cig_manager: &CigManager<NoopRawMutex>, ase_ids: [u8; 2]) -> ! {
    const FREQUENCIES_HZ: [f32; 2] = [440.0, 880.0];
    const AMPLITUDE: f32 = 8000.0;
    const SAMPLE_RATE_HZ: f32 = 48_000.0;

    let mut ticker = iso_tx::ticker_for_sdu_interval(SDU_INTERVAL);
    let mut sequence_numbers = [iso_tx::SequenceNumber::default(); 2];
    let mut sample_index: u32 = 0;
    let mut encoded = [0u8; OCTETS_PER_FRAME as usize];
    let mut iso_buf: HVec<u8, { iso_tx::MAX_ISO_PACKET_LEN }> = HVec::new();

    loop {
        ticker.next().await;
        for (channel, &ase_id) in ase_ids.iter().enumerate() {
            let Some(cis_handle) = cig_manager.cis_handle(ase_id) else { continue };
            let mut pcm = [0i16; SAMPLES_PER_FRAME];
            for (i, sample) in pcm.iter_mut().enumerate() {
                let t = (sample_index as usize + i) as f32 / SAMPLE_RATE_HZ;
                *sample = (libm::sinf(2.0 * core::f32::consts::PI * FREQUENCIES_HZ[channel] * t) * AMPLITUDE) as i16;
            }
            if matches!(cig_manager.encode(ase_id, &pcm, &mut encoded), Some(Ok(()))) {
                let seq = sequence_numbers[channel].next();
                let packet = iso_tx::build_packet(&mut iso_buf, cis_handle, seq, &encoded);
                // Best-effort: an occasional dropped ISO write just drops that one SDU.
                let _ = stack.iso().send(&packet).await;
            }
        }
        sample_index = sample_index.wrapping_add(SAMPLES_PER_FRAME as u32);
    }
}
