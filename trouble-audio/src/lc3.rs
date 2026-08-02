//! Thin, stateful wrapper around the `lc3-codec` crate's `Lc3Encoder`/`Lc3Decoder`, parameterized
//! from a negotiated Codec_Specific_Configuration (Sampling_Frequency, Frame_Duration).
//!
//! `lc3-codec`'s encoder/decoder hold state across frames (long-term post filter memory, attack
//! detection, packet loss concealment, ...) and borrow their scratch buffers rather than owning
//! them, so this wrapper leaks its buffers (`Box::leak`) to give them `'static` lifetime and owns
//! the encoder/decoder for as long as the audio session lasts - reasonable since that memory is
//! meant to live for the session's duration anyway, same as it would for a stack-allocated
//! embedded buffer.
//!
//! # Sizing a `#[global_allocator]` heap
//!
//! On a `no_std` target, [`Lc3MonoEncoder::new`]/[`Lc3MonoDecoder::new`]'s leaked buffers are by
//! far the largest and most predictable consumer of heap: tens of KB each, fixed for the life of
//! the session, entirely determined by the negotiated Sampling_Frequency/Frame_Duration. Every
//! other allocation this crate makes (PAC records, ASE/codec configuration blobs, GATT server
//! construction, ...) is comparatively tiny (low hundreds of bytes) and shaped by *your*
//! `PeripheralConfig`/ASE count rather than anything computable here - budget headroom for those
//! by hand, same as you always would.
//!
//! [`Lc3MonoEncoder::heap_bytes`]/[`Lc3MonoDecoder::heap_bytes`] are `const fn`, so a binary that
//! knows its own topology at compile time (which sampling frequency/frame duration it'll
//! negotiate, how many concurrent mono encoders/decoders it builds - one per ASE, e.g. two for
//! stereo) can turn the "did I size `HEAP_SIZE` correctly" question into a build-time
//! `const`-assertion instead of a runtime `handle_alloc_error` panic days into testing. Both are a
//! safe upper bound rather than byte-exact (see their doc comments for why) - fine for this use,
//! since over-provisioning a `const HEAP_SIZE` costs nothing but unused RAM, while
//! under-provisioning is the `handle_alloc_error` panic this exists to avoid:
//!
//! ```ignore
//! const SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz48000;
//! const FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;
//! const STEREO_DECODER_BYTES: usize =
//!     2 * match Lc3MonoDecoder::heap_bytes(SAMPLING_FREQUENCY, FRAME_DURATION) {
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

/// On top of the scratch buffers `Lc3MonoDecoder`/`Lc3MonoEncoder` explicitly leak, `lc3-codec`'s
/// `Lc3Decoder`/`Lc3Encoder` (with its default `alloc` feature, which this crate uses) also
/// allocate a small `Vec<Channel>` internally to hold their own per-channel filter-state structs -
/// one element for us, since we always ask for `num_channels = 1` (mono). Those `Channel` types
/// are private to `lc3-codec`, so `size_of::<T>()` isn't reachable from here; these margins are a
/// generous, empirically-measured upper bound instead (real cost measured on a 64-bit host: ~1056
/// bytes decoder / ~5320 bytes encoder, both well under these values - see `lc3::tests`). Safe as
/// an upper bound on the 32-bit embedded targets this feature actually exists for: those structs
/// hold slices/pointers, which are narrower there, so the real allocation there is smaller still,
/// never bigger. Re-measure (see `lc3::tests::decoder_heap_bytes_matches_what_new_actually_allocates`)
/// if a `lc3-codec` upgrade ever changes these structs enough to blow through the margin.
const DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES: usize = 2048;
/// See [`DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES`] - same reasoning, `Lc3Encoder`'s internal
/// per-channel struct just happens to be larger.
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
    /// Bytes [`Self::new`] will [`Box::leak`]/otherwise allocate for the given negotiated
    /// Sampling_Frequency/Frame_Duration - see the module docs for how to use this to
    /// compile-time-assert a `#[global_allocator]` heap is big enough. A safe upper bound, not
    /// necessarily byte-exact: covers the three big scratch buffers `new` leaks exactly (same
    /// `calc_working_buffer_lengths` call, same element types, tens of KB - the overwhelming
    /// majority of this number) plus [`INTERNAL_BOOKKEEPING_MARGIN_BYTES`] for `lc3-codec`'s own
    /// small internal per-channel state, which its public API doesn't expose enough to size
    /// exactly from here (see that constant's doc for why).
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
    /// Bytes [`Self::new`] will [`Box::leak`]/otherwise allocate for the given negotiated
    /// Sampling_Frequency/Frame_Duration - see the module docs for how to use this to
    /// compile-time-assert a `#[global_allocator]` heap is big enough. A safe upper bound, not
    /// necessarily byte-exact: covers the two big scratch buffers `new` leaks exactly (same
    /// `calc_working_buffer_lengths` call, same element types, tens of KB - the overwhelming
    /// majority of this number) plus [`DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES`] for
    /// `lc3-codec`'s own small internal per-channel state, which its public API doesn't expose
    /// enough to size exactly from here (see that constant's doc for why).
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

    /// `heap_bytes` must actually be usable in a `const` context - that's the entire point
    /// (compile-time `HEAP_SIZE` assertions in downstream binaries) - and must be a safe *upper
    /// bound* on what `new` really allocates (it deliberately never estimates low, so it never
    /// silently sets a caller up for the `handle_alloc_error` panic it exists to avoid - see
    /// `DECODER_INTERNAL_BOOKKEEPING_MARGIN_BYTES`'s doc for why it isn't byte-exact). The two big
    /// scratch buffers it accounts for exactly (19884 + 7680 bytes for this config) come from a
    /// real `handle_alloc_error` panic backtrace hit on nRF54L15 hardware, not a hand guess.
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
