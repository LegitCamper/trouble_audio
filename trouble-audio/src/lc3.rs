//! Thin, stateful wrapper around the `lc3-codec` crate's `Lc3Encoder`/`Lc3Decoder`, parameterized
//! from a negotiated Codec_Specific_Configuration (Sampling_Frequency, Frame_Duration).
//!
//! `lc3-codec`'s encoder/decoder hold state across frames and borrow their scratch buffers rather
//! than owning them, so this wrapper leaks its buffers (`Box::leak`) to give them `'static`
//! lifetime for the session's duration.
//!
//! # Sizing a `#[global_allocator]` heap
//!
//! [`Lc3MonoEncoder::heap_bytes`]/[`Lc3MonoDecoder::heap_bytes`] are `const fn`, so a binary that
//! knows its topology at compile time (sampling frequency/frame duration, how many concurrent
//! encoders/decoders it builds) can turn "is `HEAP_SIZE` big enough" into a build-time assertion
//! instead of a runtime `handle_alloc_error` panic:
//!
//! ```ignore
//! const STEREO_DECODER_BYTES: usize =
//!     2 * match Lc3MonoDecoder::heap_bytes(SamplingFrequency::Hz48000, FrameDuration::Duration10MS) {
//!         Ok(n) => n,
//!         Err(_) => panic!("unsupported sampling frequency"),
//!     };
//! const _: () = assert!(HEAP_SIZE >= STEREO_DECODER_BYTES, "HEAP_SIZE too small for two LC3 decoders");
//! ```

use alloc::boxed::Box;
use alloc::vec;
use core::mem::size_of;
use lc3_codec::common::complex::{Complex, Scaler};
use lc3_codec::common::config::{FrameDuration as Lc3FrameDuration, Lc3Config, SamplingFrequency as Lc3SamplingFrequency};
pub use lc3_codec::decoder::lc3_decoder::Lc3DecoderError;
use lc3_codec::decoder::lc3_decoder::Lc3Decoder;
pub use lc3_codec::encoder::lc3_encoder::Lc3EncoderError;
use lc3_codec::encoder::lc3_encoder::Lc3Encoder;

use crate::generic_audio::{FrameDuration, SamplingFrequency};

/// This crate's [`SamplingFrequency`]/[`FrameDuration`] cover the full set BAP allows;
/// `lc3-codec` only implements a subset of sampling frequencies (and all frame durations).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedSamplingFrequency(pub SamplingFrequency);

/// `lc3-codec`'s `Lc3Decoder`/`Lc3Encoder` also allocate a small internal `Vec<Channel>` that
/// can't be sized exactly (the `Channel` type is private), on top of the scratch buffers below -
/// a generous upper bound instead (measured ~1056 bytes decoder / ~5320 bytes encoder on a
/// 64-bit host; safe on 32-bit embedded targets, where narrower pointers only shrink it further).
const DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES: usize = 2048;
/// See [`DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES`].
const ENCODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES: usize = 8192;

const fn to_lc3_sampling_frequency(value: SamplingFrequency) -> Result<Lc3SamplingFrequency, UnsupportedSamplingFrequency> {
    match value {
        SamplingFrequency::Hz8000 => Ok(Lc3SamplingFrequency::Hz8000),
        SamplingFrequency::Hz16000 => Ok(Lc3SamplingFrequency::Hz16000),
        SamplingFrequency::Hz24000 => Ok(Lc3SamplingFrequency::Hz24000),
        SamplingFrequency::Hz32000 => Ok(Lc3SamplingFrequency::Hz32000),
        SamplingFrequency::Hz44100 => Ok(Lc3SamplingFrequency::Hz44100),
        SamplingFrequency::Hz48000 => Ok(Lc3SamplingFrequency::Hz48000),
        other => Err(UnsupportedSamplingFrequency(other)),
    }
}

const fn to_lc3_frame_duration(value: FrameDuration) -> Lc3FrameDuration {
    match value {
        FrameDuration::Duration7_5MS => Lc3FrameDuration::SevenPointFiveMs,
        FrameDuration::Duration10MS => Lc3FrameDuration::TenMs,
    }
}

/// A single-channel (mono) LC3 encoder. One ASE/CIS carries one audio channel, so this is the
/// unit of encoding state LE Audio unicast needs - stereo is two of these, one per ASE.
pub struct Lc3MonoEncoder {
    encoder: Lc3Encoder<'static>,
    /// Number of PCM samples [`Self::encode`] expects per call (`Lc3Config::nf`).
    pub samples_per_frame: usize,
}

impl Lc3MonoEncoder {
    /// Bytes [`Self::new`] will allocate for the given negotiated Sampling_Frequency/Frame_Duration
    /// - see the module docs for using this to compile-time-assert a heap is big enough. A safe
    /// upper bound, not byte-exact (see [`ENCODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES`]).
    pub const fn heap_bytes(sampling_frequency: SamplingFrequency, frame_duration: FrameDuration) -> Result<usize, UnsupportedSamplingFrequency> {
        let fs = match to_lc3_sampling_frequency(sampling_frequency) {
            Ok(fs) => fs,
            Err(e) => return Err(e),
        };
        let n_ms = to_lc3_frame_duration(frame_duration);
        let (integer_len, scaler_len, complex_len) = Lc3Encoder::calc_working_buffer_lengths(1, n_ms, fs);
        Ok(integer_len * size_of::<i16>()
            + scaler_len * size_of::<Scaler>()
            + complex_len * size_of::<Complex>()
            + ENCODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES)
    }

    /// Builds an encoder for the given negotiated Sampling_Frequency/Frame_Duration.
    pub fn new(
        sampling_frequency: SamplingFrequency,
        frame_duration: FrameDuration,
    ) -> Result<Self, UnsupportedSamplingFrequency> {
        let fs = to_lc3_sampling_frequency(sampling_frequency)?;
        let n_ms = to_lc3_frame_duration(frame_duration);
        let (integer_len, scaler_len, complex_len) = Lc3Encoder::calc_working_buffer_lengths(1, n_ms, fs);
        let integer_buf = Box::leak(vec![0i16; integer_len].into_boxed_slice());
        let scaler_buf = Box::leak(vec![0.0; scaler_len].into_boxed_slice());
        let complex_buf = Box::leak(vec![Default::default(); complex_len].into_boxed_slice());
        let encoder = Lc3Encoder::new(1, n_ms, fs, integer_buf, scaler_buf, complex_buf);
        Ok(Self {
            encoder,
            samples_per_frame: Lc3Config::new(fs, n_ms).nf,
        })
    }

    /// Encodes `samples_per_frame` PCM samples into `out`. `out`'s length picks the codec frame
    /// size, i.e. the negotiated Octets_Per_Codec_Frame.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<(), Lc3EncoderError> {
        self.encoder.encode_frame(0, pcm, out)
    }
}

/// A single-channel (mono) LC3 decoder. See [`Lc3MonoEncoder`] for why mono is the unit here.
pub struct Lc3MonoDecoder {
    decoder: Lc3Decoder<'static>,
    /// Number of PCM samples [`Self::decode`] produces per call (`Lc3Config::nf`).
    pub samples_per_frame: usize,
}

impl Lc3MonoDecoder {
    /// Bytes [`Self::new`] will allocate for the given negotiated Sampling_Frequency/Frame_Duration
    /// - see the module docs for using this to compile-time-assert a heap is big enough. A safe
    /// upper bound, not byte-exact (see [`DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES`]).
    pub const fn heap_bytes(sampling_frequency: SamplingFrequency, frame_duration: FrameDuration) -> Result<usize, UnsupportedSamplingFrequency> {
        let fs = match to_lc3_sampling_frequency(sampling_frequency) {
            Ok(fs) => fs,
            Err(e) => return Err(e),
        };
        let n_ms = to_lc3_frame_duration(frame_duration);
        let (scaler_len, complex_len) = Lc3Decoder::calc_working_buffer_lengths(1, n_ms, fs);
        Ok(scaler_len * size_of::<Scaler>() + complex_len * size_of::<Complex>() + DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES)
    }

    /// Builds a decoder for the given negotiated Sampling_Frequency/Frame_Duration.
    pub fn new(
        sampling_frequency: SamplingFrequency,
        frame_duration: FrameDuration,
    ) -> Result<Self, UnsupportedSamplingFrequency> {
        let fs = to_lc3_sampling_frequency(sampling_frequency)?;
        let n_ms = to_lc3_frame_duration(frame_duration);
        let (scaler_len, complex_len) = Lc3Decoder::calc_working_buffer_lengths(1, n_ms, fs);
        let scaler_buf = Box::leak(vec![0.0; scaler_len].into_boxed_slice());
        let complex_buf = Box::leak(vec![Default::default(); complex_len].into_boxed_slice());
        let decoder = Lc3Decoder::new(1, n_ms, fs, scaler_buf, complex_buf);
        Ok(Self {
            decoder,
            samples_per_frame: Lc3Config::new(fs, n_ms).nf,
        })
    }

    /// Decodes one encoded frame from `buf_in` into `samples_out` (`samples_per_frame` long).
    pub fn decode(&mut self, buf_in: &[u8], samples_out: &mut [i16]) -> Result<(), Lc3DecoderError> {
        self.decoder.decode_frame(16, 0, buf_in, samples_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a synthetic PCM signal and decodes it back, checking the round trip is at least
    /// in the right ballpark - this isn't asserting perceptual codec quality, just that the
    /// wrapper is wired up correctly (right buffer sizes, right frame sizes, no panics).
    #[test]
    fn encode_then_decode_round_trips_a_sine_wave() {
        let sampling_frequency = SamplingFrequency::Hz48000;
        let frame_duration = FrameDuration::Duration10MS;
        let mut encoder = Lc3MonoEncoder::new(sampling_frequency, frame_duration).unwrap();
        let mut decoder = Lc3MonoDecoder::new(sampling_frequency, frame_duration).unwrap();
        assert_eq!(encoder.samples_per_frame, decoder.samples_per_frame);

        let n = encoder.samples_per_frame;
        let pcm_in: alloc::vec::Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f32 / 48000.0;
                ((2.0 * core::f32::consts::PI * 440.0 * t).sin() * 8000.0) as i16
            })
            .collect();

        // 48000 Hz / 10ms mono at a typical LE Audio bitrate (~96 kbps) is 120 octets/frame.
        let mut encoded = [0u8; 120];
        encoder.encode(&pcm_in, &mut encoded).unwrap();

        let mut pcm_out = vec![0i16; n];
        decoder.decode(&encoded, &mut pcm_out).unwrap();

        // Encoding is lossy, so compare energy rather than exact samples.
        let energy_in: i64 = pcm_in.iter().map(|&s| (s as i64) * (s as i64)).sum();
        let energy_out: i64 = pcm_out.iter().map(|&s| (s as i64) * (s as i64)).sum();
        assert!(energy_out > energy_in / 2, "decoded energy {energy_out} too low vs input {energy_in}");
    }

    /// 19884 + 7680 bytes come from a real `handle_alloc_error` panic backtrace on nRF54L15
    /// hardware, not a guess.
    const DECODER_HEAP_BYTES_48K_10MS: usize = match Lc3MonoDecoder::heap_bytes(SamplingFrequency::Hz48000, FrameDuration::Duration10MS) {
        Ok(n) => n,
        Err(_) => panic!("Hz48000 must be supported"),
    };

    #[test]
    fn decoder_heap_bytes_is_a_safe_upper_bound_on_what_new_actually_allocates() {
        assert_eq!(DECODER_HEAP_BYTES_48K_10MS, 19884 + 7680 + DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES);

        let before = crate::test_alloc::allocated();
        let _decoder = Lc3MonoDecoder::new(SamplingFrequency::Hz48000, FrameDuration::Duration10MS).unwrap();
        let allocated = crate::test_alloc::allocated() - before;
        assert!(
            allocated <= DECODER_HEAP_BYTES_48K_10MS,
            "heap_bytes() = {DECODER_HEAP_BYTES_48K_10MS} underestimated the real allocation of {allocated} bytes - bump the margin"
        );
    }

    #[test]
    fn encoder_heap_bytes_is_a_safe_upper_bound_on_what_new_actually_allocates() {
        let expected = Lc3MonoEncoder::heap_bytes(SamplingFrequency::Hz48000, FrameDuration::Duration10MS).unwrap();

        let before = crate::test_alloc::allocated();
        let _encoder = Lc3MonoEncoder::new(SamplingFrequency::Hz48000, FrameDuration::Duration10MS).unwrap();
        let allocated = crate::test_alloc::allocated() - before;
        assert!(
            allocated <= expected,
            "heap_bytes() = {expected} underestimated the real allocation of {allocated} bytes - bump the margin"
        );
    }

    #[test]
    fn heap_bytes_rejects_the_same_sampling_frequencies_new_rejects() {
        assert_eq!(
            Lc3MonoDecoder::heap_bytes(SamplingFrequency::Hz384000, FrameDuration::Duration10MS),
            Err(UnsupportedSamplingFrequency(SamplingFrequency::Hz384000))
        );
        assert_eq!(
            Lc3MonoEncoder::heap_bytes(SamplingFrequency::Hz384000, FrameDuration::Duration10MS),
            Err(UnsupportedSamplingFrequency(SamplingFrequency::Hz384000))
        );
    }
}
