//! Named LC3 broadcast presets from BAP Table 6.4 and GMAP Table 3.1.
//!
//! Hardware conformance needs more than one configuration: the media presets (10 ms SDUs, high
//! transport latency, several retransmissions) and the gaming presets (7.5 ms SDUs, single-digit
//! transport latency, few retransmissions) exercise different controller scheduling paths. Keeping
//! them as data lets one firmware image be built per preset and lets tests name the preset instead
//! of restating six fields.

use crate::generic_audio::{FrameDuration, SamplingFrequency};

/// One LC3 stream configuration: codec parameters plus the QoS the BIG/CIG is created with.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPreset {
    /// Preset name as written in BAP/GMAP (e.g. `"48_2_1"`, `"32_1_gs"`).
    pub name: &'static str,
    pub sampling_frequency: SamplingFrequency,
    pub frame_duration: FrameDuration,
    /// Octets per codec frame, also used as Max_SDU for a single-frame-per-SDU stream.
    pub octets_per_frame: u16,
    /// Retransmission number requested for the BIG/CIS.
    pub rtn: u8,
    pub max_transport_latency_ms: u16,
    pub presentation_delay_us: u32,
}

impl AudioPreset {
    /// SDU interval implied by the frame duration, microseconds.
    pub const fn sdu_interval_us(&self) -> u32 {
        match self.frame_duration {
            FrameDuration::Duration7_5MS => 7_500,
            FrameDuration::Duration10MS => 10_000,
        }
    }

    /// PCM samples per codec frame, i.e. the LC3 encoder/decoder frame size.
    pub const fn samples_per_frame(&self) -> usize {
        let hz = self.sampling_rate_hz() as usize;
        match self.frame_duration {
            // 7.5 ms and 10 ms both divide every supported LC3 rate exactly.
            FrameDuration::Duration7_5MS => hz * 75 / 10_000,
            FrameDuration::Duration10MS => hz / 100,
        }
    }

    /// Sampling rate in Hz.
    pub const fn sampling_rate_hz(&self) -> u32 {
        match self.sampling_frequency {
            SamplingFrequency::Hz8000 => 8_000,
            SamplingFrequency::Hz11025 => 11_025,
            SamplingFrequency::Hz16000 => 16_000,
            SamplingFrequency::Hz22050 => 22_050,
            SamplingFrequency::Hz24000 => 24_000,
            SamplingFrequency::Hz32000 => 32_000,
            SamplingFrequency::Hz44100 => 44_100,
            SamplingFrequency::Hz48000 => 48_000,
            SamplingFrequency::Hz88200 => 88_200,
            SamplingFrequency::Hz96000 => 96_000,
            SamplingFrequency::Hz176400 => 176_400,
            SamplingFrequency::Hz192000 => 192_000,
            SamplingFrequency::Hz384000 => 384_000,
        }
    }
}

/// BAP broadcast media preset 48_2_1: 48 kHz, 10 ms, high-reliability media streaming.
pub const BROADCAST_48_2_1: AudioPreset = AudioPreset {
    name: "48_2_1",
    sampling_frequency: SamplingFrequency::Hz48000,
    frame_duration: FrameDuration::Duration10MS,
    octets_per_frame: 100,
    rtn: 2,
    max_transport_latency_ms: 20,
    presentation_delay_us: 40_000,
};

/// BAP broadcast media preset 48_1_1: 48 kHz, 7.5 ms.
pub const BROADCAST_48_1_1: AudioPreset = AudioPreset {
    name: "48_1_1",
    sampling_frequency: SamplingFrequency::Hz48000,
    frame_duration: FrameDuration::Duration7_5MS,
    octets_per_frame: 75,
    rtn: 2,
    max_transport_latency_ms: 15,
    presentation_delay_us: 40_000,
};

/// GMAP broadcast gaming preset 48_1_gs: 48 kHz, 7.5 ms, low transport latency.
pub const BROADCAST_48_1_GS: AudioPreset = AudioPreset {
    name: "48_1_gs",
    sampling_frequency: SamplingFrequency::Hz48000,
    frame_duration: FrameDuration::Duration7_5MS,
    octets_per_frame: 75,
    rtn: 1,
    max_transport_latency_ms: 8,
    presentation_delay_us: 10_000,
};

/// GMAP broadcast gaming preset 32_1_gs: 32 kHz, 7.5 ms, lowest-bandwidth gaming configuration.
pub const BROADCAST_32_1_GS: AudioPreset = AudioPreset {
    name: "32_1_gs",
    sampling_frequency: SamplingFrequency::Hz32000,
    frame_duration: FrameDuration::Duration7_5MS,
    octets_per_frame: 60,
    rtn: 1,
    max_transport_latency_ms: 8,
    presentation_delay_us: 10_000,
};

/// Every preset this crate names, for matrix tests.
pub const BROADCAST_PRESETS: &[AudioPreset] = &[
    BROADCAST_48_2_1,
    BROADCAST_48_1_1,
    BROADCAST_48_1_GS,
    BROADCAST_32_1_GS,
];

/// Looks a preset up by its BAP/GMAP name.
pub fn broadcast_preset(name: &str) -> Option<AudioPreset> {
    BROADCAST_PRESETS
        .iter()
        .copied()
        .find(|preset| preset.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdu_interval_matches_frame_duration() {
        assert_eq!(BROADCAST_48_2_1.sdu_interval_us(), 10_000);
        assert_eq!(BROADCAST_48_1_GS.sdu_interval_us(), 7_500);
    }

    #[test]
    fn samples_per_frame_matches_lc3_frame_sizes() {
        assert_eq!(BROADCAST_48_2_1.samples_per_frame(), 480);
        assert_eq!(BROADCAST_48_1_GS.samples_per_frame(), 360);
        assert_eq!(BROADCAST_32_1_GS.samples_per_frame(), 240);
    }

    #[test]
    fn gaming_presets_are_lower_latency_than_media_presets() {
        for gaming in [BROADCAST_48_1_GS, BROADCAST_32_1_GS] {
            assert!(gaming.max_transport_latency_ms < BROADCAST_48_2_1.max_transport_latency_ms);
            assert!(gaming.presentation_delay_us < BROADCAST_48_2_1.presentation_delay_us);
            assert!(gaming.rtn <= BROADCAST_48_2_1.rtn);
        }
    }

    #[test]
    fn presets_are_found_by_name_and_unknown_names_are_rejected() {
        assert_eq!(broadcast_preset("48_1_gs"), Some(BROADCAST_48_1_GS));
        assert_eq!(broadcast_preset("nonexistent"), None);
    }

    #[test]
    fn preset_names_are_unique() {
        for (i, preset) in BROADCAST_PRESETS.iter().enumerate() {
            assert!(
                !BROADCAST_PRESETS[..i]
                    .iter()
                    .any(|other| other.name == preset.name),
                "duplicate preset name {}",
                preset.name
            );
        }
    }
}
