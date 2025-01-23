//! Generic Audio structures
//!

use core::ops::Deref;
use core::slice;
use heapless::Vec;
use postcard::{from_bytes, to_vec};
use serde::{Deserialize, Serialize};
use trouble_host::{prelude::*, types::gatt_traits::*};

mod metadata;
pub use metadata::*;

mod capabilities;
pub use capabilities::*;

mod configuration;
pub use configuration::*;

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
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
}

impl FixedGattValue for AudioLocation {
    const SIZE: usize = size_of::<AudioLocation>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            Ok(from_bytes(data).unwrap())
        }
    }

    fn to_gatt(&self) -> &[u8] {
        // to_vec::<AudioLocation, { Self::SIZE }>(self).unwrap()
        // SAFETY
        // - Slice is of type u8 so data is guaranteed valid for reads of any length
        // - Data and len are tied to the address and size of the type
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
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
}

impl Into<u8> for AudioInputType {
    fn into(self) -> u8 {
        self as u8
    }
}

impl From<u8> for AudioInputType {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::Unspecified,
            0x01 => Self::Bluetooth,
            0x02 => Self::Microphone,
            0x03 => Self::Analog,
            0x04 => Self::Digital,
            0x05 => Self::Radio,
            0x06 => Self::Streaming,
            0x07 => Self::Ambient,
            _ => Self::default(),
        }
    }
}

/// A bitfield of values that, when set to 0b1 for a bit,
/// describes audio data as being intended for the use case represented by that bit.
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test() {
        // let audio = AudioLocation::default().to_gatt();
        let a = AudioLocation::RightSurround;
        let audio = trouble_host::prelude::FixedGattValue::to_gatt(&a);
        println!("{:?}", audio);
        assert_eq!(
            AudioLocation::RightSurround,
            <AudioLocation as FixedGattValue>::from_gatt(audio).unwrap()
        );
    }
}
