//! Polls the Pico's BOOTSEL button and signals [`media_control::BUTTON_PRESSED`] once per press.
//!
//! BOOTSEL has no dedicated GPIO - it's multiplexed onto the QSPI flash's CS pin, so reading it
//! (`embassy_rp::bootsel::is_bootsel_pressed`) is a synchronous, non-trivial operation (briefly
//! disables flash XIP) rather than a simple pin read, and so isn't interrupt-driven; this task
//! polls it instead.

use embassy_rp::bootsel::is_bootsel_pressed;
use embassy_rp::peripherals::BOOTSEL;
use embassy_rp::Peri;
use embassy_time::{Duration, Timer};
use trouble_audio_example_apps::media_control;

const POLL_INTERVAL: Duration = Duration::from_millis(30);

/// Polls forever, signalling [`media_control::BUTTON_PRESSED`] on each press's leading edge -
/// debounced by construction, since a press is only ever signalled once per press-and-release.
pub async fn poll(mut bootsel: Peri<'static, BOOTSEL>) -> ! {
    let mut was_pressed = false;
    loop {
        let pressed = is_bootsel_pressed(bootsel.reborrow());
        if pressed && !was_pressed {
            media_control::BUTTON_PRESSED.signal(());
        }
        was_pressed = pressed;
        Timer::after(POLL_INTERVAL).await;
    }
}
