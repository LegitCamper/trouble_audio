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
            // Sampling_Frequency's wire values are 1-indexed, unlike this enum's discriminants
            // (which are 0-indexed to match `SupportedSamplingFrequencies`' bit positions).
            Self::SamplingFrequency(v) => out.push(*v as u8 + 1),
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

    /// `value` is 1-indexed on the wire (a real central sends `0x08` for 48 kHz) - shift down by
    /// 1 to match this enum's 0-indexed discriminants.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value.checked_sub(1) {
            Some(0) => Ok(Self::Hz8000),
            Some(1) => Ok(Self::Hz11025),
            Some(2) => Ok(Self::Hz16000),
            Some(3) => Ok(Self::Hz22050),
            Some(4) => Ok(Self::Hz24000),
            Some(5) => Ok(Self::Hz32000),
            Some(6) => Ok(Self::Hz44100),
            Some(7) => Ok(Self::Hz48000),
            Some(8) => Ok(Self::Hz88200),
            Some(9) => Ok(Self::Hz96000),
            Some(10) => Ok(Self::Hz176400),
            Some(11) => Ok(Self::Hz192000),
            Some(12) => Ok(Self::Hz384000),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `0x08` is the literal byte a real central sends for 48 kHz - anchoring to that catches an
    /// indexing mismatch that a pure encode/decode round trip wouldn't.
    #[test]
    fn sampling_frequency_decodes_the_wire_value_a_real_central_sends_for_48khz() {
        assert_eq!(SamplingFrequency::try_from(0x08).unwrap(), SamplingFrequency::Hz48000);
    }

    #[test]
    fn sampling_frequency_encode_decode_round_trips_to_the_spec_wire_value() {
        let mut out = alloc::vec::Vec::new();
        CodecSpecificConfiguration::SamplingFrequency(SamplingFrequency::Hz48000).encode_value(&mut out);
        assert_eq!(out, alloc::vec![0x08]);
    }
}
