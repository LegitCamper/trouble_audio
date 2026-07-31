#![no_std]
#![no_main]

use core::mem::MaybeUninit;

use bt_hci::controller::ControllerCmdSync;
use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_nrf::{bind_interrupts, config, cracen, mode::Blocking};
use embedded_alloc::LlffHeap as Heap;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::vendor::NordicCigReservedTimeSet;
use nrf_sdc::{self as sdc, mpsl};
use static_cell::StaticCell;
use trouble_audio_example_apps::{basic_audio_sink, basic_audio_source};
use trouble_host::prelude::*;
use {defmt_rtt as _, panic_probe as _};

/// `trouble_audio`/`basic_audio_source` use `alloc` for LE Audio's variable-length data (PAC
/// records, codec configuration, metadata, ...), so a global allocator must be installed.
///
/// Also has to cover `Lc3MonoEncoder`'s working buffers (`Box::leak`'d, never freed): one
/// encoder needs 15904 bytes at 48kHz/10ms (3800 integer + 4424 scaler + 7680 complex), and this
/// source runs two encoders (stereo) concurrently - budget generously (256KB total RAM).
const HEAP_SIZE: usize = 64 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

/// Address this source identifies itself with (arbitrary, matches `examples/linux/src/bin/audio_source.rs`).
const OUR_ADDRESS: [u8; 6] = [0xff, 0x8f, 0x1c, 0x05, 0xe4, 0xff];

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
        .support_scan()
        .support_central()
        .support_cis_central()
        .central_count(1)?
        .buffer_cfg(
            DefaultPacketPool::MTU as u16,
            DefaultPacketPool::MTU as u16,
            L2CAP_TXQ,
            L2CAP_RXQ,
        )?
        // `support_cis_central()` only enables the capability - CIG/CIS/ISO buffer counts all
        // default to 0, so `LE Create CIS` fails with `MEMORY_CAPACITY_EXCEEDED` no matter what.
        // One CIG/two CIS to match the stereo stream this source creates; TX-heavy since this
        // role plays audio out rather than receiving it.
        .cig_count(1)?
        .cis_count(2)?
        .iso_buffer_cfg(4, 128, 2, 1, 2, 128)?
        .build(p, rng, mpsl, mem)
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("start");
    {
        static HEAP_MEM: StaticCell<[MaybeUninit<u8>; HEAP_SIZE]> = StaticCell::new();
        let heap_mem = HEAP_MEM.init([const { MaybeUninit::uninit() }; HEAP_SIZE]);
        unsafe { HEAP.init(heap_mem.as_ptr() as usize, HEAP_SIZE) }
    }

    // ExternalXtal, not Internal/InternalRC: the earlier hang in embassy-nrf's clock init looked
    // like a missing crystal (`while events_xostarted == 0 {}` never completing) but was actually
    // an unrelated `probe-rs run` flash/reset handoff bug on this board - the crystal was fine.
    // CIS/ISO timing needs crystal-grade LFCLK accuracy; RC is good enough for connections/GATT
    // but was the likely cause of `LE Set CIG Parameters` being rejected with "Unsupported
    // Feature or Parameter Value".
    let mut config: config::Config = Default::default();
    config.clock_speed = config::ClockSpeed::CK128;
    config.hfclk_source = config::HfclkSource::ExternalXtal;
    config.lfclk_source = config::LfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);
    defmt::info!("clocks initialized");
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

    // Central + security + CIS needs more SDC memory than the plain peripheral role this was
    // copied from (4720B); nrf52's central+security example needs 7056B for central+security
    // alone. Reserving real CIG/CIS/ISO buffers (see `build_sdc`) adds more on top of that -
    // budget generously. Bump this if `build_sdc` fails - it logs the exact number of bytes
    // needed.
    let mut sdc_mem = sdc::Mem::<12288>::new();
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    // Default reserved time (1300us/ISO interval) is for concurrent ACL/other-role activity we
    // don't have (this is a single-purpose central with one connection) - free that budget for
    // the CIG itself. Without this, `LE Set CIG Parameters` for our 2-CIS/10ms-interval/RTN=2
    // stereo stream was rejected with "Unsupported Feature or Parameter Value" (doesn't fit
    // alongside the default reservation). See sdc_hci_vs.h's `sdc_hci_cmd_vs_cig_reserved_time_set`.
    unwrap!(sdc.exec(&NordicCigReservedTimeSet::new(0)).await);

    defmt::info!("Running ble audio source example");

    basic_audio_source::run(sdc, OUR_ADDRESS, basic_audio_sink::ADDRESS, None).await
}
