#![no_std]
#![no_main]

use core::cell::RefCell;
use core::mem::MaybeUninit;

use bt_hci::controller::ControllerCmdSync;
use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::{bind_interrupts, config, cracen, mode::Blocking};
use embedded_alloc::LlffHeap as Heap;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::vendor::NordicCigReservedTimeSet;
use nrf_sdc::{self as sdc, mpsl};
use nrf54l15_connect_kit::bond_store::rram_bond_store;
use static_cell::{ConstStaticCell, StaticCell};
use trouble_audio::generic_audio::{FrameDuration, SamplingFrequency};
use trouble_audio::lc3::Lc3MonoEncoder;
use trouble_audio_example_apps::{basic_audio_sink, basic_audio_source};
use trouble_host::prelude::*;
// Links the software AES/P-256 backend behind trouble-host's `security` feature.
use {defmt_rtt as _, embassy_crypto_rustcrypto as _, panic_probe as _};

/// Dominated by `Lc3MonoEncoder`'s working buffers, one per channel (`CHANNEL_COUNT` = 2) - see
/// `trouble_audio::lc3` for the `heap_bytes`/`const`-assertion below that keeps this sized
/// correctly at build time.
const HEAP_SIZE: usize = 64 * 1024;

/// Matches `basic_audio_source::run`'s hardcoded (not `pub`) negotiation.
const NEGOTIATED_SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz48000;
const NEGOTIATED_FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;

/// `basic_audio_source::run` always streams a fixed stereo pair - see its `ase_ids: [u8; 2]`.
const CHANNEL_COUNT: usize = 2;

/// Headroom for everything else `alloc`-backed - not computed exactly like the LC3 buffers below.
const MISC_ALLOC_BUDGET_BYTES: usize = 16 * 1024;

const ENCODER_HEAP_BYTES: usize =
    match Lc3MonoEncoder::heap_bytes(NEGOTIATED_SAMPLING_FREQUENCY, NEGOTIATED_FRAME_DURATION) {
        Ok(n) => n,
        Err(_) => panic!("unsupported sampling frequency"),
    };
const _: () = assert!(
    HEAP_SIZE >= CHANNEL_COUNT * ENCODER_HEAP_BYTES + MISC_ALLOC_BUDGET_BYTES,
    "HEAP_SIZE too small to fit one Lc3MonoEncoder per channel plus misc allocation headroom"
);

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
        // Zephyr's bap_unicast_server advertises with extended advertising only, which a legacy
        // initiator cannot see, so this connects with `LE Extended Create Connection`.
        .support_ext_scan()
        .support_ext_central()
        // Extended advertisers put AUX_ADV_IND on the secondary PHY, which Zephyr defaults to 2M,
        // so without 2M support the server's connectable ext adv is never seen.
        .support_le_2m_phy()
        .support_cis_central()
        .central_count(1)?
        .buffer_cfg(
            DefaultPacketPool::MTU as u16,
            DefaultPacketPool::MTU as u16,
            L2CAP_TXQ,
            L2CAP_RXQ,
        )?
        // `support_cis_central()` only enables the capability - CIG/CIS/ISO buffer counts
        // default to 0, so `LE Create CIS` fails with `MEMORY_CAPACITY_EXCEEDED`.
        .cig_count(1)?
        .cis_count(2)?
        .iso_buffer_cfg(4, 128, 2, 1, 2, 128)?
        .build(p, rng, mpsl, mem)
}

/// Peer to connect to. Defaults to this crate's own sink example; set `TROUBLE_AUDIO_TARGET` at
/// build time (e.g. `c0:fd:94:b5:ae:d6`) to point at a foreign peer such as Zephyr's
/// `bap_unicast_server`, whose identity address is fixed per board.
fn target_address() -> [u8; 6] {
    match option_env!("TROUBLE_AUDIO_TARGET") {
        None => basic_audio_sink::ADDRESS,
        Some(text) => {
            let bytes = text.as_bytes();
            assert!(
                bytes.len() == 17,
                "TROUBLE_AUDIO_TARGET must look like aa:bb:cc:dd:ee:ff"
            );
            let mut addr = [0u8; 6];
            // Addresses are written most-significant octet first, but `Address::random` takes
            // them little-endian, as on the wire.
            let mut i = 0;
            while i < 6 {
                let hi = hex_digit(bytes[i * 3]);
                let lo = hex_digit(bytes[i * 3 + 1]);
                addr[5 - i] = hi << 4 | lo;
                i += 1;
            }
            addr
        }
    }
}

const fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("TROUBLE_AUDIO_TARGET must be hex octets separated by ':'"),
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

    // ExternalXtal, not RC: CIS/ISO timing needs crystal-grade LFCLK accuracy - RC caused
    // `LE Set CIG Parameters` to be rejected with "Unsupported Feature or Parameter Value".
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
    let mut sdc_mem = sdc::Mem::<12288>::new();
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    // Frees the default ACL/other-role time reservation for the CIG itself - without this,
    // `LE Set CIG Parameters` was rejected with "Unsupported Feature or Parameter Value".
    unwrap!(sdc.exec(&NordicCigReservedTimeSet::new(0)).await);

    // Persists the bond to on-chip RRAM so re-pairing isn't needed after every reflash.
    let flash = RefCell::new(Nvmc::new(p.RRAMC));
    let bond_store = rram_bond_store(&flash);

    defmt::info!("Running ble audio source example");

    basic_audio_source::run(
        sdc,
        OUR_ADDRESS,
        target_address(),
        Some(&bond_store),
        nrf54l15_connect_kit::audio_preset(),
    )
    .await
}
