//! Bridges decoded LE Audio PCM (48 kHz signed 16-bit, one mono stream per Sink ASE) into the
//! [`crate::pio_audio`] task's 22.05 kHz unsigned 8-bit interleaved stereo buffer.

use core::sync::atomic::Ordering;

use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use trouble_audio::cis::{CisManager, DecodedPcm};
use trouble_audio::generic_audio::AudioLocation;

use crate::pio_audio::{self, AUDIO_BUFFER, AUDIO_BUFFER_READY, AUDIO_BUFFER_SAMPLES, AUDIO_BUFFER_WRITTEN};

const SOURCE_HZ: u32 = 48_000;
const LEFT: usize = 0;
const RIGHT: usize = 1;

/// Drains `cis_manager`'s decoded PCM forever: demultiplexes it into left/right, decimates each
/// from 48 kHz down to [`pio_audio::SAMPLE_RATE_HZ`] and converts to unsigned 8-bit PCM, then
/// hands filled buffers off to [`pio_audio::audio_handler`] over the `AUDIO_BUFFER*` statics.
pub async fn play_decoded_audio(cis_manager: &CisManager<NoopRawMutex, 2>) -> ! {
    // Fixed-point accumulators for nearest-neighbor decimation from 48 kHz to 22.05 kHz.
    let mut acc = [0u32; 2];
    let mut staged = [[0u8; AUDIO_BUFFER_SAMPLES]; 2];
    let mut staged_len = [0usize; 2];
    // Which channel each ASE ID's frames go to, in order of first appearance - a fallback for
    // centrals that don't send `Audio_Channel_Allocation` in Config Codec.
    let mut ase_order: [Option<u8>; 2] = [None; 2];

    loop {
        let frame = cis_manager.receive_pcm().await;
        let ch = channel_for(&frame, &mut ase_order);

        for &sample in frame.samples.iter() {
            acc[ch] += pio_audio::SAMPLE_RATE_HZ;
            if acc[ch] >= SOURCE_HZ {
                acc[ch] -= SOURCE_HZ;
                if staged_len[ch] < AUDIO_BUFFER_SAMPLES {
                    staged[ch][staged_len[ch]] = ((sample as i32 >> 8) + 128) as u8;
                    staged_len[ch] += 1;
                }
            }
        }

        if staged_len[LEFT] == AUDIO_BUFFER_SAMPLES && staged_len[RIGHT] == AUDIO_BUFFER_SAMPLES {
            while !AUDIO_BUFFER_READY.load(Ordering::Acquire) {
                yield_now().await;
            }
            #[allow(static_mut_refs)]
            unsafe {
                for i in 0..AUDIO_BUFFER_SAMPLES {
                    AUDIO_BUFFER[i * 2] = staged[LEFT][i];
                    AUDIO_BUFFER[i * 2 + 1] = staged[RIGHT][i];
                }
            }
            AUDIO_BUFFER_READY.store(false, Ordering::Release);
            AUDIO_BUFFER_WRITTEN.store(true, Ordering::Release);
            staged_len = [0, 0];
        }
    }
}

/// Resolves which channel (`LEFT`/`RIGHT`) a decoded frame belongs to: trusts the negotiated
/// `Audio_Channel_Allocation` when the central declared one, otherwise falls back to treating the
/// first ASE seen as left and the second as right.
fn channel_for(frame: &DecodedPcm, ase_order: &mut [Option<u8>; 2]) -> usize {
    if let Some(location) = frame.channel_allocation {
        return if location.contains(AudioLocation::FrontRight) && !location.contains(AudioLocation::FrontLeft) {
            RIGHT
        } else {
            LEFT
        };
    }
    if let Some(idx) = ase_order.iter().position(|ase| *ase == Some(frame.ase_id)) {
        return idx;
    }
    let idx = ase_order.iter().position(|ase| ase.is_none()).unwrap_or(LEFT);
    ase_order[idx] = Some(frame.ase_id);
    idx
}
