// Audio Capabilities Service 1.0.2
//
// The Published Audio Capabilities (PACS) service exposes
// server audio capabilities and audio availability, allowing discovery by clients.

use core::mem::size_of_val;
use defmt::*;
use trouble_host::Error;
use trouble_host::prelude::*;

pub const UUID: u32 = 0x1850;

/// A set of parameter values that denote server audio capabilities.
pub struct PACRecord<'a> {
    codec_id: u64,
    codec_specific_capabilities_length: u16,
    codec_specific_capabilities: CodecSpecificCapabilities<'a>,
    metadata_length: u16,
    metadata: Option<u64>,
}

impl<'a> PACRecord<'a> {
    /// NOTE:
    ///
    /// If the server wished to support only 30-octet codec frame lengths
    /// and 50-octet codec frame lengths, but not support codec frame
    /// lengths in-between these minimum and maximum values, then
    /// the server would need to expose discrete PAC records with
    /// the minimum and maximum values set to 30 octets in one PAC record,
    /// and with the minimum and maximum values set to 50 octets in
    /// another PAC record, as shown in Table 2.3.
    /// https://www.bluetooth.com/specifications/specs/pacs-1-0-2/
    pub fn new(
        codec_id: u64,
        codec_specific_capabilities: CodecSpecificCapabilities<'a>,
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

pub struct CodecSpecificCapabilities<'a> {
    supported_sampling_frequencies: &'a [SupportedSamplingFrequency],
    supported_octets_per_codec_frame: SupportedOctetsPerCodecFrame,
}

// impl<'a> CodecSpecificCapabilities<'a> {

//     pub fn as_bytes(Self<'a>) -> &'a [u8] {
//           let mut idx = 0;

//         if buf.len() > 0 {
//             buf[idx] = self.capability_type;
//             idx += 1;
//         }

//         let len = self.capabilities.len();
//         if len <= buf.len() - idx {
//             buf[idx..idx + len].copy_from_slice(self.capabilities);
//             idx += len;
//         }

//         buf

//     }

// }

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

#[derive(Debug, Clone, Copy)]
pub enum SupportedSamplingFrequency {
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

impl SupportedSamplingFrequency {
    /// Returns the bit position for this frequency
    pub fn bit_position(&self) -> u8 {
        *self as u8
    }
}

/// Encodes a collection of `SamplingFrequency` values into a `u16` bitfield.
pub fn encode_frequencies_to_bitfield(frequencies: &[SupportedSamplingFrequency]) -> u16 {
    let mut bitfield: u16 = 0;
    for frequency in frequencies {
        bitfield |= 1 << frequency.bit_position();
    }
    bitfield
}

/// Sink PAC characteristic containing one or more PAC records.
pub struct SinkPAC<'a> {
    pub number_of_pac_records: u8,        // Number of PAC records
    pub pac_records: &'a [PACRecord<'a>], // Array of PAC records
}

#[derive(Default, PartialEq, Eq)]
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
pub struct SinkAudioLocations(
    ///Device-wide bitmap of supported Audio Location values for
    /// all PAC records where the server supports reception of audio data.
    AudioLocation,
);

impl SinkAudioLocations {
    fn new(audio_location: AudioLocation) -> Self {
        Self(audio_location)
    }

    /// If the server detects that the Sink_Audio_Locations parameter value,
    /// written by a client by using the GATT Write Characteristic Value sub-procedure,
    /// is not 4 octets in length, or if the parameter value written
    /// includes any RFU bits set to a value of 0b1, the server shall
    /// respond with an ATT Error Response and shall set the Error Code parameter to
    /// Write Request Rejected as defined in [4].
    pub fn verify(&self, client_audio_location: AudioLocation) -> Result<(), Error> {
        if client_audio_location != self.0 {
            return Err(Error::Att(AttErrorCode::WriteNotPermitted));
        }
        Ok(())
    }
}

/// The Source PAC characteristic is used to expose PAC records when the server supports transmission of audio data.
pub struct SourcePAC<'a> {
    pub number_of_pac_records: u8,        // Number of PAC records
    pub pac_records: &'a [PACRecord<'a>], // Array of PAC records
}

/// The Source Audio Locations characteristic is used to expose the
/// supported Audio Locations when the server supports transmission of audio data.
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
    /// Write Request Rejected as defined in [4].
    pub fn verify(&self, client_audio_location: AudioLocation) -> Result<(), Error> {
        if client_audio_location != self.0 {
            return Err(Error::Att(AttErrorCode::WriteNotPermitted));
        }
        Ok(())
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
pub struct AvailableAudioContexts {
    // Bitmask of audio data Context Type values available for reception.
    available_sink_contexts: ContextType,
    // Bitmask of audio data Context Type values available for transmission.
    available_source_contexts: ContextType,
}

/// The Supported Audio Contexts characteristic exposes the server’s support for reception
/// and/or transmission of unicast audio data and/or broadcast audio data associated with specific Context Types.
pub struct SupportedAudioContexts {
    // Bitmask of audio data Context Type values available for reception.
    available_sink_contexts: ContextType,
    // Bitmask of audio data Context Type values available for transmission.
    available_source_contexts: ContextType,
}
