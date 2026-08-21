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
use trouble_audio::generic_audio::{AudioLocation, ContextType, FrameDuration, SamplingFrequency};
use trouble_audio::iso_tx::{self, SequenceNumber};
use trouble_audio::lc3::Lc3MonoEncoder;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{info, warn};

const SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz48000;
const FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;
const SAMPLES_PER_FRAME: usize = 480;
const OCTETS_PER_FRAME: u16 = 100;
const BIG_HANDLE: u8 = 0;

/// Runs an unencrypted stereo Auracast test broadcast forever.
pub async fn run<C>(controller: C, random_address: [u8; 6], broadcast_id: [u8; 3]) -> !
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
        adv_handle: 1,
        adv_sid: 1,
        random_addr: random_address,
        broadcast_id,
        bis,
        sampling_frequency: SAMPLING_FREQUENCY,
        frame_duration: FRAME_DURATION,
        octets_per_frame: OCTETS_PER_FRAME,
        sdu_interval_us: 10_000,
        max_transport_latency_ms: 20,
        rtn: 2,
        presentation_delay_us: 40_000,
        streaming_contexts: ContextType::Media,
        broadcast_code: None,
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
            if start_broadcast(&stack, &source, &config).await.is_err() {
                #[cfg(feature = "defmt")]
                warn!("[auracast_source] broadcast setup failed");
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
            info!("[auracast_source] both BIS ready");
            stream_test_tones(&stack, &source).await;
        },
    )
    .await;
    unreachable!("Auracast source tasks run forever")
}

async fn stream_test_tones<C: Controller>(
    stack: &Stack<'_, C, DefaultPacketPool>,
    source: &BigSource<NoopRawMutex>,
) -> ! {
    let mut encoders = [
        Lc3MonoEncoder::new(SAMPLING_FREQUENCY, FRAME_DURATION).unwrap(),
        Lc3MonoEncoder::new(SAMPLING_FREQUENCY, FRAME_DURATION).unwrap(),
    ];
    let mut sequences = [SequenceNumber::default(); 2];
    let mut phases = [0u32; 2];
    let phase_steps = [440u32, 660u32];
    let mut ticker = embassy_time::Ticker::every(embassy_time::Duration::from_millis(10));
    let mut packet_buffer: HVec<u8, { iso_tx::MAX_ISO_PACKET_LEN }> = HVec::new();

    loop {
        ticker.next().await;
        for channel in 0..2 {
            let Some(handle) = source.bis_handle(channel) else {
                continue;
            };
            let mut pcm = [0i16; SAMPLES_PER_FRAME];
            for sample in &mut pcm {
                *sample = if phases[channel] < 24_000 {
                    4_000
                } else {
                    -4_000
                };
                phases[channel] = (phases[channel] + phase_steps[channel]) % 48_000;
            }
            let mut encoded = [0u8; OCTETS_PER_FRAME as usize];
            if encoders[channel].encode(&pcm, &mut encoded).is_err() {
                continue;
            }
            let Some(packet) = iso_tx::build_packet(
                &mut packet_buffer,
                handle,
                sequences[channel].next(),
                &encoded,
            ) else {
                continue;
            };
            let _ = stack.iso().send(&packet).await;
        }
    }
}
