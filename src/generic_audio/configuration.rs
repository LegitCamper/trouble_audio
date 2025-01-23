use super::{AudioLocation, OctetsPerCodecFrame};

#[derive(Debug)]
#[repr(u8)]
pub enum CodecSpecificConfiguration {
    SamplingFrequency(SamplingFrequency) = 1,
    FrameDuration(FrameDuration) = 2,
    AudioChannelAllocation(AudioLocation) = 3,
    OctetsPerCodecFrame(OctetsPerCodecFrame) = 4,
}

#[derive(Default, Debug, Clone, Copy)]
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

#[derive(Default, Debug)]
#[repr(u8)]
pub enum FrameDuration {
    Duration7_5MS = 0,
    #[default]
    Duration10MS = 1,
}
