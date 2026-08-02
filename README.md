# trouble_audio

LE Audio on top of [`trouble-host`](https://github.com/embassy-rs/trouble): PACS/ASCS/BAP GATT
services, ASE Control Point, CIG/CIS/ISO setup, LC3 encode/decode.

Unicast today; broadcast (Auracast) is a planned direction, not yet implemented.

`trouble-audio/` is the core crate. `examples/apps` is shared sink/source/bond-store logic used by
every platform example below.

## Examples

| Example | Sink | Source | Transport | README |
| --- | :-: | :-: | --- | --- |
| [`nRF54L15_Connect_Kit`](examples/nRF54L15_Connect_Kit) | ✅ | ✅ | Real BLE (`nrf-sdc`) | [README](examples/nRF54L15_Connect_Kit/README.md) |
| [`rp-pico-w`](examples/rp-pico-w) | ✅ | | Real BLE (`cyw43`) | [README](examples/README.md) |
| [`linux`](examples/linux) | ✅ | ✅ | Raw HCI socket | [README](examples/linux/README.md) |
| [`linux-serial`](examples/linux-serial) | ✅ | ✅ | HCI-UART (serial) | [README](examples/linux-serial/README.md) |

`rp-pico-w` pairs with `linux`'s source for a full over-the-air round trip - see
[`examples/README.md`](examples/README.md).

## Testing

```sh
cd trouble-audio && cargo test --lib --no-default-features --features log
```

Also passes under Miri:

```sh
cd trouble-audio && MIRIFLAGS=-Zmiri-ignore-leaks cargo +nightly miri test --lib --no-default-features --features log
```

(`-Zmiri-ignore-leaks` because `trouble_audio::lc3` deliberately leaks its LC3 working buffers by
design - see that module's docs.)
