//! Minimal Auracast source application: advertises a stereo LC3 broadcast and continuously sends
//! two synthetic tones. Platform binaries only need to configure their controller for advertising,
//! periodic advertising, BIS source, and ISO buffers before calling [`run`].

use bt_hci::cmd::le::{
    LeCreateBig, LeRemoveIsoDataPath, LeSetAdvSetRandomAddr, LeSetExtAdvData, LeSetExtAdvEnable,
    LeSetExtAdvParams, LeSetPeriodicAdvData, LeSetPeriodicAdvEnable, LeSetPeriodicAdvParams,
    LeSetupIsoDataPath,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_futures::select::select3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use heapless::Vec as HVec;
use trouble_audio::big::{BigSource, BroadcastConfig, drive_big, start_broadcast};
use trouble_audio::generic_audio::{AudioLocation, ContextType};
use trouble_audio::iso_tx::{self, SequenceNumber};
use trouble_audio::lc3::Lc3MonoEncoder;
use trouble_audio::presets::AudioPreset;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{info, warn};

/// Largest PCM frame any supported preset produces (48 kHz, 10 ms).
const MAX_SAMPLES_PER_FRAME: usize = 480;
/// Largest encoded LC3 frame any supported preset produces.
const MAX_OCTETS_PER_FRAME: usize = 155;
const BIG_HANDLE: u8 = 0;

/// Runs a stereo Auracast test broadcast forever using `preset`.
pub async fn run<C>(
    controller: C,
    random_address: [u8; 6],
    broadcast_id: [u8; 3],
    preset: AudioPreset,
    broadcast_code: Option<[u8; 16]>,
) -> !
where
    C: Controller
        + ControllerCmdSync<LeSetExtAdvParams>
        + ControllerCmdSync<LeSetAdvSetRandomAddr>
        + for<'a> ControllerCmdSync<LeSetExtAdvData<'a>>
        + ControllerCmdSync<LeSetPeriodicAdvParams>
        + for<'a> ControllerCmdSync<LeSetPeriodicAdvData<'a>>
        + for<'a> ControllerCmdSync<LeSetExtAdvEnable<'a>>
        + ControllerCmdSync<LeSetPeriodicAdvEnable>
        + ControllerCmdAsync<LeCreateBig>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>,
{
    let mut resources: HostResources<DefaultPacketPool, 1, 1> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(Address::random(random_address))
        .build();
    let mut runner = stack.runner();
    let source = BigSource::<NoopRawMutex>::new(BIG_HANDLE);
    let mut bis = HVec::new();
    bis.push(AudioLocation::FrontLeft).unwrap();
    bis.push(AudioLocation::FrontRight).unwrap();
    let config = BroadcastConfig {
        big_handle: BIG_HANDLE,
        adv_handle: 0,
        adv_sid: 1,
        random_addr: random_address,
        broadcast_id,
        bis,
        sampling_frequency: preset.sampling_frequency,
        frame_duration: preset.frame_duration,
        octets_per_frame: preset.octets_per_frame,
        sdu_interval_us: preset.sdu_interval_us(),
        max_transport_latency_ms: preset.max_transport_latency_ms,
        rtn: preset.rtn,
        presentation_delay_us: preset.presentation_delay_us,
        streaming_contexts: ContextType::Media,
        broadcast_code,
    };

    select3(
        async {
            loop {
                if runner.run_with_handler(&source).await.is_err() {
                    #[cfg(feature = "defmt")]
                    warn!("[auracast_source] host runner stopped");
                }
            }
        },
        drive_big(&stack, &source),
        async {
            if let Err(_e) = start_broadcast(&stack, &source, &config).await {
                #[cfg(feature = "defmt")]
                warn!(
                    "[auracast_source] broadcast setup failed: {}",
                    defmt::Debug2Format(&_e)
                );
                core::future::pending::<()>().await;
            }
            let mut ready = [false; 2];
            while !ready.iter().all(|ready| *ready) {
                let index = usize::from(source.next_ready_bis().await);
                if let Some(ready) = ready.get_mut(index) {
                    *ready = true;
                }
            }
            #[cfg(feature = "defmt")]
            info!(
                "[auracast_source] both BIS ready preset={} interval_us={}",
                preset.name,
                preset.sdu_interval_us()
            );
            stream_test_tones(&stack, &source, preset).await;
        },
    )
    .await;
    unreachable!("Auracast source tasks run forever")
}

async fn stream_test_tones<C: Controller>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    source: &BigSource<NoopRawMutex>,
    preset: AudioPreset,
) -> ! {
    let mut encoders = [
        Lc3MonoEncoder::new(preset.sampling_frequency, preset.frame_duration).unwrap(),
        Lc3MonoEncoder::new(preset.sampling_frequency, preset.frame_duration).unwrap(),
    ];
    let mut sequences = [SequenceNumber::default(); 2];
    let mut phases = [0u32; 2];
    let phase_steps = [440u32, 660u32];
    let samples_per_frame = preset.samples_per_frame().min(MAX_SAMPLES_PER_FRAME);
    let octets_per_frame = usize::from(preset.octets_per_frame).min(MAX_OCTETS_PER_FRAME);
    let sampling_rate = preset.sampling_rate_hz();
    // 7.5 ms presets need sub-millisecond ticks, so the SDU interval drives the ticker directly.
    let mut ticker = embassy_time::Ticker::every(embassy_time::Duration::from_micros(u64::from(
        preset.sdu_interval_us(),
    )));
    let mut packet_buffer: HVec<u8, { iso_tx::MAX_ISO_PACKET_LEN }> = HVec::new();

    loop {
        ticker.next().await;
        for channel in 0..2 {
            let Some(handle) = source.bis_handle(channel) else {
                continue;
            };
            let mut pcm = [0i16; MAX_SAMPLES_PER_FRAME];
            for sample in &mut pcm[..samples_per_frame] {
                *sample = if phases[channel] < 24_000 {
                    4_000
                } else {
                    -4_000
                };
                phases[channel] = (phases[channel] + phase_steps[channel]) % sampling_rate;
            }
            let mut encoded = [0u8; MAX_OCTETS_PER_FRAME];
            if encoders[channel]
                .encode(&pcm[..samples_per_frame], &mut encoded[..octets_per_frame])
                .is_err()
            {
                continue;
            }
            let Some(packet) = iso_tx::build_packet(
                &mut packet_buffer,
                handle,
                sequences[channel].next(),
                &encoded[..octets_per_frame],
            ) else {
                continue;
            };
            let _ = stack.iso().send(&packet).await;
        }
    }
}
