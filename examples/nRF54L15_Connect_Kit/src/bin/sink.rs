#![no_std]
#![no_main]

use core::mem::MaybeUninit;

use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::{bind_interrupts, config, cracen, mode::Blocking};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_alloc::LlffHeap as Heap;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use static_cell::StaticCell;
use trouble_audio::cis::CisManager;
use trouble_audio_example_apps::basic_audio_sink;
use trouble_host::prelude::*;
use {defmt_rtt as _, panic_probe as _};

/// `trouble_audio`/`basic_audio_sink` use `alloc` for LE Audio's variable-length data (PAC
/// records, codec configuration, metadata, ...), so a global allocator must be installed.
///
/// Also has to cover `Lc3MonoDecoder`'s working buffers (`Box::leak`'d, never freed): one
/// decoder needs 27564 bytes at 48kHz/10ms (19884 scaler + 7680 complex), and this sink runs
/// two decoders (stereo) concurrently - budget generously (256KB total RAM, plenty of headroom).
const HEAP_SIZE: usize = 128 * 1024;

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
        // all default to 0, so without these `LE Create CIS` fails with
        // `MEMORY_CAPACITY_EXCEEDED` no matter what. One CIG/two CIS to match this sink's two
        // Sink ASEs (stereo); ISO buffers sized for the negotiated max_sdu (120 bytes/channel).
        //
        // rx_pdu_buffer_per_stream_count/rx_sdu_buffer_count started at 2/4 and every SDU came
        // back marked Lost (packet_status_flag=2, see on_iso_data's debug log) despite the CIS
        // being established - looks like reassembly buffer starvation rather than real RF loss,
        // so bumped generously here.
        .cig_count(1)?
        .cis_count(2)?
        .iso_buffer_cfg(4, 128, 4, 6, 8, 128)?
        .build(p, rng, mpsl, mem)
}

/// Silence/near-silence threshold below which `drive_led` treats a frame as "no audio", out of
/// the full [0, 32767] `i16` peak-magnitude range - picked to ignore LC3 dither/quantization
/// noise on true digital silence without needing a real loudness measurement.
const LED_ON_THRESHOLD: u16 = 512;

/// Drains decoded LC3 frames and turns them into a visible on/off signal on the board's single
/// user-programmable LED (Green LED, P0.2 on the nRF54L15 Connect Kit - the RGB LED next to it is
/// wired to the separate nRF52820 interface MCU and isn't controllable from here). This board has
/// no speaker/mic, so this is only meant as a proof that audio is actually arriving and decoding
/// correctly, before wiring this sink into a real playback path.
///
/// This drives the LED as a plain thresholded on/off GPIO rather than PWM brightness: on real
/// hardware, `SimplePwm` on `PWM20`/P0.2 never lit the LED at all (tried both `DutyCycle::normal`
/// and `::inverted`, confirmed with a dedicated boot-time self-test), while a plain
/// `embassy_nrf::gpio::Output` on the same pin worked immediately - so this sidesteps whatever's
/// wrong with the PWM peripheral/driver for this chip/pin/embassy-nrf revision rather than
/// chasing it further.
async fn drive_led(led: &mut Output<'_>, cis_manager: &CisManager<NoopRawMutex, { basic_audio_sink::MAX_ASES }>) -> ! {
    loop {
        let pcm = cis_manager.receive_pcm().await;
        let peak = pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0).min(i16::MAX as u16);
        defmt::debug!("[sink] drive_led: ase_id={} peak={}", pcm.ase_id, peak);
        if peak >= LED_ON_THRESHOLD {
            led.set_high();
        } else {
            led.set_low();
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

    // Plain GPIO, not PWM - see `drive_led`'s doc comment for why. Self-test: 3 unmistakable
    // blinks before BLE/audio ever touches this pin, the same way makerdiary's own `blinky`
    // sample proves out `led0` (P0.2, active-high - see nrf54l15-connectkit's board devicetree).
    // If this doesn't blink, the problem is P0.2/the LED/the board itself, not anything
    // audio-related further down.
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

    // Reserving real CIG/CIS/ISO buffers (see `build_sdc`) pushed the requirement to 9248 bytes,
    // then to 13600 once the ISO PDU/SDU buffer counts were bumped to chase the "every SDU
    // marked Lost" issue (see build_sdc's comment). Bump this if `build_sdc` fails - it logs the
    // exact number of bytes needed.
    let mut sdc_mem = sdc::Mem::<16384>::new();
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    let cis_manager = CisManager::<NoopRawMutex, { basic_audio_sink::MAX_ASES }>::new();

    defmt::info!("Running ble audio sink example");

    select(basic_audio_sink::run(sdc, &cis_manager, None), drive_led(&mut led, &cis_manager)).await;
    unreachable!("both branches above loop forever")
}
