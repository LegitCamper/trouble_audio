#![no_std]
#![no_main]

use core::cell::RefCell;
use core::mem::MaybeUninit;

use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::{bind_interrupts, config, cracen, mode::Blocking};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Instant, Timer};
use embedded_alloc::LlffHeap as Heap;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use nrf54l15_connect_kit::bond_store::rram_bond_store;
use static_cell::{ConstStaticCell, StaticCell};
use trouble_audio::cis::{CisManager, PcmFrame};
use trouble_audio::generic_audio::{FrameDuration, SamplingFrequency};
use trouble_audio::lc3::Lc3MonoDecoder;
use trouble_audio_example_apps::basic_audio_sink;
use trouble_host::prelude::*;
use {defmt_rtt as _, panic_probe as _};

#[path = "sink/i2s_output.rs"]
mod i2s_output;

use i2s_output::{I2sOutput, LC3_FRAMES_PER_DMA_BUFFER};

/// Dominated by the single `Lc3MonoDecoder`'s working buffers - see `trouble_audio::lc3` for the
/// `heap_bytes`/`const`-assertion below that keeps this sized correctly at build time.
const HEAP_SIZE: usize = 80 * 1024;

/// This sink's PAC only ever advertises 48kHz, and BAP mandates 10ms frame support - the central
/// has nothing else to negotiate (see `basic_audio_sink::run`'s `sink_pac`).
const NEGOTIATED_SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz48000;
const NEGOTIATED_FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;

/// Headroom for everything else `alloc`-backed (PAC records, GATT server construction, ...) -
/// not computed exactly like the LC3 buffers below, just budgeted.
const MISC_ALLOC_BUDGET_BYTES: usize = 16 * 1024;

const DECODER_HEAP_BYTES: usize =
    match Lc3MonoDecoder::heap_bytes(NEGOTIATED_SAMPLING_FREQUENCY, NEGOTIATED_FRAME_DURATION) {
        Ok(n) => n,
        Err(_) => panic!("unsupported sampling frequency"),
    };
const _: () = assert!(
    HEAP_SIZE >= DECODER_HEAP_BYTES + MISC_ALLOC_BUDGET_BYTES,
    "HEAP_SIZE too small to fit the mono Lc3MonoDecoder plus misc allocation headroom"
);

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    SWI00 => nrf_sdc::mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_sdc::mpsl::ClockInterruptHandler;
    RADIO_0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    TIMER10 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    GRTC_3 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    I2S20 => i2s_output::InterruptHandler;
});

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

/// How many outgoing L2CAP buffers per link
const L2CAP_TXQ: u8 = 3;

/// How many incoming L2CAP buffers per link
const L2CAP_RXQ: u8 = 3;

fn build_sdc<'d, const N: usize>(
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut cracen::Cracen<'static, Blocking>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .support_cis_peripheral()
        .peripheral_count(1)?
        .buffer_cfg(
            DefaultPacketPool::MTU as u16,
            DefaultPacketPool::MTU as u16,
            L2CAP_TXQ,
            L2CAP_RXQ,
        )?
        // `support_cis_peripheral()` only enables the capability - CIG/CIS/ISO buffer counts
        // default to 0, so without these `LE Create CIS` fails with `MEMORY_CAPACITY_EXCEEDED`.
        .cig_count(1)?
        .cis_count(2)?
        .iso_buffer_cfg(4, 128, 4, 6, 8, 128)?
        .build(p, rng, mpsl, mem)
}

/// Silence/near-silence threshold below which `drive_led` treats a frame as "no audio", out of
/// the full [0, 32767] `i16` peak-magnitude range - picked to ignore LC3 dither/quantization
/// noise on true digital silence without needing a real loudness measurement.
const LED_ON_THRESHOLD: u16 = 512;

const I2S_SHORT_FRAME_SAMPLES: usize = 476;

fn write_resampled_channel(
    output: &mut [i16],
    input: &[i16],
    channel: usize,
    output_frames: usize,
) {
    if input.is_empty() {
        return;
    }
    for output_index in 0..output_frames {
        let input_index =
            ((output_index * input.len() + output_frames / 2) / output_frames).min(input.len() - 1);
        output[output_index * 2 + channel] = input[input_index];
    }
}

fn next_i2s_frame_samples(phase: &mut u8) -> usize {
    // 47_619.047 Hz produces 476 + 4/21 samples per 10-ms LC3 frame.
    *phase += 4;
    if *phase >= 21 {
        *phase -= 21;
        I2S_SHORT_FRAME_SAMPLES + 1
    } else {
        I2S_SHORT_FRAME_SAMPLES
    }
}

/// Keeps the no-extra-hardware LED indicator while also sending decoded PCM to I2S20. The first
/// Sink ASE is decoded and mirrored to both I2S channels; the second is discarded while still
/// encoded so two software LC3 decoders cannot starve the radio/audio tasks.
async fn drive_audio_outputs(
    led: &mut Output<'_>,
    mut i2s: I2sOutput,
    cis_manager: &CisManager<NoopRawMutex, { basic_audio_sink::MAX_ASES }>,
) -> ! {
    // Four 10-ms frames make each DMA buffer about 40 ms long. Collect for at most 31 ms,
    // leaving roughly 9 ms to queue the next pointer even when MPSL briefly owns the CPU.
    const PCM_COLLECTION_WINDOW: Duration = Duration::from_millis(31);

    let mut decoder = unwrap!(Lc3MonoDecoder::new(
        NEGOTIATED_SAMPLING_FREQUENCY,
        NEGOTIATED_FRAME_DURATION,
    ));

    i2s.buffer().fill(0);
    defmt::info!("starting I2S DMA");
    i2s.start(I2S_SHORT_FRAME_SAMPLES * LC3_FRAMES_PER_DMA_BUFFER * 2)
        .await;
    defmt::info!("I2S DMA started");

    let mut selected_ase = None;
    let mut resample_phase = 0;
    let mut frames = 0u32;
    let mut dma_buffers = 0u32;
    loop {
        let mut segment_offsets = [0; LC3_FRAMES_PER_DMA_BUFFER];
        let mut segment_lengths = [0; LC3_FRAMES_PER_DMA_BUFFER];
        let mut output_frames = 0;
        for segment in 0..LC3_FRAMES_PER_DMA_BUFFER {
            segment_offsets[segment] = output_frames;
            segment_lengths[segment] = next_i2s_frame_samples(&mut resample_phase);
            output_frames += segment_lengths[segment];
        }

        i2s.buffer().fill(0);
        let mut mono_frames = 0;
        let deadline = Instant::now() + PCM_COLLECTION_WINDOW;

        while mono_frames < LC3_FRAMES_PER_DMA_BUFFER {
            let raw = match select(Timer::at(deadline), cis_manager.receive_lc3()).await {
                Either::First(()) => break,
                Either::Second(raw) => raw,
            };

            let ase_id = match selected_ase {
                Some(ase_id) => ase_id,
                None => {
                    selected_ase = Some(raw.ase_id);
                    defmt::info!(
                        "I2S selected mono ASE {}; mirroring it to left and right",
                        raw.ase_id
                    );
                    raw.ase_id
                }
            };
            if raw.ase_id != ase_id {
                continue;
            }

            let mut samples = PcmFrame::new();
            unwrap!(samples.resize_default(decoder.samples_per_frame));
            if decoder.decode(&raw.frame, &mut samples).is_err() {
                defmt::warn!("LC3 decode failed for ASE {}", raw.ase_id);
                continue;
            }

            let start = segment_offsets[mono_frames] * 2;
            let end = (segment_offsets[mono_frames] + segment_lengths[mono_frames]) * 2;
            write_resampled_channel(
                &mut i2s.buffer()[start..end],
                &samples,
                0,
                segment_lengths[mono_frames],
            );
            write_resampled_channel(
                &mut i2s.buffer()[start..end],
                &samples,
                1,
                segment_lengths[mono_frames],
            );
            mono_frames += 1;

            let peak = samples
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap_or(0)
                .min(i16::MAX as u16);
            if peak >= LED_ON_THRESHOLD {
                led.set_high();
            } else {
                led.set_low();
            }
            frames = frames.wrapping_add(1);
            if frames == 1 {
                defmt::info!("I2S received first decoded PCM frame");
            }
            if frames.is_multiple_of(100) {
                defmt::debug!(
                    "[sink] decoded mono frames={} ase_id={} peak={}",
                    frames,
                    raw.ase_id,
                    peak
                );
            }
        }
        if mono_frames == 0 {
            led.set_low();
        }
        i2s.send(output_frames * 2).await;
        dma_buffers = dma_buffers.wrapping_add(1);
        if dma_buffers == 25 {
            defmt::info!("I2S DMA remained continuous for 100 LC3 frame periods");
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("start");
    {
        static HEAP_MEM: ConstStaticCell<[MaybeUninit<u8>; HEAP_SIZE]> =
            ConstStaticCell::new([const { MaybeUninit::uninit() }; HEAP_SIZE]);
        let heap_mem = HEAP_MEM.take();
        unsafe { HEAP.init(heap_mem.as_ptr() as usize, HEAP_SIZE) }
    }

    // See `bin/source.rs` for why this needs the external crystal rather than the default RC
    // clocks - CIS/ISO timing needs crystal-grade LFCLK accuracy, here too since this device is a
    // CIS peripheral (even though the central owns the CIG).
    let mut config: config::Config = Default::default();
    config.clock_speed = config::ClockSpeed::CK128;
    config.hfclk_source = config::HfclkSource::ExternalXtal;
    config.lfclk_source = config::LfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);
    defmt::info!("clocks initialized");

    // Self-test: 3 blinks before BLE/audio touches this pin - if this doesn't blink, the problem
    // is P0.2/the LED/the board, not anything audio-related further down.
    static LED: StaticCell<Output> = StaticCell::new();
    let led = LED.init(Output::new(p.P0_02, Level::Low, OutputDrive::Standard));
    defmt::info!("LED self-test: 3 blinks via plain GPIO on P0.2");
    for i in 0..3 {
        defmt::info!("LED self-test: blink {}", i + 1);
        led.set_high();
        embassy_time::Timer::after_millis(200).await;
        led.set_low();
        embassy_time::Timer::after_millis(200).await;
    }
    defmt::info!("LED self-test: done");

    let mpsl_p = mpsl::Peripherals::new(
        p.GRTC_CH7,
        p.GRTC_CH8,
        p.GRTC_CH9,
        p.GRTC_CH10,
        p.GRTC_CH11,
        p.TIMER10,
        p.TIMER20,
        p.TEMP,
        p.PPI10_CH0,
        p.PPI20_CH1,
        p.PPIB11_CH0,
        p.PPIB21_CH0,
    );
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: 50,
        skip_wait_lfclk_started: false,
    };
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(unwrap!(mpsl::MultiprotocolServiceLayer::new(
        mpsl_p, Irqs, lfclk_cfg
    )));
    spawner.spawn(unwrap!(mpsl_task(&*mpsl)));

    let sdc_p = sdc::Peripherals::new(
        p.PPI00_CH1,
        p.PPI00_CH3,
        p.PPI10_CH1,
        p.PPI10_CH2,
        p.PPI10_CH3,
        p.PPI10_CH4,
        p.PPI10_CH5,
        p.PPI10_CH6,
        p.PPI10_CH7,
        p.PPI10_CH8,
        p.PPI10_CH9,
        p.PPI10_CH10,
        p.PPI10_CH11,
        p.PPIB00_CH1,
        p.PPIB00_CH2,
        p.PPIB00_CH3,
        p.PPIB10_CH1,
        p.PPIB10_CH2,
        p.PPIB10_CH3,
    );

    let mut rng = cracen::Cracen::new_blocking(p.CRACEN);

    // Keep this large backing store out of the debug call stack. Bump its size if `build_sdc`
    // fails - it logs the exact number of bytes needed.
    static SDC_MEM: StaticCell<sdc::Mem<16384>> = StaticCell::new();
    let sdc_mem = SDC_MEM.init_with(sdc::Mem::new);
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, sdc_mem));

    static CIS_MANAGER: StaticCell<CisManager<NoopRawMutex, { basic_audio_sink::MAX_ASES }>> =
        StaticCell::new();
    let cis_manager = CIS_MANAGER.init(CisManager::new_passthrough());

    // Persists the bond to on-chip RRAM so re-pairing isn't needed after every reflash.
    let flash = RefCell::new(Nvmc::new(p.RRAMC));
    let bond_store = rram_bond_store(&flash);

    // Initialize I2S after MPSL has finished configuring shared interrupt-controller state.
    let i2s = I2sOutput::new(p.P1_12, p.P1_11, p.P1_14);
    defmt::info!("I2S ready: BCLK=P1.12 LRCK=P1.11 SDOUT=P1.14 (no MCK)");

    defmt::info!("Running ble audio sink example");
    let _ = select(
        drive_audio_outputs(led, i2s, cis_manager),
        basic_audio_sink::run(sdc, cis_manager, Some(&bond_store)),
    )
    .await;
}
