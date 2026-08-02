//! Generic Audio structures
//!
use bitflags::bitflags;

use trouble_host::types::gatt_traits::*;

mod ltv;
pub use ltv::*;

mod metadata;
pub use metadata::*;

mod capabilities;
pub use capabilities::*;

mod configuration;
pub use configuration::*;

bitflags! {
    /// Audio_Location bitfield: identifies the physical location(s) of an audio channel.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AudioLocation: u32 {
        const Mono = 0x00000000;
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

impl Default for AudioLocation {
    fn default() -> Self {
        Self::empty()
    }
}

impl AsGatt for AudioLocation {
    const MIN_SIZE: usize = 4;
    const MAX_SIZE: usize = 4;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `self.bits` is a plain u32; reinterpreting it as 4 little-endian
        // octets matches its in-memory representation on all supported targets.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 4) }
    }
}

impl FromGatt for AudioLocation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 4] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u32::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for AudioLocation {
    const SIZE: usize = 4;
}

#[cfg(feature = "defmt")]
impl defmt::Format for AudioLocation {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "AudioLocation({=u32:b})", self.bits())
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub enum AudioInputType {
    #[default]
    Unspecified = 0x00,
    Bluetooth = 0x01,
    Microphone = 0x02,
    Analog = 0x03,
    Digital = 0x04,
    Radio = 0x05,
    Streaming = 0x06,
    Ambient = 0x07,
    Undefined,
}

bitflags! {
    /// Context_Type bitfield: describes audio data as being intended for the use case(s)
    /// represented by each set bit. Multiple bits may be set simultaneously.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ContextType: u16 {
        const Unspecified = 0x0001;
        const Conversational = 0x0002;
        const Media = 0x0004;
        const Game = 0x0008;
        const Instructional = 0x0010;
        const VoiceAssistants = 0x0020;
        const Live = 0x0040;
        const SoundEffects = 0x0080;
        const Notifications = 0x0100;
        const Ringtone = 0x0200;
        const Alerts = 0x0400;
        const Alarm = 0x0800;
    }
}

impl Default for ContextType {
    fn default() -> Self {
        Self::empty()
    }
}

impl AsGatt for ContextType {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 2;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u16 here),
        // so reinterpreting as 2 little-endian octets matches the in-memory representation.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 2) }
    }
}

impl FromGatt for ContextType {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 2] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u16::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for ContextType {
    const SIZE: usize = 2;
}

#[cfg(feature = "defmt")]
impl defmt::Format for ContextType {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "ContextType({=u16:b})", self.bits())
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone)]
pub struct OctetsPerCodecFrame {
    min_octets: u16,
    max_octets: u16,
}

impl OctetsPerCodecFrame {
    /// Creates a new min/max octets-per-codec-frame range.
    pub fn new(min_octets: u16, max_octets: u16) -> Self {
        Self {
            min_octets,
            max_octets,
        }
    }

    /// Encodes as 4 octets: min (2, LE) then max (2, LE).
    pub fn to_le_bytes(&self) -> [u8; 4] {
        let mut out = [0u8; 4];
        out[0..2].copy_from_slice(&self.min_octets.to_le_bytes());
        out[2..4].copy_from_slice(&self.max_octets.to_le_bytes());
        out
    }

    /// Decodes from 4 octets: min (2, LE) then max (2, LE).
    pub fn from_le_bytes(bytes: [u8; 4]) -> Self {
        let min_octets = u16::from_le_bytes([bytes[0], bytes[1]]);
        let max_octets = u16::from_le_bytes([bytes[2], bytes[3]]);
        Self::new(min_octets, max_octets)
    }
}
