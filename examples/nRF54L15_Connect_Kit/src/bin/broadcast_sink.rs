#![no_std]
#![no_main]

use core::mem::MaybeUninit;

use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_nrf::{bind_interrupts, config, cracen, mode::Blocking};
use embedded_alloc::LlffHeap as Heap;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use nrf54l15_connect_kit::run_auracast_sink;
use static_cell::StaticCell;
// Links the software AES/P-256 backend behind trouble-host's `security` feature.
use {defmt_rtt as _, embassy_crypto_rustcrypto as _, panic_probe as _};

/// `auracast_sink` keeps one `Lc3MonoDecoder` per BIS; sized the same way as `bin/sink.rs`.
const HEAP_SIZE: usize = 96 * 1024;

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

fn build_sdc<'d, const N: usize>(
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut cracen::Cracen<'static, Blocking>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    sdc::Builder::new()?
        // A broadcast sink only scans: extended scanning finds the Broadcast Audio Announcement
        // and periodic sync delivers the BASE.
        .support_scan()
        .support_ext_scan()
        .support_le_periodic_sync()
        .support_le_2m_phy()
        .support_bis_sink()
        // Without these, `LE BIG Create Sync` fails with "Connection Rejected due to Limited Resources".
        .big_count(1)?
        .bis_sink_count(2)?
        .periodic_sync_count(1)?
        .periodic_sync_buffer_cfg(2)?
        // Dense legacy advertising can starve the AUX report carrying the broadcast announcement.
        .scan_buffer_cfg(8)?
        // Like `support_cis_*`, this only enables the capability - without ISO RX buffers
        // `LE BIG Create Sync` fails with `MEMORY_CAPACITY_EXCEEDED`.
        .iso_buffer_cfg(0, 0, 0, 6, 8, 128)?
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
    // The embassy-crypto RNG driver must be seeded before the BLE stack is built.
    {
        let mut seed = [0u8; 32];
        rng.blocking_fill_bytes(&mut seed);
        nrf54l15_connect_kit::csprng::seed(seed);
    }

    // Bump if `build_sdc` fails - it logs the exact number of bytes needed.
    static SDC_MEM: StaticCell<sdc::Mem<16384>> = StaticCell::new();
    let sdc_mem = SDC_MEM.init_with(sdc::Mem::new);
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, sdc_mem));

    defmt::info!("Running Auracast broadcast sink example");

    run_auracast_sink(sdc).await
}
