//! A minimal LE Audio unicast sink peripheral. All the construction (HostResources, GATT server,
//! advertising) and the event loop (ASE Control Point state machine included) live in
//! `trouble_audio::le_audio::run_peripheral` - this just describes what a sink with one Sink ASE
//! looks like and hands it off.
//!
//! For the higher-level equivalent that doesn't require knowing PACS/ASCS by name, see
//! `trouble_audio::scenario::Scenario::headset(..)` (and `microphone.rs`/`headset_with_mic.rs` in
//! this crate for worked examples) - this file remains as the lower-level, more-control version.

use alloc::vec;

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use heapless::Vec as HVec;
use trouble_audio::{
    ascs::{Ase, AseType},
    cis::CisManager,
    generic_audio::{
        AudioLocation, CodecSpecificCapabilities, ContextType, OctetsPerCodecFrame, SamplingFrequency,
        SupportedFrameDurations, SupportedSamplingFrequencies,
    },
    iso::{LeAcceptCisRequest, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetupIsoDataPath},
    le_audio::{run_peripheral, BondStore, PeripheralConfig},
    pacs::{AudioContexts, PAC, PACRecord},
    CodecId,
};
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::info;

/// Max number of connections. This crate's `AscsServer` models a single active connection.
const CONNECTIONS_MAX: usize = 1;

/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC

/// Max number of Sink/Source ASEs this device exposes. Public so callers can size their own
/// `CisManager<M, MAX_ASES>` to match (see [`run`]).
pub const MAX_ASES: usize = 1;

/// Runs the audio sink peripheral forever on the given controller.
///
/// `cis_manager` is caller-owned so the caller can concurrently drain
/// [`CisManager::receive_pcm`] for decoded audio - e.g. to actually play it. This function never
/// returns; run it alongside whatever drains `cis_manager` (see [`count_decoded_frames`] for a
/// minimal example, or wire it to real audio output like the `linux` example's PipeWire sink).
pub async fn run<C>(controller: C, cis_manager: &CisManager<NoopRawMutex, 1>, bond_store: Option<&dyn BondStore>) -> !
where
    C: Controller
        + ControllerCmdAsync<LeAcceptCisRequest>
        + ControllerCmdSync<LeRejectCisRequest>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>,
{
    // Using a fixed "random" address can be useful for testing. In real scenarios, one would
    // use e.g. the MAC 6 byte array as the address (how to get that varies by the platform).
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);

    let config = PeripheralConfig {
        device_name: b"Ble Audio Sink",
        appearance: appearance::audio_sink::GENERIC_AUDIO_SINK,
        sink_pac: Some(PAC::new(&[PACRecord {
            codec_id: CodecId::default(), // LC3
            codec_specific_capabilities: vec![
                CodecSpecificCapabilities::SupportedSamplingFrequencies(SupportedSamplingFrequencies::new(&[
                    SamplingFrequency::Hz48000,
                ])),
                // Both of these are mandatory per the PACS spec - without them a central can't
                // derive any valid stream configuration from this PAC record at all (Android's LE
                // Audio client silently falls back to the phone speaker in exactly this case,
                // logging "Stream configuration is not valid").
                CodecSpecificCapabilities::SupportedFrameDurations(SupportedFrameDurations::default()), // 10ms only
                CodecSpecificCapabilities::SupportedOctetsPerCodecFrame(OctetsPerCodecFrame::new(26, 155)),
            ],
            metadata: vec![],
        }])),
        // A single location (mono), matching the single Sink ASE this example actually exposes
        // (`MAX_ASES = 1` below). Declaring both `FrontLeft | FrontRight` here previously made
        // Android's LE Audio client (correctly) decide it needed a 2-CIS stereo group
        // (`STEREO_TWO_CISES_PER_DEVICE`) - since only one ASE ever shows up, the group never
        // reaches "all ASEs configured" and CIG creation never fires, silently stalling until
        // Android's own state-machine timeout disconnects the link.
        sink_audio_locations: Some(AudioLocation::FrontLeft),
        source_pac: None,
        source_audio_locations: None,
        supported_audio_contexts: AudioContexts {
            sink_contexts: ContextType::Media | ContextType::Conversational,
            source_contexts: ContextType::empty(),
        },
        available_audio_contexts: AudioContexts {
            sink_contexts: ContextType::Media | ContextType::Conversational,
            source_contexts: ContextType::empty(),
        },
    };

    let mut ases = HVec::new();
    let _ = ases.push(AseType::Sink(Ase::new(0)));

    run_peripheral::<C, NoopRawMutex, MAX_ASES, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX>(
        controller,
        address,
        // No display/keyboard on a typical audio sink: JustWorks pairing (encrypted, no MITM
        // protection). Swap for `DisplayYesNo`/`KeyboardOnly`/etc. if the device has real IO.
        IoCapabilities::NoInputNoOutput,
        config,
        ases,
        cis_manager,
        bond_store,
    )
    .await
}

/// A minimal way to drain [`CisManager::receive_pcm`]: just counts and periodically logs decoded
/// frames, for platforms/examples with no real audio output wired up.
pub async fn count_decoded_frames(cis_manager: &CisManager<NoopRawMutex, 1>) -> ! {
    let mut frames: u32 = 0;
    loop {
        let _pcm = cis_manager.receive_pcm().await;
        frames += 1;
        if frames % 100 == 0 {
            #[cfg(feature = "defmt")]
            info!("[audio_sink] decoded {} LC3 frames", frames);
            #[cfg(feature = "log")]
            log::info!("[audio_sink] decoded {} LC3 frames", frames);
        }
    }
}
