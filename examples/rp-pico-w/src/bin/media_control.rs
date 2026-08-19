#![no_std]
#![no_main]

#[cfg(not(feature = "skip-cyw43-firmware"))]
use cyw43::aligned_bytes;
use cyw43::Cyw43439;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{BOOTSEL, DMA_CH0, DMA_CH1, PIO0, USB};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma, usb, Peri};
use embedded_alloc::LlffHeap as Heap;
use panic_probe as _;
use static_cell::StaticCell;
use trouble_audio_example_apps::media_control;
use trouble_audio_examples::button;
use trouble_host::prelude::ExternalController;

const CONTROLLER_SLOTS: usize = 10;

/// `trouble_audio` uses `alloc` for LE Audio's variable-length data (PAC records, codec
/// configuration, metadata, ...), so a global allocator must be installed even though this
/// example's plain MCS peripheral never actually allocates anything itself.
const HEAP_SIZE: usize = 16 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>, Cyw43439>,
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

#[embassy_executor::task]
async fn button_task(bootsel: Peri<'static, BOOTSEL>) {
    button::poll(bootsel).await
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

    #[cfg(feature = "skip-cyw43-firmware")]
    let (fw, clm, btfw, nvram) = {
        static EMPTY: &cyw43::Aligned<cyw43::A4, [u8]> = &cyw43::Aligned([0u8; 0]);
        (EMPTY, &[] as &[u8], EMPTY, EMPTY)
    };

    #[cfg(not(feature = "skip-cyw43-firmware"))]
    let (fw, clm, btfw, nvram) = {
        // IMPORTANT
        //
        // Download and make sure these files from https://github.com/embassy-rs/embassy/tree/main/cyw43-firmware
        // are available in `./examples/rp-pico-w`. (should be automatic)
        //
        // IMPORTANT
        let fw = aligned_bytes!("../../cyw43-firmware/43439A0.bin");
        let clm = aligned_bytes!("../../cyw43-firmware/43439A0_clm.bin");
        let btfw = aligned_bytes!("../../cyw43-firmware/43439A0_btfw.bin");
        let nvram = aligned_bytes!("../../cyw43-firmware/nvram_rp2040.bin");
        (fw, clm, btfw, nvram)
    };

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
    let (_net_device, bt_device, mut control, runner) =
        cyw43::new_with_bluetooth(state, pwr, spi, fw, btfw, nvram).await;
    spawner.spawn(cyw43_task(runner).unwrap());
    control.init(clm).await;

    // The cyw43439's Bluetooth controller doesn't support LE Isochronous Channels (BIS/CIS), so
    // it can't send or receive real LE Audio at all - `ExternalController` still works fine as a
    // plain GATT peripheral, which is all `media_control::run` needs. This device is purely a
    // remote control: it plays no audio of its own, and controls the playback of whatever other
    // device a central pairs it with.
    let controller: ExternalController<_, CONTROLLER_SLOTS> =
        ExternalController::new(trouble_audio_examples::hci_bridge::Cyw43Transport::new(bt_device));

    // Polls the BOOTSEL button and signals `media_control::BUTTON_PRESSED` on each press -
    // `media_control::run` reacts to that the same way it reacts to a central's Media Control
    // Point Play/Pause write.
    spawner.spawn(button_task(p.BOOTSEL).unwrap());

    media_control::run(controller, None).await;
}
