//! Generic Audio structures
//!
use bitflags::bitflags;

use core::{mem::transmute, slice};
use trouble_host::{prelude::*, types::gatt_traits::*};

mod metadata;
pub use metadata::*;

mod capabilities;
pub use capabilities::*;

mod configuration;
pub use configuration::*;

bitflags! {
    #[derive(Default, Debug, Clone, Copy)]
    pub struct AudioLocation: u32 {
        const Mono = 0x00000000; // Mono Audio (no specified Audio Location)
        const FrontLeft = 0x00000001;
        const FrontRight = 0x00000002;
        const FrontCenter = 0x00000004;
        const LowFrequencyEffects1 = 0x00000008;
        const BackLeft = 0x00000010;
        const BackRight = 0x00000020;
        const FrontLeftOfCenter = 0x00000040;
        const FrontRightOfCenter = 0x00000080;
        const BackCenter = 0x00000100;
        const LowFrequencyEffects2 = 0x00000200;
        const SideLeft = 0x00000400;
        const SideRight = 0x00000800;
        const TopFrontLeft = 0x00001000;
        const TopFrontRight = 0x00002000;
        const TopFrontCenter = 0x00004000;
        const TopCenter = 0x00008000;
        const TopBackLeft = 0x00010000;
        const TopBackRight = 0x00020000;
        const TopSideLeft = 0x00040000;
        const TopSideRight = 0x00080000;
        const TopBackCenter = 0x00100000;
        const BottomFrontCenter = 0x00200000;
        const BottomFrontLeft = 0x00400000;
        const BottomFrontRight = 0x00800000;
        const FrontLeftWide = 0x01000000;
        const FrontRightWide = 0x02000000;
        const LeftSurround = 0x04000000;
        const RightSurround = 0x08000000;
    }
}

impl FixedGattValue for AudioLocation {
    const SIZE: usize = size_of::<Self>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        #[cfg(feature = "defmt")]
        defmt::info!("Gatt len: {}, data: {:?}", data.len(), data);
        unsafe {
            Ok(transmute::<u32, AudioLocation>(
                <u32 as trouble_host::prelude::FixedGattValue>::from_gatt(data)?,
            ))
        }
    }

    fn as_gatt(&self) -> &[u8] {
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
    Notifications = 0x0100,
    Ringtone = 0x0200,
    Alerts = 0x0400,
    Alarm = 0x0800,
    Undefined,
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone)]
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
