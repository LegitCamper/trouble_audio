//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use super::generic_audio::*;
#[cfg(feature = "server")]
use super::LEAudioGattServer;

use core::slice;
use trouble_host::{prelude::*, types::gatt_traits::*};

#[cfg(feature = "defmt")]
use defmt::*;

use crate::CodecId;

#[cfg(feature = "server")]
pub(crate) async fn pacs_gatt_server(
    _server: &LEAudioGattServer<'_>,
    _connection_data: GattData<'_>,
) {
    #[cfg(feature = "defmt")]
    match _connection_data.process(_server).await {
        Ok(_) => {}
        Err(_e) => {
            #[cfg(feature = "defmt")]
            warn!("[gatt] error processing event: {:?}", _e);
        }
    }
}

pub(crate) const PACS_UUID: Uuid = Uuid::new_short(0x1850);

/// Published Audio Capabilities Service
#[cfg(feature = "server")]
#[gatt_service(uuid = PACS_UUID)]
pub struct Pacs {
    /// Source PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BCB", read, notify)]
    pub source_pac: PAC,

    /// Source Audio Locations characteristic
    #[characteristic(uuid = "2BCC", read, notify, write)]
    pub source_audio_locations: AudioLocation,

    /// Available Audio Contexts characteristic
    #[characteristic(uuid = "2BCD", read, notify)]
    pub available_audio_contexts: AudioContexts,
}

/// Published Audio Capabilities Service
#[cfg(feature = "client")]
// #[gatt_service(uuid = PACS_UUID)]
pub struct Pacs {
    /// Sink PAC characteristic containing one or more PAC records
    // #[characteristic(uuid = "2BC9", read, notify)]
    pub sink_pac: PAC,

    /// Sink Audio Locations characteristic
    // #[characteristic(uuid = "2BCA", read, notify, write)]
    pub sink_audio_locations: AudioLocation,

    /// Supported Audio Contexts characteristic
    // #[characteristic(uuid = "2BCE", read, notify)]
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
