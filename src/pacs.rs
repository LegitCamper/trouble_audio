//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use core::{mem::size_of_val, slice};
use defmt::*;
use trouble_host::{prelude::*, types::gatt_traits::*, Error};

/// Published Audio Capabilities Service
#[gatt_service(uuid = 0x1850)]
pub struct PublishedAudioCapabilitiesService {
    /// Sink PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BC9", read, notify)]
    sink_pac: SinkPAC,

    /// Sink Audio Locations characteristic
    #[characteristic(uuid = "2BCA", read, notify, write)]
    sink_audio_locations: SinkAudioLocations,

    /// Source PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BCB", read, notify)]
    source_pac: SourcePAC,

    /// Source Audio Locations characteristic
    #[characteristic(uuid = "2BCC", read, notify, write)]
    source_audio_locations: SourceAudioLocations,

    /// Available Audio Contexts characteristic
    #[characteristic(uuid = "2BCD", read, notify)]
    available_audio_contexts: AvailableAudioContexts,

    /// Supported Audio Contexts characteristic
    #[characteristic(uuid = "2BCD", read, notify)]
    supported_audio_contexts: SupportedAudioContexts,
}

/// A set of parameter values that denote server audio capabilities.
pub struct PACRecord {
    codec_id: u64,
    codec_specific_capabilities_length: u16,
    codec_specific_capabilities: CodecSpecificCapabilities,
    metadata_length: u16,
    metadata: Option<u64>,
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
        codec_id: u64,
        codec_specific_capabilities: CodecSpecificCapabilities,
        metadata: Option<u64>,
    ) -> Self {
        PACRecord {
            codec_id,
            codec_specific_capabilities_length: size_of_val(&codec_specific_capabilities) as u16,
            codec_specific_capabilities,
            metadata_length: size_of_val(&metadata) as u16,
            metadata,
        }
    }
}

impl Default for PACRecord {
    fn default() -> Self {
        Self::new(
            1,
            CodecSpecificCapabilities::new(
                SupportedSamplingFrequencies::default(),
                SupportedOctetsPerCodecFrame::new(30, 50),
            ),
            None,
        )
    }
}

pub struct CodecSpecificCapabilities {
    supported_sampling_frequencies: SupportedSamplingFrequencies,
    supported_octets_per_codec_frame: SupportedOctetsPerCodecFrame,
}

impl CodecSpecificCapabilities {
    pub fn new(
        supported_sampling_frequencies: SupportedSamplingFrequencies,
        supported_octets_per_codec_frame: SupportedOctetsPerCodecFrame,
    ) -> Self {
        Self {
            supported_sampling_frequencies,
            supported_octets_per_codec_frame,
        }
    }
}

pub struct SupportedOctetsPerCodecFrame {
    min_octets: u16,
    max_octets: u16,
}

impl SupportedOctetsPerCodecFrame {
    pub fn new(min_octets: u16, max_octets: u16) -> Self {
        if min_octets > max_octets {
            warn!("min_octets cannot be greater than max_octets");

            return Self {
                min_octets: max_octets,
                max_octets: min_octets,
            };
        }
        Self {
            min_octets,
            max_octets,
        }
    }

    fn encode(&self) -> u32 {
        ((self.max_octets as u32) << 16) | self.min_octets as u32
    }

    fn decode(encoded: u32) -> Self {
        let min_octets = (encoded & 0xFFFF) as u16;
        let max_octets = (encoded >> 16) as u16;
        Self::new(min_octets, max_octets)
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub enum SamplingFrequency {
    #[default]
    Hz8000 = 0,
    Hz11025 = 1,
    Hz16000 = 2,
    Hz22050 = 3,
    Hz24000 = 4,
    Hz32000 = 5,
    Hz44100 = 6,
    Hz48000 = 7,
    Hz88200 = 8,
    Hz96000 = 9,
    Hz176400 = 10,
    Hz192000 = 11,
    Hz384000 = 12,
}

impl SamplingFrequency {
    fn bit_position(&self) -> u8 {
        *self as u8
    }
}

pub struct SupportedSamplingFrequencies(u16);

impl Default for SupportedSamplingFrequencies {
    fn default() -> Self {
        Self(1 << SamplingFrequency::default().bit_position())
    }
}

impl SupportedSamplingFrequencies {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(self, sampling_frequency: SamplingFrequency) -> Self {
        Self(self.0 + 1 << sampling_frequency.bit_position())
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

#[derive(Default, Debug, PartialEq, Eq)]
pub enum AudioLocation {
    #[default]
    Mono = 0x00000000, // Mono Audio (no specified Audio Location)
    FrontLeft = 0x00000001,
    FrontRight = 0x00000002,
    FrontCenter = 0x00000004,
    LowFrequencyEffects1 = 0x00000008,
    BackLeft = 0x00000010,
    BackRight = 0x00000020,
    FrontLeftOfCenter = 0x00000040,
    FrontRightOfCenter = 0x00000080,
    BackCenter = 0x00000100,
    LowFrequencyEffects2 = 0x00000200,
    SideLeft = 0x00000400,
    SideRight = 0x00000800,
    TopFrontLeft = 0x00001000,
    TopFrontRight = 0x00002000,
    TopFrontCenter = 0x00004000,
    TopCenter = 0x00008000,
    TopBackLeft = 0x00010000,
    TopBackRight = 0x00020000,
    TopSideLeft = 0x00040000,
    TopSideRight = 0x00080000,
    TopBackCenter = 0x00100000,
    BottomFrontCenter = 0x00200000,
    BottomFrontLeft = 0x00400000,
    BottomFrontRight = 0x00800000,
    FrontLeftWide = 0x01000000,
    FrontRightWide = 0x02000000,
    LeftSurround = 0x04000000,
    RightSurround = 0x08000000,
}

/// The Sink Audio Locations characteristic is used to expose
/// the supported Audio Locations when the server supports reception of audio data.
#[derive(Default)]
pub struct SinkAudioLocations(AudioLocation);

impl SinkAudioLocations {
    fn new(audio_location: AudioLocation) -> Self {
        Self(audio_location)
    }

    /// If the server detects that the Sink_Audio_Locations parameter value,
    /// written by a client by using the GATT Write Characteristic Value sub-procedure,
    /// is not 4 octets in length, or if the parameter value written
    /// includes any RFU bits set to a value of 0b1, the server shall
    /// respond with an ATT Error Response and shall set the Error Code parameter to
    pub fn verify(&self, client_audio_location: AudioLocation) -> Result<(), Error> {
        if client_audio_location != self.0 {
            return Err(Error::Att(AttErrorCode::WriteNotPermitted));
        }
        Ok(())
    }
}

impl FixedGattValue for SinkAudioLocations {
    const SIZE: usize = size_of::<Self>();

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

/// The Source Audio Locations characteristic is used to expose the
/// supported Audio Locations when the server supports transmission of audio data.
#[derive(Default)]
pub struct SourceAudioLocations(
    ///Device-wide bitmap of supported Audio Location values for
    /// all PAC records where the server supports reception of audio data.
    AudioLocation,
);

impl SourceAudioLocations {
    fn new(audio_location: AudioLocation) -> Self {
        Self(audio_location)
    }

    /// If the server detects that the Source_Audio_Locations parameter value,
    /// written by a client by using the GATT Write Characteristic Value sub-procedure,
    /// is not 4 octets in length, or if the parameter value written
    /// includes any RFU bits set to a value of 0b1, the server shall respond
    /// with an ATT Error Response and shall set the Error Code parameter to
    pub fn verify(&self, client_audio_location: AudioLocation) -> Result<(), Error> {
        if client_audio_location != self.0 {
            return Err(Error::Att(AttErrorCode::WriteNotPermitted));
        }
        Ok(())
    }
}

impl FixedGattValue for SourceAudioLocations {
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

/// A bitfield of values that, when set to 0b1 for a bit,
/// describes audio data as being intended for the use case represented by that bit.
#[derive(Default)]
pub enum ContextType {
    /// Prohibited
    #[default]
    Prohibited = 0x0000,
    /// Identifies audio where the use case context does not match any other defined value,
    /// or where the context is unknown or cannot be determined.
    Unspecified = 0x0001,
    /// Conversation between humans, for example, in telephony or video calls,
    /// including traditional cellular as well as VoIP and Push-to-Talk.
    Conversational = 0x0002,
    /// Media, for example, music playback, radio, podcast or movie soundtrack, or TV audio.
    Media = 0x0004,
    /// Audio associated with video gaming, for example gaming media; gaming effects;
    /// music and in-game voice chat between participants; or a mix of all the above.
    Game = 0x0008,
    /// Instructional audio, for example, in navigation, announcements, or user guidance.
    Instructional = 0x0010,
    /// Man-machine communication, for example, with voice recognition or virtual assistants.
    VoiceAssistants = 0x0020,
    /// Live audio, for example, from a microphone where audio is perceived both
    /// through a direct acoustic path and through an LE Audio Stream.
    Live = 0x0040,
    /// Sound effects including keyboard and touch feedback;
    /// menu and user interface sounds; and other system sounds.
    SoundEffects = 0x0080,
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

        let sink_audio_locations = SinkAudioLocations::default();
        let sink_audio_locations_gatt = GattValue::to_gatt(&sink_audio_locations);
        <SinkAudioLocations as FixedGattValue>::from_gatt(sink_audio_locations_gatt)
            .unwrap()
            .verify(AudioLocation::default())
            .unwrap();

        let source_pac = SourcePAC::default();
        let source_pac_gatt = GattValue::to_gatt(&source_pac);
        <SourcePAC as FixedGattValue>::from_gatt(source_pac_gatt).unwrap();

        let source_audio_locations = SourceAudioLocations::default();
        let source_audio_locations_gatt = GattValue::to_gatt(&source_audio_locations);
        <SourceAudioLocations as FixedGattValue>::from_gatt(source_audio_locations_gatt)
            .unwrap()
            .verify(AudioLocation::default())
            .unwrap();

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
