# nRF54L15 Connect Kit

Firmware for [makerdiary's nRF54L15 Connect
Kit](https://github.com/makerdiary/nrf54l15-connectkit): `sink` and `source` binaries, LE Audio
unicast over real BLE via the `nrf-sdc` SoftDevice Controller. The `sink` sends decoded dual-mono
PCM over I2S and also drives the Green LED (P0.2) from decoded peak amplitude, so it remains
testable without any external audio hardware.

## Status

| | Verified on hardware | Notes |
| --- | --- | --- |
| Pairing + CIS/ISO | ✅ | JustWorks, real stereo connection against a Samsung Galaxy S23+ |
| LC3 decode + LED indicator | ✅ | Both the boot self-test blink and real playback |
| I2S output | ✅ | Clocks and audible output validated with an Adafruit UDA1334A; dual-mono keeps decoding within the real-time budget |
| Reconnect (e.g. track skip) | ✅ | No longer OOM-panics after repeated reconnects |
| Two-board `source` -> `sink` | ✅ | Stereo CIG/two CIS paths, 200 ISO packets/s, and LC3 decode verified with two nRF54L15 Connect Kits |
| Bond persistence across reflash | ✅ | Both boards reconnect and encrypt from their on-chip RRAM bond after reflashing |
| Auracast source/sink controller plumbing | ❌ | Compiles against nRF SDC BIS support; host lifecycle is unit-tested, radio validation pending |

The library exposes `run_auracast_source` and `run_auracast_sink`, compile-checked against
`SoftdeviceController`. A board binary calling them must enable `support_bis_source()` or
`support_bis_sink()` respectively and provision the matching ISO TX/RX buffers in its SDC builder.

## Building and flashing

```sh
cargo run --release --bin sink    # or --bin source
```

Must be `--release` - debug builds overflow the stack during GATT server construction and corrupt
the heap. Flashes via [`probe-rs`](https://probe.rs) (see `.cargo/config.toml` - pinned to a
specific probe ID, adjust if yours differs). RTT logging is on by default.

**Known flake**: `probe-rs run` sometimes hangs between `start` and `clocks initialized` on first
flash after erasing. Press the board's reset button once; no reflash needed.

## Hardware notes

- Green LED is P0.2, active-high, plain on/off GPIO - not PWM: `SimplePwm` never drove this pin in
  testing (tried `DutyCycle::normal` and `::inverted`). The RGB LED next to it belongs to a
  separate nRF52820 interface MCU and isn't controllable from here.
- Both binaries need the external crystals (`HfclkSource`/`LfclkSource::ExternalXtal`) - CIS/ISO
  timing needs crystal-grade accuracy, the default RC clocks aren't good enough.

## Sink I2S wiring

The sink is an I2S master and emits signed 16-bit dual-mono in standard Philips I2S format. It
selects the first incoming Sink ASE, decodes that mono LC3 stream, and mirrors it to both left and
right DAC outputs. Discarding the other ASE before decoding keeps the pure-Rust LC3 workload from
starving the nRF54L15 radio and audio tasks. Nordic's
closest clock setting gives a 47.619 kHz LRCK for the negotiated 48 kHz LC3 stream; the firmware
evenly resamples each 480-sample LC3 frame to keep playback synchronized. Use short wires and a
3-wire I2S DAC/amplifier that derives its clocks from BCLK/LRCK, such as a MAX98357A breakout.
Both outputs carry the same selected channel; this example intentionally prioritizes continuous,
testable audio over distinct left/right software decoding.

The DMA output batches four LC3 frames (about 40 ms) at a time. This leaves enough scheduling
headroom for the mono LC3 decoder and radio stack to run without missing the next-buffer deadline;
if that deadline is missed, the nRF54L15 reuses the previous buffer and audio sounds robotic. The
CIS event callback only queues encoded LC3 data; decoding happens in the audio-output task.

| Connect Kit header | nRF54L15 | I2S board | Notes |
| --- | --- | --- | --- |
| Pin 26 | P1.12 | BCLK / SCK | 1.524 MHz bit clock; P1.12 is clock-capable |
| Pin 25 | P1.11 | LRC / WS | 47.619 kHz word clock |
| Pin 28 | P1.14 | DIN | Audio data from the Connect Kit |
| Pin 3 or 39 | GND | GND | A common ground is required |

These exact GPIOs matter: the nRF54L15 `I2S20` peripheral can only use port P1, and its SCK
output must use a clock-capable pin. P2 pin selections compile and the peripheral registers accept
them, but no clocks are emitted. This example does not route MCLK; use a three-wire DAC that can
derive its clock from BCLK/LRCK. See the
[Connect Kit header diagram](https://github.com/makerdiary/nrf54l15-connectkit/blob/main/docs/assets/images/gpios-pinout.png)
before wiring.

Power the audio board according to its own documentation. The Connect Kit GPIO level is selected
by its 1.8 V/3.3 V VDD_GPIO setting; do not drive an nRF54L15 GPIO above that level. A typical
MAX98357A breakout can be powered from 3.3 V when VDD_GPIO is configured for 3.3 V, but confirm
the breakout's supply range and speaker-current needs first.

Flash and test with or without the I2S board:

```sh
cargo run --release --bin sink
```

At boot the Green LED blinks three times. During playback it follows decoded audio level whether
or not anything is wired to the I2S pins. With a connected DAC/amplifier, start LE Audio playback
and listen for output; RTT should continue logging decoded-frame counts.

### Adafruit UDA1334A stereo decoder

The [Adafruit UDA1334A](https://learn.adafruit.com/adafruit-i2s-stereo-decoder-uda1334a/pinouts)
uses the three required I2S signals and derives its internal clock without MCLK:

| Connect Kit header | UDA1334A |
| --- | --- |
| Pin 1 (`VBUS`, USB 5 V) | `VIN` |
| Pin 3 (`GND`) | `GND` |
| Pin 26 (`P1.12`) | `BCLK` |
| Pin 25 (`P1.11`) | `WSEL` |
| Pin 28 (`P1.14`) | `DIN` |

Leave `SCLK` disconnected and do not pull `Mute` high. The analog jack is a line-level output with
a nominal 3-kilohm load; use powered headphones or an amplifier for reliable volume. Low-impedance
passive headphones may be quiet or distorted.
