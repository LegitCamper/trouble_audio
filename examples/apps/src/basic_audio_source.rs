//! A minimal LE Audio unicast source central: connects to the stereo sink this session's
//! [`crate::basic_audio_sink`] describes, negotiates both its Sink ASEs, and streams a generated
//! test tone down to it over CIS/ISO. The mirror image of `basic_audio_sink`/[`crate::sink`]
//! (peripheral, GATT server) - this is a GATT client instead, using `trouble_audio::{ase_client,
//! cig}` (the central-side ASE Control Point driver and CIG/CIS establishment manager) rather than
//! `AscsServer`/`CisManager`.

use alloc::vec::Vec as AVec;

use bt_hci::cmd::le::{
    LeCreateCis, LeExtCreateConn, LeReadLocalSupportedFeatures, LeRemoveCig, LeRemoveIsoDataPath,
    LeSetHostFeature, LeSetupIsoDataPath,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_futures::select::{select, select4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use heapless::Vec as HVec;
use trouble_audio::ascs::{AscsClient, Ase, ConfigQosEntry, TargetLatency};
use trouble_audio::cig::{self, AseQos, CigManager, SetCigParametersCmd};
use trouble_audio::generic_audio::{
    AudioLocation, CodecSpecificConfiguration, ContextType, Metadata, encode_list,
};
use trouble_audio::lc3::Lc3MonoEncoder;
use trouble_audio::presets::AudioPreset;
use trouble_audio::{CodecId, ase_client, cis, iso_tx};
use trouble_host::prelude::*;

use crate::sink::BondStore;

#[cfg(feature = "defmt")]
use defmt::{Debug2Format, info, warn};

/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC
/// Max number of connections.
const CONNECTIONS_MAX: usize = 1;
/// Max number of GATT services this client caches per connection.
const MAX_SERVICES: usize = 20;

/// Largest PCM and encoded frames supported by the preset table. Buffers stay fixed-size so the
/// embedded source does no allocation while streaming; only the negotiated prefix is used.
const MAX_SAMPLES_PER_FRAME: usize = 480;
const MAX_OCTETS_PER_FRAME: usize = 155;

const CIG_ID: u8 = 1;
const TONE_FREQUENCIES_HZ: [f32; 2] = [440.0, 880.0];
const TONE_AMPLITUDE: f32 = 8000.0;
// Both tones complete an integer number of cycles in 50 ms (22 and 44 respectively), so five
// 10-ms frames form a seamless repeating test waveform.
const TONE_PERIOD_FRAMES: usize = 5;
type EncodedTone = [[[u8; MAX_OCTETS_PER_FRAME]; TONE_PERIOD_FRAMES]; 2];

/// Runs the audio source central forever on the given controller: connects to `target` (a
/// "random" address, e.g. [`crate::basic_audio_sink::ADDRESS`]), then streams a generated stereo
/// test tone to it. Never returns.
///
/// `bond_store`, if given, persists the bond so that reconnects after a process restart don't need
/// to re-pair (the sink remembers the LTK; without a stored bond on the source side, it won't know
/// to resume encryption rather than pair fresh, which can fail if the peer enforces re-pairing
/// prevention).
pub async fn run<C>(
    controller: C,
    our_address: [u8; 6],
    target: [u8; 6],
    bond_store: Option<&dyn BondStore>,
    preset: AudioPreset,
    channels: usize,
) -> !
where
    C: Controller
        + for<'a> ControllerCmdSync<SetCigParametersCmd<'a>>
        + for<'a> ControllerCmdAsync<LeCreateCis<'a>>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeRemoveCig>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>
        + ControllerCmdAsync<LeExtCreateConn>,
{
    let target = Address::random(target);
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
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
    // This example sends a fixed pre-encoded tone. Passthrough avoids retaining two large LC3
    // encoders after the tone has been cached and lets the first ISO packet be sent immediately
    // when the data paths open.
    assert!(channels == 1 || channels == 2, "only mono and stereo are supported");
    let cig_manager = if channels == 1 {
        CigManager::<NoopRawMutex>::new_mono_passthrough()
    } else {
        CigManager::<NoopRawMutex>::new_passthrough()
    };

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
                connect_and_stream(
                    &stack,
                    &mut central,
                    target,
                    &cig_manager,
                    bond_store,
                    preset,
                    channels,
                )
                .await;
                embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
            }
        },
    )
    .await;

    unreachable!("all branches above loop forever")
}

async fn connect_and_stream<C: Controller + ControllerCmdAsync<LeExtCreateConn>>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    central: &mut Central<'_, C, DefaultPacketPool>,
    target: Address,
    cig_manager: &CigManager<NoopRawMutex>,
    bond_store: Option<&dyn BondStore>,
    preset: AudioPreset,
    channels: usize,
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
        info!(
            "[le_audio_source] connecting to {:?}",
            Debug2Format(&target)
        );
        match embassy_time::with_timeout(
            embassy_time::Duration::from_secs(10),
            central.connect_ext(&config),
        )
        .await
        {
            Ok(Ok(conn)) => break conn,
            Ok(Err(_e)) => {
                #[cfg(feature = "log")]
                log::warn!("[le_audio_source] connect failed: {:?}", _e);
                #[cfg(feature = "defmt")]
                warn!("[le_audio_source] connect failed: {}", Debug2Format(&_e));
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
            ConnectionEvent::PairingComplete {
                bond: Some(bond), ..
            } => {
                // Keep the bond info; we'll persist it once Encrypted confirms the link is up.
                let _ = stack.add_bond_information(bond.clone());
                pending_bond = Some(bond);
            }
            ConnectionEvent::Encrypted {
                bond: Some(bond), ..
            } => {
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
        let ascs = match AscsClient::new(&client).await {
            Ok(ascs) => ascs,
            Err(_e) => {
                #[cfg(feature = "log")]
                log::warn!("[le_audio_source] ASCS discovery failed: {:?}", _e);
                #[cfg(feature = "defmt")]
                warn!("[le_audio_source] ASCS discovery failed");
                return;
            }
        };
        stream_tone(stack, &client, &ascs, cig_manager, preset, channels).await;
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
    preset: AudioPreset,
    channels: usize,
) {
    let phy = if preset.sdu_interval_us() == 7_500 {
        PhySet::M2
    } else {
        PhySet::M1
    };
    let mut ase_ids = [0u8; 2];
    let mut found = 0;
    for (i, characteristic) in ascs.sink_ases().take(channels).enumerate() {
        let mut buf = [0u8; 90];
        let Ok(n) = client.read_characteristic(characteristic, &mut buf).await else {
            return;
        };
        let Ok(ase) = Ase::from_gatt(&buf[..n]) else {
            return;
        };
        ase_ids[i] = ase.id();
        found += 1;
    }
    if found < channels {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] peer exposes too few Sink ASEs");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] peer exposes too few Sink ASEs");
        return;
    }
    let ase_ids = &ase_ids[..channels];

    let codec_config = |location: AudioLocation| {
        encode_list(&[
            CodecSpecificConfiguration::SamplingFrequency(preset.sampling_frequency),
            CodecSpecificConfiguration::FrameDuration(preset.frame_duration),
            CodecSpecificConfiguration::OctetsPerCodecFrame(preset.octets_per_frame),
            CodecSpecificConfiguration::AudioChannelAllocation(location),
        ])
    };
    let locations = [AudioLocation::FrontLeft, AudioLocation::FrontRight];
    let mut config_codec_entries: AVec<_> = AVec::new();
    for (&ase_id, &location) in ase_ids.iter().zip(locations.iter()) {
        let _ = config_codec_entries.push((
            ase_id,
            TargetLatency::Lower,
            phy,
            CodecId::default(),
            codec_config(location),
        ));
    }
    if ase_client::config_codec(client, ascs, config_codec_entries)
        .await
        .is_err()
    {
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
        sdu_interval: preset.sdu_interval_us().to_le_bytes()[..3]
            .try_into()
            .unwrap(),
        framing: 0,
        phy,
        max_sdu: preset.octets_per_frame,
        retransmission_number: preset.rtn,
        max_transport_latency: preset.max_transport_latency_ms,
        presentation_delay: preset.presentation_delay_us.to_le_bytes()[..3]
            .try_into()
            .unwrap(),
    };
    let all_qos = [qos(0), qos(1)];
    let ase_qos = &all_qos[..channels];
    let mut config_qos_entries: AVec<ConfigQosEntry> = AVec::new();
    for (&ase_id, q) in ase_ids.iter().zip(ase_qos.iter()) {
        let _ = config_qos_entries.push((
            ase_id,
            q.cig_id,
            q.cis_id,
            q.sdu_interval,
            q.framing,
            q.phy,
            q.max_sdu,
            q.retransmission_number,
            q.max_transport_latency,
            q.presentation_delay,
        ));
    }
    if ase_client::config_qos(client, ascs, config_qos_entries)
        .await
        .is_err()
    {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] Config QoS failed");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] Config QoS failed");
        return;
    }

    // Encoding two channels live takes longer than the 10-ms radio interval on the nRF54L15.
    // Prepare the repeating waveform before CIG creation so sequence zero is ready as soon as the
    // CIS data paths become usable.
    let tone = encode_test_tone(preset);

    // Both ASEs are QoS-configured - `cig_manager` now has everything it needs to create the CIG
    // and both CIS (carried out by `cig::drive_cig`, running concurrently - see `run`).
    for (&ase_id, &q) in ase_ids.iter().zip(ase_qos.iter()) {
        cig_manager.configure(ase_id, q, preset.sampling_frequency, preset.frame_duration);
    }

    let metadata = encode_list(&[Metadata::StreamingAudioContexts(ContextType::Media)]);
    let mut enable_entries: AVec<_> = AVec::new();
    for &ase_id in ase_ids.iter() {
        let _ = enable_entries.push((ase_id, metadata.clone()));
    }
    if ase_client::enable(client, ascs, enable_entries)
        .await
        .is_err()
    {
        #[cfg(feature = "log")]
        log::warn!("[le_audio_source] Enable failed");
        #[cfg(feature = "defmt")]
        warn!("[le_audio_source] Enable failed");
        return;
    }

    let mut ready = [false; 2];
    while ready[..channels].iter().any(|r| !r) {
        let ase_id = cig_manager.next_ready_ase().await;
        for (slot, &target) in ready.iter_mut().zip(ase_ids.iter()) {
            if ase_id == target {
                *slot = true;
            }
        }
    }
    #[cfg(feature = "log")]
    log::info!(
        "[le_audio_source] both channels streaming preset={} interval_us={}",
        preset.name,
        preset.sdu_interval_us()
    );
    #[cfg(feature = "defmt")]
    info!(
        "[le_audio_source] both channels streaming preset={} interval_us={}",
        preset.name,
        preset.sdu_interval_us()
    );

    // The peer learns that each CIS is established before its asynchronous HCI data-path setup
    // command has necessarily completed. Give a small embedded peer time to install both paths;
    // sending immediately can keep its receive runner continuously ready and starve the second
    // setup command.
    embassy_time::Timer::after_millis(250).await;

    play_tone(stack, cig_manager, ase_ids, &tone, preset).await;
}

fn encode_test_tone(preset: AudioPreset) -> EncodedTone {
    let mut tone = [[[0u8; MAX_OCTETS_PER_FRAME]; TONE_PERIOD_FRAMES]; 2];
    let samples_per_frame = preset.samples_per_frame();
    let octets_per_frame = usize::from(preset.octets_per_frame);
    for (channel, frequency_hz) in TONE_FREQUENCIES_HZ.into_iter().enumerate() {
        let mut encoder = Lc3MonoEncoder::new(preset.sampling_frequency, preset.frame_duration)
            .expect("the negotiated LC3 sampling frequency is supported");
        for frame in 0..TONE_PERIOD_FRAMES {
            let mut pcm = [0i16; MAX_SAMPLES_PER_FRAME];
            for (i, sample) in pcm[..samples_per_frame].iter_mut().enumerate() {
                let sample_index = frame * samples_per_frame + i;
                let t = sample_index as f32 / preset.sampling_rate_hz() as f32;
                *sample = (libm::sinf(2.0 * core::f32::consts::PI * frequency_hz * t)
                    * TONE_AMPLITUDE) as i16;
            }
            encoder
                .encode(
                    &pcm[..samples_per_frame],
                    &mut tone[channel][frame][..octets_per_frame],
                )
                .expect("fixed-size LC3 test-tone frame is valid");
        }
    }
    tone
}

/// Encodes and sends a generated two-tone stereo signal (440 Hz left, 880 Hz right) forever, paced
/// to the negotiated SDU interval.
async fn play_tone<C: Controller>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    cig_manager: &CigManager<NoopRawMutex>,
    ase_ids: &[u8],
    tone: &EncodedTone,
    preset: AudioPreset,
) -> ! {
    let sdu_interval = preset.sdu_interval_us().to_le_bytes()[..3]
        .try_into()
        .unwrap();
    let mut ticker = iso_tx::ticker_for_sdu_interval(sdu_interval);
    let octets_per_frame = usize::from(preset.octets_per_frame);
    let mut sequence_numbers = [iso_tx::SequenceNumber::default(); 2];
    let mut iso_buf: HVec<u8, { iso_tx::MAX_ISO_PACKET_LEN }> = HVec::new();
    let mut frames = 0u32;
    let mut packets_sent = 0u32;
    let mut packets_dropped = 0u32;

    loop {
        ticker.next().await;
        let frame = frames as usize % TONE_PERIOD_FRAMES;
        for (channel, &ase_id) in ase_ids.iter().enumerate() {
            let Some(cis_handle) = cig_manager.cis_handle(ase_id) else {
                continue;
            };
            let seq = sequence_numbers[channel].next();
            // The frame size is fixed at build time, well under the packet bound.
            let packet = iso_tx::build_packet(
                &mut iso_buf,
                cis_handle,
                seq,
                &tone[channel][frame][..octets_per_frame],
            )
            .unwrap();
            // Best-effort: an occasional dropped ISO write just drops that one SDU.
            match stack.iso().send(&packet).await {
                Ok(()) => packets_sent = packets_sent.wrapping_add(1),
                Err(_) => packets_dropped = packets_dropped.wrapping_add(1),
            }
        }
        frames = frames.wrapping_add(1);
        if frames % 100 == 0 {
            #[cfg(feature = "log")]
            log::info!(
                "[le_audio_source] ISO packets: sent={}, dropped={}",
                packets_sent,
                packets_dropped
            );
            #[cfg(feature = "defmt")]
            info!(
                "[le_audio_source] ISO packets: sent={}, dropped={}",
                packets_sent, packets_dropped
            );
        }
    }
}
