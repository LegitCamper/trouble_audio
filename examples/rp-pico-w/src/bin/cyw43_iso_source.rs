#![no_std]
#![no_main]

//! Experimental RP2040W LC3 source over the CYW43439 resident ISO patch.
//!
//! The patch owns a fixed unencrypted mono radio configuration, so this reuses `trouble_audio`'s
//! LC3 encoder and HCI ISO framing without pretending the current image is a negotiated Auracast
//! BIG. `trouble_host` cannot own this session yet: its startup reset removes the resident hooks,
//! and the image does not implement the standard BIG command sequence.

use bt_hci::param::ConnHandle;
use bt_hci::transport::Transport;
use cyw43::Cyw43439;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0, USB};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma, usb};
use embedded_alloc::LlffHeap as Heap;
use panic_probe as _;
use static_cell::StaticCell;
use trouble_audio::generic_audio::{FrameDuration, SamplingFrequency};
use trouble_audio::iso_tx::{self, SequenceNumber};
use trouble_audio::lc3::Lc3MonoEncoder;

const HEAP_SIZE: usize = 16 * 1024;
const ISO_HANDLE: u16 = 0x0042;
const FRAME_SAMPLES: usize = 80;
const FRAME_OCTETS: usize = 26;

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<
        'static,
        cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>,
        Cyw43439,
    >,
) -> ! {
    runner.run().await
}

/// Serves `log::info!`/etc calls (this crate's dependencies' `"log"`-feature output) over a USB
/// CDC-ACM serial port - connect with any serial terminal (e.g. `screen /dev/ttyACM0 115200`)
/// rather than needing a debug probe.
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
    let (_net_device, bt_device, _control, mut runner) =
        cyw43::new_with_bluetooth(state, pwr, spi, fw, btfw, nvram).await;
    embassy_time::Timer::after_secs(2).await;
    let mut installed = match cyw43::iso::install(&mut runner).await {
        Ok(installed) => installed,
        Err(error) => {
            log::error!("CYW43 ISO install failed: {error:?}");
            core::future::pending().await
        }
    };
    if let Err(error) = installed.start(&mut runner).await {
        log::error!("CYW43 ISO start failed: {error:?}");
        core::future::pending().await
    }
    log::info!(
        "CYW43 native BIS started: handle=0x{:04x}, 8 kHz, 10 ms, 26 octets",
        ISO_HANDLE
    );
    spawner.spawn(cyw43_task(runner).unwrap());
    stream_lc3(&bt_device).await;
}

async fn stream_lc3(driver: &cyw43::bluetooth::BtDriver<'_>) -> ! {
    let mut encoder =
        match Lc3MonoEncoder::new(SamplingFrequency::Hz8000, FrameDuration::Duration10MS) {
            Ok(encoder) => encoder,
            Err(_) => {
                log::error!("LC3 encoder rejected mandatory 8 kHz mode");
                core::future::pending().await
            }
        };
    if encoder.samples_per_frame != FRAME_SAMPLES {
        log::error!("LC3 encoder returned an unexpected frame size");
        core::future::pending().await
    }
    let mut sequence = SequenceNumber::default();
    let mut ticker = embassy_time::Ticker::every(embassy_time::Duration::from_millis(10));
    let mut phase = 0u32;
    let mut packet_buffer = Default::default();
    let mut read_buffer = [0u8; 1024];

    loop {
        ticker.next().await;
        let mut pcm = [0i16; FRAME_SAMPLES];
        for sample in &mut pcm {
            *sample = if phase < 4_000 { 4_000 } else { -4_000 };
            phase = (phase + 440) % 8_000;
        }
        let mut encoded = [0u8; FRAME_OCTETS];
        if encoder.encode(&pcm, &mut encoded).is_err() {
            log::error!("LC3 encode failed");
            continue;
        }
        let Some(packet) = iso_tx::build_packet(
            &mut packet_buffer,
            ConnHandle::new(ISO_HANDLE),
            sequence.next(),
            &encoded,
        ) else {
            log::error!("LC3 frame does not fit one complete HCI ISO packet");
            core::future::pending().await
        };
        if driver.write(&packet).await.is_err() {
            log::error!("CYW43 ISO write failed");
            core::future::pending().await
        }
        if iso_tx::wait_for_completed_packet(driver, &mut read_buffer, ConnHandle::new(ISO_HANDLE))
            .await
            .is_err()
        {
            log::error!("CYW43 HCI read failed while waiting for ISO credit");
            core::future::pending().await
        }
    }
}
