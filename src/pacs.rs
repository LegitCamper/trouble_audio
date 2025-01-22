//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use super::generic_audio::*;
use core::slice;
use trouble_host::{prelude::*, types::gatt_traits::*};

use crate::CodecdId;

/// Published Audio Capabilities Service
#[gatt_service(uuid = 0x1850)]
pub struct PublishedAudioCapabilitiesService {
    /// Sink PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BC9", read, notify)]
    sink_pac: SinkPAC,

    /// Sink Audio Locations characteristic
    #[characteristic(uuid = "2BCA", read, notify, write)]
    sink_audio_locations: AudioLocation,

    /// Source PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BCB", read, notify)]
    source_pac: SourcePAC,

    /// Source Audio Locations characteristic
    #[characteristic(uuid = "2BCC", read, notify, write)]
    source_audio_locations: AudioLocation,

    /// Available Audio Contexts characteristic
    #[characteristic(uuid = "2BCD", read, notify)]
    available_audio_contexts: AvailableAudioContexts,

    /// Supported Audio Contexts characteristic
    #[characteristic(uuid = "2BCD", read, notify)]
    supported_audio_contexts: SupportedAudioContexts,
}

/// A set of parameter values that denote server audio capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PACRecord {
    codec_id: CodecdId,
    codec_specific_capabilities: &'static [CodecSpecificCapabilities],
    metadata: Option<&'static [u8]>,
}

impl PACRecord {
    /// NOTE:
    ///
    /// If the server wished to support only 30-octet codec frame lengths
    /// and 50-octet codec frame lengths, but not support codec frame
    /// lengths in-between these minimum and maximum values, then
    /// the server would need to expose discrete PAC records with
    /// the minimum and maximum values set to 30 octets in one PAC record,
    /// and with the minimum and maximum values set to 50 octets in
    /// another PAC record, as shown in Table 2.3.
    /// <https://www.bluetooth.com/specifications/specs/pacs-1-0-2/>
    pub fn new(
        codec_id: CodecdId,
        codec_specific_capabilities: &'static [CodecSpecificCapabilities],
        metadata: Option<&'static [u8]>,
    ) -> Self {
        PACRecord {
            codec_id,
            codec_specific_capabilities,
            metadata,
        }
    }
}

impl GattValue for PACRecord {
    const MIN_SIZE: usize = size_of::<PACRecord>();

    const MAX_SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        todo!()
    }

    fn to_gatt(&self) -> &[u8] {
        todo!()
    }
}

/// Sink PAC characteristic containing one or more PAC records.
#[derive(Default)]
pub struct SinkPAC {
    pub number_of_pac_records: u8,         // Number of PAC records
    pub pac_records: &'static [PACRecord], // Array of PAC records
}

impl FixedGattValue for SinkPAC {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            // SAFETY
            // - Pointer is considered "valid" as per the rules outlined for validity in std::ptr v1.82.0
            // - Pointer was generated from a slice of bytes matching the size of the type implementing Primitive,
            //     and all types implementing Primitive are valid for all possible configurations of bits
            // - Primitive trait is constrained to require Copy
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        // SAFETY
        // - Slice is of type u8 so data is guaranteed valid for reads of any length
        // - Data and len are tied to the address and size of the type
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

/// The Sink Audio Locations characteristic i
/// The Source PAC characteristic is used to expose PAC records when the server supports transmission of audio data.
#[derive(Default)]
pub struct SourcePAC {
    pub number_of_pac_records: u8,         // Number of PAC records
    pub pac_records: &'static [PACRecord], // Array of PAC records
}

impl FixedGattValue for SourcePAC {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            // SAFETY
            // - Pointer is considered "valid" as per the rules outlined for validity in std::ptr v1.82.0
            // - Pointer was generated from a slice of bytes matching the size of the type implementing Primitive,
            //     and all types implementing Primitive are valid for all possible configurations of bits
            // - Primitive trait is constrained to require Copy
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        // SAFETY
        // - Slice is of type u8 so data is guaranteed valid for reads of any length
        // - Data and len are tied to the address and size of the type
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

/// The Available Audio Contexts characteristic exposes the availability of the server
/// for reception and/or transmission of unicast audio data only, associated with specific Context Types.
#[derive(Default)]
pub struct AvailableAudioContexts {
    // Bitmask of audio data Context Type values available for reception.
    available_sink_contexts: ContextType,
    // Bitmask of audio data Context Type values available for transmission.
    available_source_contexts: ContextType,
}

impl FixedGattValue for AvailableAudioContexts {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            // SAFETY
            // - Pointer is considered "valid" as per the rules outlined for validity in std::ptr v1.82.0
            // - Pointer was generated from a slice of bytes matching the size of the type implementing Primitive,
            //     and all types implementing Primitive are valid for all possible configurations of bits
            // - Primitive trait is constrained to require Copy
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        // SAFETY
        // - Slice is of type u8 so data is guaranteed valid for reads of any length
        // - Data and len are tied to the address and size of the type
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

/// The Supported Audio Contexts characteristic exposes the server’s support for reception
/// and/or transmission of unicast audio data and/or broadcast audio data associated with specific Context Types.
#[derive(Default)]
pub struct SupportedAudioContexts {
    // Bitmask of audio data Context Type values available for reception.
    available_sink_contexts: ContextType,
    // Bitmask of audio data Context Type values available for transmission.
    available_source_contexts: ContextType,
}

impl FixedGattValue for SupportedAudioContexts {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            // SAFETY
            // - Pointer is considered "valid" as per the rules outlined for validity in std::ptr v1.82.0
            // - Pointer was generated from a slice of bytes matching the size of the type implementing Primitive,
            //     and all types implementing Primitive are valid for all possible configurations of bits
            // - Primitive trait is constrained to require Copy
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        // SAFETY
        // - Slice is of type u8 so data is guaranteed valid for reads of any length
        // - Data and len are tied to the address and size of the type
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characteristics() {
        let sink_pac = SinkPAC::default();
        let sink_pac_gatt = GattValue::to_gatt(&sink_pac);
        <SinkPAC as FixedGattValue>::from_gatt(sink_pac_gatt).unwrap();

        let source_pac = SourcePAC::default();
        let source_pac_gatt = GattValue::to_gatt(&source_pac);
        <SourcePAC as FixedGattValue>::from_gatt(source_pac_gatt).unwrap();

        let available_audio_locations = AvailableAudioContexts::default();
        let available_audio_locations_gatt = GattValue::to_gatt(&available_audio_locations);
        <AvailableAudioContexts as FixedGattValue>::from_gatt(available_audio_locations_gatt)
            .unwrap();

        let supported_audio_locations = SupportedAudioContexts::default();
        let supported_audio_locations_gatt = GattValue::to_gatt(&supported_audio_locations);
        <SupportedAudioContexts as FixedGattValue>::from_gatt(supported_audio_locations_gatt)
            .unwrap();

        let source_pac = SourcePAC::default();
        let source_pac_gatt = GattValue::to_gatt(&source_pac);
        <SourcePAC as FixedGattValue>::from_gatt(source_pac_gatt).unwrap();
    }
}
