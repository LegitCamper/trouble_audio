//! A minimal LE Audio unicast sink peripheral. All the construction (HostResources, GATT server,
//! advertising) and the event loop (ASE Control Point state machine included) live in
//! `trouble_audio::le_audio::run_peripheral` - this just describes what a sink with one Sink ASE
//! looks like and hands it off.

use alloc::vec;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use heapless::Vec as HVec;
use trouble_audio::{
    ascs::{Ase, AseType},
    generic_audio::{AudioLocation, CodecSpecificCapabilities, ContextType, SamplingFrequency, SupportedSamplingFrequencies},
    le_audio::{run_peripheral, PeripheralConfig},
    pacs::{AudioContexts, PAC, PACRecord},
    CodecId,
};
use trouble_host::prelude::*;

/// Max number of connections. This crate's `AscsServer` models a single active connection.
const CONNECTIONS_MAX: usize = 1;

/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC

/// Max number of Sink/Source ASEs this device exposes.
const MAX_ASES: usize = 1;

/// Runs the audio sink peripheral forever on the given controller.
pub async fn run<C>(controller: C) -> !
where
    C: Controller,
{
    // Using a fixed "random" address can be useful for testing. In real scenarios, one would
    // use e.g. the MAC 6 byte array as the address (how to get that varies by the platform).
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);

    let config = PeripheralConfig {
        device_name: b"Ble Audio Sink",
        appearance: appearance::audio_sink::GENERIC_AUDIO_SINK,
        sink_pac: Some(PAC::new(&[PACRecord {
            codec_id: CodecId::default(), // LC3
            codec_specific_capabilities: vec![CodecSpecificCapabilities::SupportedSamplingFrequencies(
                SupportedSamplingFrequencies::new(&[SamplingFrequency::Hz48000]),
            )],
            metadata: vec![],
        }])),
        sink_audio_locations: Some(AudioLocation::FrontLeft | AudioLocation::FrontRight),
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
    )
    .await
}
