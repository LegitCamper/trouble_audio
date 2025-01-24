//! Generic Audio structures
//!

use core::{mem::transmute, slice};
use trouble_host::{prelude::*, types::gatt_traits::*};

mod metadata;
pub use metadata::*;

mod capabilities;
pub use capabilities::*;

mod configuration;
pub use configuration::*;

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
#[repr(u64)]
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

impl FixedGattValue for AudioLocation {
    const SIZE: usize = size_of::<AudioLocation>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe {
                Ok(transmute::<u64, AudioLocation>(u64::from_le_bytes(
                    data.try_into().expect("incorrect length"),
                )))
            }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub enum AudioInputType {
    #[default]
    Unspecified = 0x00, // Unspecified Input
    Bluetooth = 0x01,  // Bluetooth Audio Stream
    Microphone = 0x02, // Microphone
    Analog = 0x03,     // Analog Interface
    Digital = 0x04,    // Digital Interface
    Radio = 0x05,      // AM/FM/XM/etc.
    Streaming = 0x06,  // Streaming Audio Source
    Ambient = 0x07,    // Transparency/Pass-through
    Undefined,
}

/// A bitfield of values that, when set to 0b1 for a bit,
/// describes audio data as being intended for the use case represented by that bit.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug, Clone)]
#[repr(u16)]
pub enum ContextType {
    #[default]
    Prohibited = 0x0000,
    Unspecified = 0x0001,
    Conversational = 0x0002,
    Media = 0x0004,
    Game = 0x0008,
    Instructional = 0x0010,
    VoiceAssistants = 0x0020,
    Live = 0x0040,
    SoundEffects = 0x0080,
    Undefined,
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default)]
pub struct OctetsPerCodecFrame {
    min_octets: u16,
    max_octets: u16,
}

impl OctetsPerCodecFrame {
    pub fn new(min_octets: u16, max_octets: u16) -> Self {
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
