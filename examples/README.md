# Demos: streaming and remote control over real Bluetooth hardware

Two independent demos live here:

1. **Stereo audio round trip** - `linux`'s `audio_source` streaming to an LE Audio sink.
2. **Media control remote** - `rp-pico-w` as a pure MCS remote (no audio of its own).

Both build on `examples/apps` for the actual LE Audio logic (GATT services, ASE Control Point,
CIG/CIS/ISO setup); the platform crates just supply the HCI transport and local I/O.

## Demo 1: stereo audio round trip

**Source** - a Linux box with a Bluetooth adapter (from `examples/linux`):

```sh
cargo run --bin audio_source [device-number]
```

`device-number` selects the HCI adapter (`hci0` = `0`, the default if omitted) - see
`bt-hci-linux`'s `Transport::new`. The source connects to the sink's fixed address
(`basic_audio_sink::ADDRESS`), pairs (JustWorks), negotiates both Sink ASEs, and streams a
generated two-tone test signal (440 Hz left, 880 Hz right).

**Sink** - either of:

- `examples/linux`'s `audio_sink`: Linux as the sink peripheral, playing decoded audio through
  PipeWire. `cargo run --bin audio_sink [device-number]` - use a *different* adapter than the
  source's.
- `examples/nRF54L15_Connect_Kit`'s `sink`: real embedded hardware - see its own README.

Both sinks advertise the same fixed address, so don't run two at once on visible adapters.
`examples/linux-serial` is the same sink/source pair over an HCI-UART (serial) transport instead
of a raw HCI socket.

## Demo 2: `rp-pico-w` media control remote

The Pico W's cyw43439 controller doesn't support LE Isochronous Channels (CIS/BIS), so it can't
stream LE Audio at all - instead it runs `media_control`: a GATT peripheral exposing the (Generic)
Media Control Service. A paired central (e.g. a phone) drives Play/Pause over the Media Control
Point; the BOOTSEL button toggles the same state from the device side.

Flash it from `examples/rp-pico-w`:

```sh
cargo run --bin media_control --release
```

Downloads the cyw43 Wi-Fi/Bluetooth firmware on first build (see `build.rs`); pass
`--features skip-cyw43-firmware` to skip that during iteration (Bluetooth won't work without the
firmware, but it compiles/links).

Logging is over USB-serial (`embassy-usb-logger`), not defmt/RTT - no debug probe needed, and it
matches this crate's `elf2uf2-rs` flashing runner (drag-and-drop over BOOTSEL). After flashing:

```sh
screen /dev/ttyACM0 115200
```

(device name varies by OS - check `dmesg`/`ls /dev/tty.*` after plugging in).
