# trouble_audio

LE Audio on top of [`trouble-host`](https://github.com/embassy-rs/trouble): PACS/ASCS/BAP GATT
services, ASE Control Point, CIG/CIS/ISO setup, LC3 encode/decode (or raw-LC3 passthrough).

Unicast plus both Auracast roles:

- `big.rs`: Broadcast Audio Announcement, BASE periodic advertising, BIG creation, encrypted or
  unencrypted broadcasts, and BIS data-path setup.
- `big_sink.rs`: discovery, fragmented BASE reassembly, periodic-advertising and BIG
  synchronization, BIS selection, encrypted Broadcast Codes, and raw-LC3 or decoded-PCM output.

The complete source-to-sink lifecycle is exercised with synthetic HCI events and ISO packets, so
the host behavior can be tested without a radio. Real-controller validation is still required.

`trouble-audio/` is the core crate. `examples/apps` includes complete reusable Auracast source and
sink loops as well as the shared unicast/bond-store logic used by the platform examples below.

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

## Temporary upstream requirements

Auracast behavior remains in this repository. The current local patches are only a compile harness
for three controller/host plumbing changes that belong upstream:

- `bt-hci`: model `BigHandle` as the one-octet HCI value and encode both `BIG_Handle` and the
  two-octet periodic `Sync_Handle` in `LE BIG Create Sync`.
- `trouble-host`: forward periodic-advertising sync/report/lost and BIG sync established/lost
  events to `EventHandler`.
- `nrf-sdc`: implement its existing controller C APIs for create/terminate BIG, BIG create/terminate
  sync, including the variable-length BIS array.

Once those changes are published, remove the corresponding local `[patch]` entries in the workspace
manifest. No Auracast policy, BASE parsing, codec selection, or lifecycle state needs to live in a
fork.
