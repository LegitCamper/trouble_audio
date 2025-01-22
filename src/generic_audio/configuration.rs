use super::{AudioLocation, OctetsPerCodecFrame};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecSpecificConfiguration {
    SamplingFrequency(SamplingFrequency),
    FrameDuration(FrameDuration),
    AudioChannelAllocation(AudioLocation),
    OctetsPerCodecFrame(OctetsPerCodecFrame),
}

impl CodecSpecificConfiguration {
    pub(crate) fn as_type(&self) -> u8 {
        match self {
            Self::SamplingFrequency(_) => 1,
            Self::FrameDuration(_) => 2,
            Self::AudioChannelAllocation(_) => 3,
            Self::OctetsPerCodecFrame(_) => 4,
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
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
    Undefined,
}

impl SamplingFrequency {
    pub(crate) fn bit_position(&self) -> u8 {
        *self as u8
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub enum FrameDuration {
    Duration7_5MS = 0,
    #[default]
    Duration10MS = 1,
    Undefined,
}

impl From<u8> for FrameDuration {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Duration7_5MS,
            1 => Self::Duration10MS,
            _ => Self::Undefined,
        }
    }
}

impl Into<u8> for FrameDuration {
    fn into(self) -> u8 {
        self as u8
    }
}
