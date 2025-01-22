//! Generic Audio structures
//!

use core::slice;
use trouble_host::{prelude::*, types::gatt_traits::*, Error};

use crate::CodecdId;

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
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
    Undefined,
}

impl AudioLocation {
    /// If the server detects that the Source_Audio_Locations parameter value,
    /// written by a client by using the GATT Write Characteristic Value sub-procedure,
    /// is not 4 octets in length, or if the parameter value written
    /// includes any RFU bits set to a value of 0b1, the server shall respond
    /// with an ATT Error Response and shall set the Error Code parameter to
    pub fn verify(&self, client_audio_location: AudioLocation) -> Result<(), Error> {
        if client_audio_location != *self {
            return Err(Error::Att(AttErrorCode::WRITE_REQUEST_REJECTED));
        }
        Ok(())
    }
}

impl FixedGattValue for AudioLocation {
    const SIZE: usize = 4;

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            let value = (data[0] as u32)
                | ((data[1] as u32) << 8)
                | ((data[2] as u32) << 16)
                | ((data[3] as u32) << 24);
            match value {
                0x00000000 => Ok(AudioLocation::Mono),
                0x00000001 => Ok(AudioLocation::FrontLeft),
                0x00000002 => Ok(AudioLocation::FrontRight),
                0x00000004 => Ok(AudioLocation::FrontCenter),
                0x00000008 => Ok(AudioLocation::LowFrequencyEffects1),
                0x00000010 => Ok(AudioLocation::BackLeft),
                0x00000020 => Ok(AudioLocation::BackRight),
                0x00000040 => Ok(AudioLocation::FrontLeftOfCenter),
                0x00000080 => Ok(AudioLocation::FrontRightOfCenter),
                0x00000100 => Ok(AudioLocation::BackCenter),
                0x00000200 => Ok(AudioLocation::LowFrequencyEffects2),
                0x00000400 => Ok(AudioLocation::SideLeft),
                0x00000800 => Ok(AudioLocation::SideRight),
                0x00001000 => Ok(AudioLocation::TopFrontLeft),
                0x00002000 => Ok(AudioLocation::TopFrontRight),
                0x00004000 => Ok(AudioLocation::TopFrontCenter),
                0x00008000 => Ok(AudioLocation::TopCenter),
                0x00010000 => Ok(AudioLocation::TopBackLeft),
                0x00020000 => Ok(AudioLocation::TopBackRight),
                0x00040000 => Ok(AudioLocation::TopSideLeft),
                0x00080000 => Ok(AudioLocation::TopSideRight),
                0x00100000 => Ok(AudioLocation::TopBackCenter),
                0x00200000 => Ok(AudioLocation::BottomFrontCenter),
                0x00400000 => Ok(AudioLocation::BottomFrontLeft),
                0x00800000 => Ok(AudioLocation::BottomFrontRight),
                0x01000000 => Ok(AudioLocation::FrontLeftWide),
                0x02000000 => Ok(AudioLocation::FrontRightWide),
                0x04000000 => Ok(AudioLocation::LeftSurround),
                0x08000000 => Ok(AudioLocation::RightSurround),
                _ => Ok(AudioLocation::Undefined),
            }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        // SAFETY
        // - Slice is of type u8 so data is guaranteed valid for reads of any length
        // - Data and len are tied to the address and size of the type
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

pub enum AudioInputType {
    Unspecified = 0x00, // Unspecified Input
    Bluetooth = 0x01,   // Bluetooth Audio Stream
    Microphone = 0x02,  // Microphone
    Analog = 0x03,      // Analog Interface
    Digital = 0x04,     // Digital Interface
    Radio = 0x05,       // AM/FM/XM/etc.
    Streaming = 0x06,   // Streaming Audio Source
    Ambient = 0x07,     // Transparency/Pass-through
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecSpecificCapabilities {
    SupportedSamplingFrequencies(SamplingFrequencies),
    SupportedFrameDurations(FrameDurations),
    SupportedAudioChannelCounts(AudioChannelCounts),
    SupportedOctetsPerCodecFrame(OctetsPerCodecFrame),
    SupportedMaxCodecFramesPerSDU(u8),
}

impl CodecSpecificCapabilities {
    pub fn as_type(&self) -> u8 {
        match self {
            Self::SupportedSamplingFrequencies(_) => 1,
            Self::SupportedFrameDurations(_) => 2,
            Self::SupportedAudioChannelCounts(_) => 3,
            Self::SupportedOctetsPerCodecFrame(_) => 4,
            Self::SupportedMaxCodecFramesPerSDU(_) => 5,
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingFrequencies(u8);

impl Default for SamplingFrequencies {
    fn default() -> Self {
        Self(1 << SamplingFrequency::default().bit_position())
    }
}

impl SamplingFrequencies {
    pub fn new(frequencies: &[SamplingFrequency]) -> Self {
        let mut sampling_frequencies = 0;
        for frequency in frequencies {
            Self::add(&mut sampling_frequencies, *frequency)
        }
        SamplingFrequencies(sampling_frequencies)
    }

    pub fn add(frequencies: &mut u8, sampling_frequency: SamplingFrequency) {
        *frequencies += 1 << sampling_frequency.bit_position();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameDurations(u8);

impl FrameDurations {
    pub fn new(
        support_7_5_ms: bool,
        support_10_ms: bool,
        prefer_7_5_ms: bool,
        prefer_10_ms: bool,
    ) -> Self {
        let mut value = 0;
        if support_7_5_ms {
            value |= 0b0000_0001; // Set bit 0
        }
        if support_10_ms {
            value |= 0b0000_0010; // Set bit 1
        }
        if support_7_5_ms && support_10_ms && prefer_7_5_ms {
            value |= 0b0001_0000; // Set bit 4
        }
        if support_7_5_ms && support_10_ms && prefer_10_ms {
            value |= 0b0010_0000; // Set bit 5
        }

        Self(value)
    }
}

impl Default for FrameDurations {
    fn default() -> Self {
        Self::new(false, true, false, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
pub struct AudioChannelCounts(u8);

impl AudioChannelCounts {
    /// Creates a new `SupportedAudioChannelCounts` instance.
    ///
    /// - `channels`: A slice of `u8` values representing the supported channel counts (1 to 8).
    ///
    /// Returns a `AudioChannelCounts` struct.
    pub fn new(channels: &[u8]) -> Self {
        let mut value = 0;

        for &channel in channels {
            if channel >= 1 && channel <= 8 {
                value |= 1 << (channel - 1);
            }
        }

        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct OctetsPerCodecFrame {
    min_octets: u16,
    max_octets: u16,
}

impl OctetsPerCodecFrame {
    pub fn new(min_octets: u16, max_octets: u16) -> Self {
        if min_octets > max_octets {
            defmt::panic!("min_octets cannot be greater than max_octets");
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
