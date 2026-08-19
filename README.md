# trouble_audio

LE Audio on top of [`trouble-host`](https://github.com/embassy-rs/trouble): PACS/ASCS/BAP GATT
services, ASE Control Point, CIG/CIS/ISO setup, LC3 encode/decode (or raw-LC3 passthrough).

Unicast plus a broadcast (Auracast) **source** (`big.rs`: announcement + BASE periodic
advertising + BIG creation); the broadcast sink role is blocked on bt-hci publishing the periodic
advertising sync events.

`trouble-audio/` is the core crate. `examples/apps` is shared sink/source/bond-store logic used by
every platform example below.

## Examples

| Example | Sink | Source | Transport | README |
| --- | :-: | :-: | --- | --- |
| [`nRF54L15_Connect_Kit`](examples/nRF54L15_Connect_Kit) | ✅ | ✅ | Real BLE (`nrf-sdc`) | [README](examples/nRF54L15_Connect_Kit/README.md) |
| [`rp-pico-w`](examples/rp-pico-w) | - | - | Real BLE (`cyw43`) | [README](examples/README.md) |
| [`linux`](examples/linux) | ✅ | ✅ | Raw HCI socket | [README](examples/linux/README.md) |
| [`linux-serial`](examples/linux-serial) | ✅ | ✅ | HCI-UART (serial) | [README](examples/linux-serial/README.md) |

`rp-pico-w` is a pure media-control remote (MCS) - its cyw43439 controller has no CIS support, so
it can't stream audio. See [`examples/README.md`](examples/README.md) for both demos.

## Testing

```sh
cd trouble-audio && cargo test --lib --features log
```

Also passes under Miri:

```sh
cd trouble-audio && MIRIFLAGS=-Zmiri-ignore-leaks cargo +nightly miri test --lib --no-default-features --features log
```

(`-Zmiri-ignore-leaks` because test fixtures deliberately `Box::leak` their `'static` GATT store
buffers.)
