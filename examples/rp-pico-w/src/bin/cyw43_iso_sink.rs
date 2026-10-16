#![no_std]
#![no_main]

//! Experimental RP2040W LC3 sink over the CYW43439 resident ISO RX patch.
//!
//! Blinks the onboard LED while LC3 audio is arriving, like the nRF54L15 `sink` binary does.
//!
//! This is **not** an Auracast receiver. The resident RX image owns a fixed, unencrypted,
//! single-BIS radio configuration (2404 MHz, AA `0x718ebed6`, CRC init `0x123401`) and has no
//! BIGInfo-derived anchor, so it cannot discover or synchronize to a standard broadcast. It
//! receives only from a transmitter already using that exact identity - currently the
//! `marker-tx --features marker-lc3` fixture in `cyw43_iso`. `trouble_host` cannot own this
//! session: its
//! startup reset removes the resident hooks, and the image implements no BIG Create Sync.

use bt_hci::param::ConnHandle;
use bt_hci::transport::Transport;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0, USB};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma, usb};
use embassy_time::Duration;
use embedded_alloc::LlffHeap as Heap;
use panic_probe as _;
use static_cell::StaticCell;
use trouble_audio::generic_audio::{FrameDuration, SamplingFrequency};
use trouble_audio::iso_rx::SduReassembler;
use trouble_audio::lc3::Lc3MonoDecoder;

const HEAP_SIZE: usize = 16 * 1024;
const ISO_HANDLE: u16 = 0x0042;
const FRAME_SAMPLES: usize = 80;
const SAMPLING_FREQUENCY: SamplingFrequency = SamplingFrequency::Hz8000;
const FRAME_DURATION: FrameDuration = FrameDuration::Duration10MS;

/// Peak `i16` magnitude below which a decoded frame counts as silence, matching the nRF54L15
/// `sink` binary's threshold. Ignores LC3 dither on true digital silence without measuring
/// real loudness.
const LED_ON_THRESHOLD: u16 = 512;

/// Maximum gap before reception is considered stopped and the LED is forced off.
const RX_TIMEOUT: Duration = Duration::from_millis(120);

/// Toggle every 25 decoded frames: 250 ms at the fixed 10-ms [NAME_8ecb7ed05d86] cadence, visibly
/// blinking at 2 Hz while non-silent LC3 arrives.
const LED_TOGGLE_FRAMES: u32 = 25;

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

/// Serves `log::info!`/etc over a USB CDC-ACM serial port, so no debug probe is needed.
#[embassy_executor::task]
async fn logger_task(driver: usb::Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Debug, driver);
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    {
        use core::mem::MaybeUninit;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        #[allow(static_mut_refs)]
        unsafe {
            HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE)
        }
    }

    let p = embassy_rp::init(Default::default());

    spawner.spawn(logger_task(usb::Driver::new(p.USB, Irqs)).unwrap());

    let fw = cyw43_firmware::CYW43439_WIFI;
    let btfw = cyw43::iso::BOOT_FIRMWARE;
    let nvram = cyw43_firmware::CYW43439_NVRAM_RP2040;

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        cyw43_pio::DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
        dma::Channel::new(p.DMA_CH1, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (_net_device, bt_device, mut control, mut runner) =
        cyw43::new_with_bluetooth(state, pwr, spi, fw, btfw, nvram).await;
    embassy_time::Timer::after_secs(2).await;

    // The resident patch needs exclusive ownership of the unstarted runner, so installation and
    // start happen before any concurrent runner task exists.
    let mut installed = match cyw43::iso::install_rx_stream_diagnostic(&mut runner).await {
        Ok(installed) => installed,
        Err(error) => {
            log::error!("CYW43 ISO RX install failed: {error:?}");
            core::future::pending().await
        }
    };
    if let Err(error) = installed.start(&mut runner).await {
        log::error!("CYW43 ISO RX start failed: {error:?}");
        core::future::pending().await
    }
    log::info!(
        "CYW43 native RX started: handle=0x{:04x}, 2404 MHz, 8 kHz, 10 ms, 26 octets",
        ISO_HANDLE
    );

    // `Runner::run` owns the bus from here, so the LED is driven through the same runner by the
    // borrowed-run loop below rather than by a separate task.
    let stop = core::sync::atomic::AtomicBool::new(false);
    embassy_futures::join::join(
        runner.run_borrowed_until(&stop),
        blink_while_receiving(&bt_device, &mut control),
    )
    .await;
    core::future::pending().await
}

/// Drains received ISO packets, decodes each LC3 SDU, and holds the onboard LED on while frames
/// are non-silent.
async fn blink_while_receiving(
    driver: &cyw43::bluetooth::BtDriver<'_>,
    control: &mut cyw43::Control<'_>,
) -> ! {
    let mut decoder = match Lc3MonoDecoder::new(SAMPLING_FREQUENCY, FRAME_DURATION) {
        Ok(decoder) => decoder,
        Err(_) => {
            log::error!("LC3 decoder rejected mandatory 8 kHz mode");
            core::future::pending().await
        }
    };
    if decoder.samples_per_frame != FRAME_SAMPLES {
        log::error!("LC3 decoder returned an unexpected frame size");
        core::future::pending().await
    }

    let mut reassembler = SduReassembler::default();
    let mut bytes = [0u8; 1024];
    let mut pcm = [0i16; FRAME_SAMPLES];
    let mut led_on = false;
    let mut decoded = 0u32;
    let mut audible_frames = 0u32;

    loop {
        let packet = match embassy_time::with_timeout(
            RX_TIMEOUT,
            driver.read::<bt_hci::ControllerToHostPacket>(&mut bytes),
        )
        .await
        {
            Ok(Ok(bt_hci::ControllerToHostPacket::Iso(packet))) => packet,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => {
                log::error!("CYW43 HCI read failed");
                core::future::pending().await
            }
            Err(_) => {
                if led_on {
                    led_on = false;
                    control.gpio_set(0, false).await;
                }
                audible_frames = 0;
                continue;
            }
        };
        if packet.handle() != ConnHandle::new(ISO_HANDLE) {
            continue;
        }
        let sdu = match reassembler.push(&packet) {
            Ok(Some(sdu)) => sdu,
            Ok(None) => continue,
            Err(reason) => {
                log::warn!("ISO SDU dropped: {reason:?}");
                continue;
            }
        };
        if decoder.decode(&sdu.data, &mut pcm).is_err() {
            log::warn!("LC3 decode failed for SDU {}", sdu.sequence_number);
            continue;
        }
        decoded += 1;
        let peak = pcm
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap_or(0);
        if peak >= LED_ON_THRESHOLD {
            audible_frames += 1;
            if audible_frames.is_multiple_of(LED_TOGGLE_FRAMES) {
                led_on = !led_on;
                control.gpio_set(0, led_on).await;
                log::info!("LC3 LED {} after {} audible frames", led_on, audible_frames);
            }
        } else {
            audible_frames = 0;
            if led_on {
                led_on = false;
                control.gpio_set(0, false).await;
            }
        }
        if decoded % 100 == 1 {
            log::info!(
                "decoded LC3 SDU {} (count {}, peak {}, led {})",
                sdu.sequence_number,
                decoded,
                peak,
                led_on
            );
        }
    }
}
