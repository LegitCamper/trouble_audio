#![no_std]
#![no_main]

use core::cell::RefCell;
use core::mem::MaybeUninit;

use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::{bind_interrupts, config, cracen, mode::Blocking};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_alloc::LlffHeap as Heap;
use nrf54l15_connect_kit::bond_store::rram_bond_store;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use static_cell::StaticCell;
use trouble_audio::cis::CisManager;
use trouble_audio::generic_audio::{FrameDuration, SamplingFrequency};
use trouble_audio::lc3::Lc3MonoDecoder;
use trouble_audio_example_apps::basic_audio_sink;
use trouble_host::prelude::*;
use {defmt_rtt as _, panic_probe as _};

/// Dominated by `Lc3MonoDecoder`'s working buffers, one per ASE (`MAX_ASES` = 2) - see
/// `trouble_audio::lc3` for the `heap_bytes`/`const`-assertion below that keeps this sized
/// correctly at build time.
const HEAP_SIZE: usize = 128 * 1024;

/// This sink's PAC only ever advertises 48kHz, and BAP mandates 10ms frame support - the central
/// has nothing else to negotiate (see `basic_audio_sink::run`'s `sink_pac`).
const NEGOTIATED_SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz48000;
const NEGOTIATED_FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;

/// Headroom for everything else `alloc`-backed (PAC records, GATT server construction, ...) -
/// not computed exactly like the LC3 buffers below, just budgeted.
const MISC_ALLOC_BUDGET_BYTES: usize = 16 * 1024;

const DECODER_HEAP_BYTES: usize = match Lc3MonoDecoder::heap_bytes(NEGOTIATED_SAMPLING_FREQUENCY, NEGOTIATED_FRAME_DURATION) {
    Ok(n) => n,
    Err(_) => panic!("unsupported sampling frequency"),
};
const _: () = assert!(
    HEAP_SIZE >= basic_audio_sink::MAX_ASES * DECODER_HEAP_BYTES + MISC_ALLOC_BUDGET_BYTES,
    "HEAP_SIZE too small to fit one Lc3MonoDecoder per ASE plus misc allocation headroom"
);

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    SWI00 => nrf_sdc::mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_sdc::mpsl::ClockInterruptHandler;
    RADIO_0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    TIMER10 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    GRTC_3 => nrf_sdc::mpsl::HighPrioInterruptHandler;
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

/// Drains decoded LC3 frames and turns them into a visible on/off signal on the Green LED (P0.2 -
/// the only LED this chip can drive; the RGB LED next to it belongs to a separate nRF52820 MCU).
/// This board has no speaker, so this is proof audio is actually arriving and decoding.
///
/// Plain on/off GPIO, not PWM brightness: `SimplePwm` on this chip/pin never lit the LED in
/// testing (tried both `DutyCycle::normal` and `::inverted`), while plain `Output` worked.
async fn drive_led(led: &mut Output<'_>, cis_manager: &CisManager<NoopRawMutex, { basic_audio_sink::MAX_ASES }>) -> ! {
    let mut frames = 0u32;
    loop {
        let pcm = cis_manager.receive_pcm().await;
        let peak = pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0).min(i16::MAX as u16);
        if peak >= LED_ON_THRESHOLD {
            led.set_high();
        } else {
            led.set_low();
        }
        frames = frames.wrapping_add(1);
        if frames % 100 == 0 {
            defmt::debug!("[sink] decoded frames={} latest_ase_id={} peak={}", frames, pcm.ase_id, peak);
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("start");
    {
        static HEAP_MEM: StaticCell<[MaybeUninit<u8>; HEAP_SIZE]> = StaticCell::new();
        let heap_mem = HEAP_MEM.init([const { MaybeUninit::uninit() }; HEAP_SIZE]);
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
    let mut led = Output::new(p.P0_02, Level::Low, OutputDrive::Standard);
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

    // Bump if `build_sdc` fails - it logs the exact number of bytes needed.
    let mut sdc_mem = sdc::Mem::<16384>::new();
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    let cis_manager = CisManager::<NoopRawMutex, { basic_audio_sink::MAX_ASES }>::new();

    // Persists the bond to on-chip RRAM so re-pairing isn't needed after every reflash.
    let flash = RefCell::new(Nvmc::new(p.RRAMC));
    let bond_store = rram_bond_store(&flash);

    defmt::info!("Running ble audio sink example");

    select(
        basic_audio_sink::run(sdc, &cis_manager, Some(&bond_store)),
        drive_led(&mut led, &cis_manager),
    )
    .await;
    unreachable!("both branches above loop forever")
}
