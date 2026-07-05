use super::{AudioLocation, Ltv};
use trouble_host::types::gatt_traits::FromGattError;

/// A single entry of the Codec_Specific_Configuration LTV structure.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
#[repr(u8)]
pub enum CodecSpecificConfiguration {
    SamplingFrequency(SamplingFrequency) = 1,
    FrameDuration(FrameDuration) = 2,
    AudioChannelAllocation(AudioLocation) = 3,
    /// The chosen codec frame size, in octets.
    OctetsPerCodecFrame(u16) = 4,
    CodecFrameBlocksPerSdu(u8) = 5,
}

impl Ltv for CodecSpecificConfiguration {
    fn ltv_type(&self) -> u8 {
        match self {
            Self::SamplingFrequency(_) => 1,
            Self::FrameDuration(_) => 2,
            Self::AudioChannelAllocation(_) => 3,
            Self::OctetsPerCodecFrame(_) => 4,
            Self::CodecFrameBlocksPerSdu(_) => 5,
        }
    }

    fn encode_value(&self, out: &mut alloc::vec::Vec<u8>) {
        match self {
            Self::SamplingFrequency(v) => out.push(*v as u8),
            Self::FrameDuration(v) => out.push(*v as u8),
            Self::AudioChannelAllocation(v) => out.extend_from_slice(&v.bits().to_le_bytes()),
            Self::OctetsPerCodecFrame(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::CodecFrameBlocksPerSdu(v) => out.push(*v),
        }
    }

    fn decode(ty: u8, value: &[u8]) -> Result<Self, FromGattError> {
        match ty {
            1 => Ok(Self::SamplingFrequency(SamplingFrequency::try_from(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )?)),
            2 => Ok(Self::FrameDuration(FrameDuration::try_from(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )?)),
            3 => {
                let bytes: [u8; 4] = value.try_into().map_err(|_| FromGattError::InvalidLength)?;
                Ok(Self::AudioChannelAllocation(AudioLocation::from_bits_truncate(
                    u32::from_le_bytes(bytes),
                )))
            }
            4 => {
                let bytes: [u8; 2] = value.try_into().map_err(|_| FromGattError::InvalidLength)?;
                Ok(Self::OctetsPerCodecFrame(u16::from_le_bytes(bytes)))
            }
            5 => Ok(Self::CodecFrameBlocksPerSdu(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
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

impl TryFrom<u8> for SamplingFrequency {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Hz8000),
            1 => Ok(Self::Hz11025),
            2 => Ok(Self::Hz16000),
            3 => Ok(Self::Hz22050),
            4 => Ok(Self::Hz24000),
            5 => Ok(Self::Hz32000),
            6 => Ok(Self::Hz44100),
            7 => Ok(Self::Hz48000),
            8 => Ok(Self::Hz88200),
            9 => Ok(Self::Hz96000),
            10 => Ok(Self::Hz176400),
            11 => Ok(Self::Hz192000),
            12 => Ok(Self::Hz384000),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameDuration {
    Duration7_5MS = 0,
    #[default]
    Duration10MS = 1,
}

impl TryFrom<u8> for FrameDuration {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Duration7_5MS),
            1 => Ok(Self::Duration10MS),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}
