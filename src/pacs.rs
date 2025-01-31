//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use super::generic_audio::*;

use core::slice;
use embassy_futures::select::select;
use trouble_host::{prelude::*, types::gatt_traits::*};

use bt_hci::uuid::{characteristic, service};

#[cfg(feature = "defmt")]
use defmt::*;

mod source;
pub use source::{source_client, source_server};
mod sink;
pub use sink::{sink_client, sink_server};

use crate::CodecId;

/// Published Audio Capabilities Service Central
#[gatt_service(uuid = service::PUBLISHED_AUDIO_CAPABILITIES)]
pub struct PacsSource {
    /// Source PAC characteristic containing one or more PAC records
    #[characteristic(uuid = characteristic::SOURCE_PAC, read, notify)]
    pub source_pac: PAC,

    /// Source Audio Locations characteristic
    #[characteristic(uuid = characteristic::SOURCE_AUDIO_LOCATIONS, read, notify, write)]
    pub source_audio_locations: AudioLocation,

    /// Available Audio Contexts characteristic
    #[characteristic(uuid = characteristic::AVAILABLE_AUDIO_CONTEXTS, read, notify)]
    pub available_audio_contexts: AudioContexts,
}

/// Published Audio Capabilities Service Peripheral
#[gatt_service(uuid = service::PUBLISHED_AUDIO_CAPABILITIES)]
pub struct PacsSink {
    /// Sink PAC characteristic containing one or more PAC records
    #[characteristic(uuid = characteristic::SINK_PAC, read, notify)]
    pub sink_pac: PAC,

    /// Sink Audio Locations characteristic
    #[characteristic(uuid = characteristic::SINK_AUDIO_LOCATIONS, read, notify, write)]
    pub sink_audio_locations: AudioLocation,

    /// Supported Audio Contexts characteristic
    #[characteristic(uuid = characteristic::SUPPORTED_AUDIO_CONTEXTS, read, notify)]
    pub supported_audio_contexts: AudioContexts,
}

/// A set of parameter values that denote server audio capabilities.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default)]
pub struct PACRecord {
    pub codec_id: CodecId,
    pub codec_specific_capabilities: &'static [CodecSpecificCapabilities],
    pub metadata: &'static [Metadata],
}

/// The Sink Audio Locations characteristic i
/// The Source PAC characteristic is used to expose PAC records when the server supports transmission of audio data.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub struct PAC {
    number_of_pac_records: u8,
    pac_records: &'static [PACRecord],
}

impl PAC {
    fn new(records: &'static [PACRecord]) -> Self {
        Self {
            number_of_pac_records: records.len() as u8,
            pac_records: records,
        }
    }
}

impl FixedGattValue for PAC {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub struct AudioContexts {
    /// Bitmask of audio data Context Type values for reception.
    pub sink_contexts: ContextType,
    /// Bitmask of audio data Context Type values for transmission.
    pub source_contexts: ContextType,
}

impl FixedGattValue for AudioContexts {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}
