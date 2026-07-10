//! Plays decoded LE Audio PCM out through PipeWire, so the sink example's audio can actually be
//! heard rather than just counted. PipeWire's main loop is synchronous and owns its own thread,
//! so it runs on a dedicated `std::thread` rather than sharing the tokio runtime driving the BLE
//! side; decoded frames cross over an unbounded channel (never blocks the async sender - frames
//! are just queued for the next `process` callback).
//!
//! The sink example exposes one Sink ASE per stereo channel (see
//! `trouble_audio_example_apps::basic_audio_sink`), so [`CisManager::receive_pcm`] multiplexes
//! both channels' decoded frames onto one queue, tagged with which ASE (and, if the central
//! declared one, which [`AudioLocation`]) each frame came from. This module demultiplexes that
//! back into left/right buffers before handing PipeWire interleaved stereo samples.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;
use trouble_audio::cis::DecodedPcm;
use trouble_audio::generic_audio::AudioLocation;

const CHAN_SIZE: usize = core::mem::size_of::<i16>();
const CHANNELS: usize = 2;
const LEFT: usize = 0;
const RIGHT: usize = 1;

struct PlaybackState {
    rx: Receiver<DecodedPcm>,
    /// Which channel (`LEFT`/`RIGHT`) each ASE ID's frames go to, in order of first appearance -
    /// used as a fallback for centrals that don't send `Audio_Channel_Allocation` in Config Codec.
    ase_order: [Option<u8>; CHANNELS],
    buffered: [VecDeque<i16>; CHANNELS],
}

impl PlaybackState {
    /// Resolves which channel a decoded frame belongs to: trusts the negotiated
    /// `Audio_Channel_Allocation` when the central declared one, otherwise falls back to treating
    /// the first ASE seen as left and the second as right.
    fn channel_for(&mut self, frame: &DecodedPcm) -> usize {
        if let Some(location) = frame.channel_allocation {
            if location.contains(AudioLocation::FrontRight) && !location.contains(AudioLocation::FrontLeft) {
                return RIGHT;
            }
            return LEFT;
        }
        if let Some(idx) = self.ase_order.iter().position(|ase| *ase == Some(frame.ase_id)) {
            return idx;
        }
        let idx = self.ase_order.iter().position(|ase| ase.is_none()).unwrap_or(LEFT);
        self.ase_order[idx] = Some(frame.ase_id);
        idx
    }
}

/// Spawns a dedicated thread running a stereo S16LE PipeWire playback stream at `sample_rate`.
/// Send decoded PCM frames into the returned channel to have them played back.
pub fn spawn_playback(sample_rate: u32) -> Sender<DecodedPcm> {
    let (tx, rx) = channel::<DecodedPcm>();
    std::thread::Builder::new()
        .name("pipewire-playback".into())
        .spawn(move || {
            if let Err(e) = run_playback_loop(sample_rate, rx) {
                log::error!("[pipewire] playback failed: {e}");
            }
        })
        .expect("failed to spawn PipeWire playback thread");
    tx
}

fn run_playback_loop(sample_rate: u32, rx: Receiver<DecodedPcm>) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let state = PlaybackState {
        rx,
        ase_order: [None; CHANNELS],
        buffered: [VecDeque::new(), VecDeque::new()],
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "trouble-audio-sink",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::AUDIO_CHANNELS => "2",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, state| {
            while let Ok(frame) = state.rx.try_recv() {
                let channel = state.channel_for(&frame);
                state.buffered[channel].extend(frame.samples.iter().copied());
            }
            match stream.dequeue_buffer() {
                None => log::warn!("[pipewire] out of buffers"),
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    let data = &mut datas[0];
                    let frame_size = CHAN_SIZE * CHANNELS;
                    let n_frames = if let Some(slice) = data.data() {
                        let n_frames = slice.len() / frame_size;
                        for i in 0..n_frames {
                            // Underrun (no decoded audio buffered yet) plays silence rather than
                            // stalling the stream.
                            let left = state.buffered[LEFT].pop_front().unwrap_or(0);
                            let right = state.buffered[RIGHT].pop_front().unwrap_or(0);
                            let start = i * frame_size;
                            slice[start..start + CHAN_SIZE].copy_from_slice(&left.to_le_bytes());
                            slice[start + CHAN_SIZE..start + frame_size].copy_from_slice(&right.to_le_bytes());
                        }
                        n_frames
                    } else {
                        0
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = frame_size as _;
                    *chunk.size_mut() = (frame_size * n_frames) as _;
                }
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(sample_rate);
    audio_info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[LEFT] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[RIGHT] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();
    Ok(())
}
