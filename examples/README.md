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

## Demo 2: `rp-pico-w`

`media_control` is a normal GATT peripheral exposing the (Generic) Media Control Service. A paired
central (e.g. a phone) drives Play/Pause over the Media Control Point; the BOOTSEL button toggles
the same state from the device side.

The two `cyw43_iso_*` binaries use an experimental resident CYW43439 controller patch instead:

- `cyw43_iso_source` encodes a synthetic 8-kHz mono signal to 26-byte LC3 SDUs and broadcasts it.
  A stock Nordic SDC receiver has discovered and decoded this stream through normal periodic/BIG
  synchronization, but the session uses a fixed diagnostic HCI handle rather than Create BIG.
- `cyw43_iso_sink` receives and decodes the same LC3 format, blinking the Pico W onboard LED at
  2 Hz while non-silent frames arrive. Its fixed-channel RX image has no BIGInfo-derived anchor,
  so this is not yet an Auracast sink; use the `cyw43_iso` nRF54 `marker-tx` binary with feature
  `marker-lc3` as its matching source.

Build or flash from `examples/rp-pico-w` (its Cargo config selects the old trait solver because
current nightly otherwise ICEs in `generic_const_exprs`):

```sh
cargo run --bin cyw43_iso_sink --release --features cyw43-iso
cargo run --bin cyw43_iso_source --release --features cyw43-iso
```

`media_control` is feature-gated separately because Trouble's security feature requires an
`embassy-crypto` AES/P-256/RNG backend. The RP2040 example has no secure RNG backend wired yet;
keeping it out of `cyw43-iso` builds prevents an unrelated remote-control binary from introducing
undefined crypto-driver symbols into the LC3 source/sink build.

Downloads the cyw43 Wi-Fi/Bluetooth firmware on first build (see `build.rs`); pass
`--features skip-cyw43-firmware` to skip that during iteration (Bluetooth won't work without the
firmware, but it compiles/links).

Logging is over USB-serial (`embassy-usb-logger`), not defmt/RTT - no debug probe needed, and it
matches this crate's `elf2uf2-rs` flashing runner (drag-and-drop over BOOTSEL). After flashing:

```sh
screen /dev/ttyACM0 115200
```

(device name varies by OS - check `dmesg`/`ls /dev/tty.*` after plugging in).
