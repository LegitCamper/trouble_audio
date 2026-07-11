# End-to-end demo: stereo LE Audio over real Bluetooth hardware

Two pieces, two crates:

- **`rp-pico-w`** - a Raspberry Pi Pico W running the LE Audio **sink** (`audio_sink`): advertises
  a 2-ASE stereo Sink (front left/right), and plays whatever it receives out an aux jack via
  PIO-driven PWM.
- **`linux`** - a Linux box running the LE Audio **source** (`audio_source`): connects to the
  Pico's sink, negotiates both channels, and streams a generated two-tone test signal (440 Hz
  left, 880 Hz right) to it.

Together they're a full over-the-air round trip with no phone/BlueZ involved on either side - both
ends speak raw HCI directly via `trouble-host`.

Both use `examples/apps`'s `basic_audio_sink`/`basic_audio_source` for the actual LE Audio logic
(GATT services, ASE Control Point, CIG/CIS/ISO setup); the platform-specific `rp-pico-w`/`linux`
crates just supply the HCI transport and real audio I/O.

## Hardware (rp-pico-w)

Stereo aux output is wired via two PIO-PWM channels (`src/pio_audio.rs`):

| Signal | GPIO | Physical pin |
| --- | --- | --- |
| Left  | GP12 | 16 |
| Right | GP11 | 15 |
| GND   | -    | 18 |

Each needs an RC low-pass filter between the GPIO and the aux jack tip/ring to turn the PWM output
into an analog signal - the PIO program only produces the PWM square wave itself.

## Logging (rp-pico-w)

`audio_sink` logs over USB-serial (`embassy-usb-logger`, via `log::`), not defmt/RTT - no debug
probe needed, and it matches this crate's `elf2uf2-rs` flashing runner (drag-and-drop over
BOOTSEL, not probe-rs). After flashing, connect with any serial terminal, e.g.:

```sh
screen /dev/ttyACM0 115200
```

(device name varies by OS - check `dmesg`/`ls /dev/tty.*` after plugging in).

## Running it

1. **Flash the sink** onto the Pico W (from `examples/rp-pico-w`):

   ```sh
   cargo run --bin audio_sink --release
   ```

   Downloads the cyw43 Wi-Fi/Bluetooth firmware on first build (see `build.rs`); pass
   `--features skip-cyw43-firmware` to skip that during iteration if you don't have the firmware
   files yet (Bluetooth won't actually work without them, but it'll compile/link).

2. **Run the source** on a Linux box with a Bluetooth adapter (from `examples/linux`):

   ```sh
   cargo run --bin audio_source [device-number]
   ```

   `device-number` selects the HCI adapter (`hci0` = `0`, the default if omitted) - see
   `bt-hci-linux`'s `Transport::new`.

The source connects to the sink's fixed address (`basic_audio_sink::ADDRESS`), pairs (JustWorks -
the sink declares `IoCapabilities::NoInputNoOutput`), negotiates both Sink ASEs, and starts
streaming. You should hear the two-tone test signal through whatever's wired to the Pico's aux
output.

## The other sink: `linux`'s own `audio_sink`

`examples/linux` also has its own `audio_sink` binary - Linux acting as the sink peripheral itself
(playing decoded audio through PipeWire instead of real hardware). Useful for testing
`audio_source` without a Pico W handy, or for testing the sink role without real aux hardware:

```sh
cargo run --bin audio_sink [device-number]
```

Point `audio_source` at it the same way (it always targets `basic_audio_sink::ADDRESS`, and both
`audio_sink` binaries advertise the same one) - just don't run both `audio_sink` binaries at once
on address-visible adapters, or a central won't know which one it's actually talking to.
