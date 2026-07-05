use super::{Ltv, OctetsPerCodecFrame, SamplingFrequency};
use trouble_host::types::gatt_traits::FromGattError;

/// A single entry of the Codec_Specific_Capabilities LTV structure.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
#[repr(u8)]
pub enum CodecSpecificCapabilities {
    SupportedSamplingFrequencies(SupportedSamplingFrequencies) = 1,
    SupportedFrameDurations(SupportedFrameDurations) = 2,
    SupportedAudioChannelCounts(SupportedAudioChannelCounts) = 3,
    SupportedOctetsPerCodecFrame(OctetsPerCodecFrame) = 4,
    SupportedMaxCodecFramesPerSDU(u8) = 5,
}

impl Ltv for CodecSpecificCapabilities {
    fn ltv_type(&self) -> u8 {
        match self {
            Self::SupportedSamplingFrequencies(_) => 1,
            Self::SupportedFrameDurations(_) => 2,
            Self::SupportedAudioChannelCounts(_) => 3,
            Self::SupportedOctetsPerCodecFrame(_) => 4,
            Self::SupportedMaxCodecFramesPerSDU(_) => 5,
        }
    }

    fn encode_value(&self, out: &mut alloc::vec::Vec<u8>) {
        match self {
            Self::SupportedSamplingFrequencies(v) => out.extend_from_slice(&v.0.to_le_bytes()),
            Self::SupportedFrameDurations(v) => out.push(v.0),
            Self::SupportedAudioChannelCounts(v) => out.push(v.0),
            Self::SupportedOctetsPerCodecFrame(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::SupportedMaxCodecFramesPerSDU(v) => out.push(*v),
        }
    }

    fn decode(ty: u8, value: &[u8]) -> Result<Self, FromGattError> {
        match ty {
            1 => {
                let bytes: [u8; 2] = value.try_into().map_err(|_| FromGattError::InvalidLength)?;
                Ok(Self::SupportedSamplingFrequencies(SupportedSamplingFrequencies(
                    u16::from_le_bytes(bytes),
                )))
            }
            2 => Ok(Self::SupportedFrameDurations(SupportedFrameDurations(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            ))),
            3 => Ok(Self::SupportedAudioChannelCounts(SupportedAudioChannelCounts(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            ))),
            4 => {
                let bytes: [u8; 4] = value.try_into().map_err(|_| FromGattError::InvalidLength)?;
                Ok(Self::SupportedOctetsPerCodecFrame(OctetsPerCodecFrame::from_le_bytes(bytes)))
            }
            5 => Ok(Self::SupportedMaxCodecFramesPerSDU(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// Supported_Sampling_Frequencies: a bitfield, one bit per [`SamplingFrequency`] value.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedSamplingFrequencies(u16);

impl Default for SupportedSamplingFrequencies {
    fn default() -> Self {
        Self(1 << SamplingFrequency::default() as u16)
    }
}

impl SupportedSamplingFrequencies {
    /// Builds a bitfield with the given frequencies marked as supported.
    pub fn new(frequencies: &[SamplingFrequency]) -> Self {
        let mut sampling_frequencies = Self(0);
        for frequency in frequencies {
            sampling_frequencies.add(*frequency)
        }
        sampling_frequencies
    }

    /// Marks a frequency as supported.
    pub fn add(&mut self, sampling_frequency: SamplingFrequency) {
        self.0 |= 1 << sampling_frequency as u16;
    }

    /// Whether a frequency is marked as supported.
    pub fn supports(&self, sampling_frequency: SamplingFrequency) -> bool {
        self.0 & (1 << sampling_frequency as u16) != 0
    }
}

/// Supported_Frame_Durations: a bitfield describing which frame durations are supported,
/// and which is preferred when both are.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedFrameDurations(u8);

impl SupportedFrameDurations {
    /// Builds a bitfield from which durations are supported and, if both are, which is preferred.
    pub fn new(support_7_5_ms: bool, support_10_ms: bool, prefer_7_5_ms: bool, prefer_10_ms: bool) -> Self {
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

impl Default for SupportedFrameDurations {
    fn default() -> Self {
        Self::new(false, true, false, false)
    }
}

/// Supported_Audio_Channel_Counts: a bitfield, bit N-1 set means N channels are supported.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedAudioChannelCounts(u8);

impl SupportedAudioChannelCounts {
    /// Marks `count` channels as supported (1-8; out of range sets no bits).
    pub fn new(count: u8) -> Self {
        let mut value = 0;

        if (1..=8).contains(&count) {
            value |= 1 << (count - 1);
        }

        Self(value)
    }
}
