//! Minimal nRF54L15 I2S20 transmit driver for the sink example.
//!
//! Embassy 0.11 exposes the nRF54L15 I2S PAC but does not yet implement its high-level `I2S`
//! instance. This module can go away once the pinned Embassy revision supports `I2S20`.

use core::future::poll_fn;
use core::mem::size_of;
use core::sync::atomic::{Ordering, compiler_fence};
use core::task::Poll;

use embassy_nrf::gpio::{Level, Output, OutputDrive, Pin};
use embassy_nrf::interrupt::typelevel::{Handler, Interrupt};
use embassy_nrf::{Peri, pac, peripherals};
use embassy_sync::waitqueue::AtomicWaker;
use static_cell::StaticCell;

/// Four LC3 frames per DMA buffer leave enough time to queue around radio scheduling.
pub const LC3_FRAMES_PER_DMA_BUFFER: usize = 4;

/// Each resampled LC3 frame is at most 477 stereo frames.
pub const MAX_STEREO_SAMPLES: usize = 477 * LC3_FRAMES_PER_DMA_BUFFER * 2;

static TX_WAKER: AtomicWaker = AtomicWaker::new();

#[repr(C, align(4))]
struct DmaBuffers([[i16; MAX_STEREO_SAMPLES]; 2]);

impl DmaBuffers {
    const fn new() -> Self {
        Self([[0; MAX_STEREO_SAMPLES]; 2])
    }
}

static DMA_BUFFERS: StaticCell<DmaBuffers> = StaticCell::new();

pub struct InterruptHandler;

impl Handler<embassy_nrf::interrupt::typelevel::I2S20> for InterruptHandler {
    unsafe fn on_interrupt() {
        let regs = pac::I2S20;
        if regs.events_txptrupd().read() != 0 {
            regs.intenclr().write(|w| w.set_txptrupd(true));
            TX_WAKER.wake();
        }
    }
}

/// Double-buffered 16-bit stereo I2S output for a three-wire DAC.
pub struct I2sOutput {
    regs: pac::i2s::I2s,
    buffers: &'static mut DmaBuffers,
    next_buffer: usize,
}

impl I2sOutput {
    pub fn new(
        sck: Peri<'static, peripherals::P1_12>,
        lrck: Peri<'static, peripherals::P1_11>,
        sdout: Peri<'static, peripherals::P1_14>,
    ) -> Self {
        let sck_psel = sck.psel_bits();
        let lrck_psel = lrck.psel_bits();
        let sdout_psel = sdout.psel_bits();
        // Match Nordic's nrfx I2S master setup: the peripheral PSEL registers route the
        // signals, while PIN_CNF must still mark clock and data pins as outputs.
        Output::new(sck, Level::Low, OutputDrive::Standard).persist();
        Output::new(lrck, Level::Low, OutputDrive::Standard).persist();
        Output::new(sdout, Level::Low, OutputDrive::Standard).persist();

        let regs = pac::I2S20;
        let config = regs.config();
        config
            .mode()
            .write(|w| w.set_mode(pac::i2s::vals::Mode::Master));
        config.rxen().write(|w| w.set_rxen(false));
        config.txen().write(|w| w.set_txen(false));
        config.mcken().write(|w| w.set_mcken(true));
        // 32 MHz / 21, then / 32: 47_619 Hz LRCK, the closest nRF54L15 setting to 48 kHz.
        config
            .mckfreq()
            .write(|w| w.set_mckfreq(pac::i2s::vals::Mckfreq::_32mdiv21));
        config
            .ratio()
            .write(|w| w.set_ratio(pac::i2s::vals::Ratio::_32x));
        config
            .swidth()
            .write(|w| w.set_swidth(pac::i2s::vals::Swidth::_16bit));
        config
            .align()
            .write(|w| w.set_align(pac::i2s::vals::Align::Left));
        config
            .format()
            .write(|w| w.set_format(pac::i2s::vals::Format::I2s));
        config
            .channels()
            .write(|w| w.set_channels(pac::i2s::vals::Channels::Stereo));

        let psel = regs.psel();
        // The UDA1334A derives its internal clock from BCLK, so no physical MCK pin is needed.
        psel.mck().write_value(pac::shared::regs::Psel(1 << 31));
        psel.sck().write_value(sck_psel);
        psel.lrck().write_value(lrck_psel);
        psel.sdin().write_value(pac::shared::regs::Psel(1 << 31));
        psel.sdout().write_value(sdout_psel);
        cortex_m::asm::dsb();

        regs.events_txptrupd().write_value(0);
        regs.intenset().write(|w| w.set_txptrupd(true));
        embassy_nrf::interrupt::typelevel::I2S20::unpend();
        // SAFETY: `bind_interrupts!` installs `InterruptHandler`, and this private constructor
        // consumes the three fixed singleton pin tokens so safe code cannot create a second owner.
        unsafe { embassy_nrf::interrupt::typelevel::I2S20::enable() };

        Self {
            regs,
            buffers: DMA_BUFFERS.init_with(DmaBuffers::new),
            next_buffer: 0,
        }
    }

    pub fn buffer(&mut self) -> &mut [i16; MAX_STEREO_SAMPLES] {
        &mut self.buffers.0[self.next_buffer]
    }

    pub async fn start(&mut self, stereo_samples: usize) {
        self.regs.enable().write(|w| w.set_enable(true));
        cortex_m::asm::dsb();
        self.queue_buffer(stereo_samples);
        self.next_buffer ^= 1;
        self.regs.tasks_start().write_value(1);
        cortex_m::asm::dsb();
        self.wait_tx_ptr_update().await;
    }

    pub async fn send(&mut self, stereo_samples: usize) {
        self.queue_buffer(stereo_samples);
        self.next_buffer ^= 1;
        self.wait_tx_ptr_update().await;
    }

    fn queue_buffer(&mut self, stereo_samples: usize) {
        assert!(stereo_samples <= MAX_STEREO_SAMPLES && stereo_samples.is_multiple_of(2));
        let buffer = &self.buffers.0[self.next_buffer][..stereo_samples];
        let ptr = buffer.as_ptr() as u32;
        assert!(ptr.is_multiple_of(4));

        compiler_fence(Ordering::SeqCst);
        // nRF54L15 RXTXD.MAXCNT counts bytes. TXPTRUPD is generated after the peripheral
        // consumes ceil(MAXCNT / 4) 32-bit words from the DMA buffer.
        let dma_bytes = stereo_samples * size_of::<i16>();
        self.regs
            .rxtxd()
            .maxcnt()
            .write(|w| w.set_maxcnt(dma_bytes as u16));
        self.regs.txd().ptr().write_value(ptr);
        self.regs.config().txen().write(|w| w.set_txen(true));
        cortex_m::asm::dsb();
    }

    async fn wait_tx_ptr_update(&self) {
        poll_fn(|cx| {
            TX_WAKER.register(cx.waker());
            if self.regs.events_txptrupd().read() == 0 {
                Poll::Pending
            } else {
                self.regs.events_txptrupd().write_value(0);
                self.regs.intenset().write(|w| w.set_txptrupd(true));
                compiler_fence(Ordering::SeqCst);
                Poll::Ready(())
            }
        })
        .await
    }
}
