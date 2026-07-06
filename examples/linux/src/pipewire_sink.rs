//! Plays decoded LE Audio PCM out through PipeWire, so the sink example's audio can actually be
//! heard rather than just counted. PipeWire's main loop is synchronous and owns its own thread,
//! so it runs on a dedicated `std::thread` rather than sharing the tokio runtime driving the BLE
//! side; decoded frames cross over an unbounded channel (never blocks the async sender - frames
//! are just queued for the next `process` callback).

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;
use trouble_audio::cis::PcmFrame;

const CHAN_SIZE: usize = core::mem::size_of::<i16>();

struct PlaybackState {
    rx: Receiver<PcmFrame>,
    buffered: VecDeque<i16>,
}

/// Spawns a dedicated thread running a mono S16LE PipeWire playback stream at `sample_rate`.
/// Send decoded PCM frames into the returned channel to have them played back.
pub fn spawn_playback(sample_rate: u32) -> Sender<PcmFrame> {
    let (tx, rx) = channel::<PcmFrame>();
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

fn run_playback_loop(sample_rate: u32, rx: Receiver<PcmFrame>) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let state = PlaybackState {
        rx,
        buffered: VecDeque::new(),
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "trouble-audio-sink",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::AUDIO_CHANNELS => "1",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, state| {
            while let Ok(frame) = state.rx.try_recv() {
                state.buffered.extend(frame.iter().copied());
            }
            match stream.dequeue_buffer() {
                None => log::warn!("[pipewire] out of buffers"),
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    let data = &mut datas[0];
                    let n_frames = if let Some(slice) = data.data() {
                        let n_frames = slice.len() / CHAN_SIZE;
                        for i in 0..n_frames {
                            // Underrun (no decoded audio buffered yet) plays silence rather than
                            // stalling the stream.
                            let sample = state.buffered.pop_front().unwrap_or(0);
                            let start = i * CHAN_SIZE;
                            slice[start..start + CHAN_SIZE].copy_from_slice(&sample.to_le_bytes());
                        }
                        n_frames
                    } else {
                        0
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = CHAN_SIZE as _;
                    *chunk.size_mut() = (CHAN_SIZE * n_frames) as _;
                }
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(sample_rate);
    audio_info.set_channels(1);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
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
