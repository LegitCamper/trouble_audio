# nRF54L15 Connect Kit

Firmware for [makerdiary's nRF54L15 Connect
Kit](https://github.com/makerdiary/nrf54l15-connectkit): `sink` and `source` binaries, LE Audio
unicast over real BLE via the `nrf-sdc` SoftDevice Controller. No speaker on this board - `sink`
proves audio is flowing by driving the Green LED (P0.2) on/off with decoded peak amplitude.

## Status

| | Verified on hardware | Notes |
| --- | --- | --- |
| Pairing + CIS/ISO | ✅ | JustWorks, real stereo connection against a Samsung Galaxy S23+ |
| LC3 decode + LED indicator | ✅ | Both the boot self-test blink and real playback |
| Reconnect (e.g. track skip) | ✅ | No longer OOM-panics after repeated reconnects |
| `source` | ❌ | Builds clean, covered by `trouble_audio::cig`'s unit tests, not yet run on hardware |
| Bond persistence across reflash | ❌ | Implemented (`src/bond_store.rs`, on-chip RRAM), not yet confirmed end-to-end |
| Auracast source/sink controller plumbing | ❌ | Compiles against nRF SDC BIS support; host lifecycle is unit-tested, radio validation pending |

**Known open issue**: real ISO packets often show up with `data_len=0` even after a clean CIS
establishment - not yet root-caused, likely `nrf-sdc` controller-level (ISO buffer/timing). See
`build_sdc`'s comments in `src/bin/sink.rs`/`src/bin/source.rs`.

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
