#![no_std]

pub mod button;

use embassy_rp::Peri;
use embassy_rp::dma;
use embassy_rp::peripherals::{PIN_11, PIN_12, PIO1};
use embassy_rp::pio::{Common, StateMachine};

/// Peripherals `pio_audio::audio_handler` needs: PIO1 (free for this - PIO0 drives the cyw43
/// Wi-Fi/Bluetooth SPI), one state machine and one DMA channel per stereo PWM output pin.
pub struct Audio {
    pub pio: Common<'static, PIO1>,
    pub sm0: StateMachine<'static, PIO1, 0>,
    pub sm1: StateMachine<'static, PIO1, 1>,
    /// GPIO12 (physical pin 16).
    pub left: Peri<'static, PIN_12>,
    /// GPIO11 (physical pin 15).
    pub right: Peri<'static, PIN_11>,
    pub dma0: dma::Channel<'static>,
    pub dma1: dma::Channel<'static>,
}
