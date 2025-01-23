use super::{OctetsPerCodecFrame, SamplingFrequency};

#[derive(Debug)]
#[repr(u8)]
pub enum CodecSpecificCapabilities {
    SupportedSamplingFrequencies(SupportedSamplingFrequencies) = 1,
    SupportedFrameDurations(SupportedFrameDurations) = 2,
    SupportedAudioChannelCounts(SupportedAudioChannelCounts) = 3,
    SupportedOctetsPerCodecFrame(OctetsPerCodecFrame) = 4,
    SupportedMaxCodecFramesPerSDU(u8) = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportedSamplingFrequencies(u8);

impl Default for SupportedSamplingFrequencies {
    fn default() -> Self {
        Self(1 << SamplingFrequency::default() as u8)
    }
}

impl SupportedSamplingFrequencies {
    pub fn new(frequencies: &[SamplingFrequency]) -> Self {
        let mut sampling_frequencies = 0;
        for frequency in frequencies {
            Self::add(&mut sampling_frequencies, *frequency)
        }
        SupportedSamplingFrequencies(sampling_frequencies)
    }

    pub fn add(frequencies: &mut u8, sampling_frequency: SamplingFrequency) {
        *frequencies += 1 << sampling_frequency as u8;
    }
}

impl From<u8> for SupportedSamplingFrequencies {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

impl Into<u8> for SupportedSamplingFrequencies {
    fn into(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportedFrameDurations(u8);

impl SupportedFrameDurations {
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

impl Default for SupportedFrameDurations {
    fn default() -> Self {
        Self::new(false, true, false, false)
    }
}

impl From<u8> for SupportedFrameDurations {
    fn from(value: u8) -> Self {
        SupportedFrameDurations(value)
    }
}

impl Into<u8> for SupportedFrameDurations {
    fn into(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
pub struct SupportedAudioChannelCounts(u8);

impl SupportedAudioChannelCounts {
    pub fn new(count: u8) -> Self {
        let mut value = 0;

        if count >= 1 && count <= 8 {
            value |= 1 << (count - 1);
        }

        Self(value)
    }
}

impl Into<u8> for SupportedAudioChannelCounts {
    fn into(self) -> u8 {
        self.0
    }
}

impl From<u8> for SupportedAudioChannelCounts {
    fn from(value: u8) -> Self {
        Self(value)
    }
}
