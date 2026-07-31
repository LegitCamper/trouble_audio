# nRF54L15 LE Audio sink - testing log

Goal: nRF54L15 Connect Kit as an LE Audio unicast sink (peripheral), receiving real
stereo audio from a Samsung Galaxy S23+ over CIS/ISO, with no speaker on the board -
proof of life is `sink.rs`'s `drive_led` task turning decoded PCM peak amplitude into
PWM brightness on the Green LED (P0.2 - the only LED the nRF54L15 itself can drive; the
RGB LED next to it belongs to the separate nRF52820 interface MCU).

Binary under test: `cargo run --release --bin sink` (must be `--release` - debug builds
overflow the stack in GATT server construction and corrupt the heap, see below).

Known flake: `probe-rs run` sometimes hangs between "start" and "clocks initialized" on
first flash after erasing - a flash/reset handoff issue, not a real crystal problem.
Press the board's physical reset button once if this happens; no reflash needed.

## Fixed so far, in the order hit

1. **`bt-hci-driver` unresolved dependency.** `nrf-sdc`'s own `[patch.crates-io]` doesn't
   propagate to downstream builds (Cargo only applies `[patch]` from the workspace root).
   Added `bt-hci-driver` to this example's own patch section.
2. **Missing toolchain bits.** Installed the `thumbv8m.main-none-eabihf` rustup target and
   the `clang` pacman package (bindgen needs it for `nrf-sdc-sys`/`nrf-mpsl-sys`).
3. **`trouble-host` too old for `bt-hci`'s `alloc_buf()`/`Controller::Buffer<'_>` API.**
   No existing `trouble-host` branch (upstream or LegitCamper's fork) had adopted it.
   Cherry-picked upstream PR #634's `host.rs` hunk onto current `embassy-rs/trouble` main,
   pushed as `LegitCamper/trouble@nrf54-bt-hci-alloc` (`679b3fd7`). All three places that
   reference `trouble-host` (`trouble-audio/Cargo.toml`, `examples/apps/Cargo.toml`,
   this example's `Cargo.toml`) now point at that one commit - they used to point at two
   different, older revs, which would have caused duplicate-crate type errors on its own.
4. **`trouble-audio`'s own CIS/CIG code was stale against a `bt-hci` API change already
   made in `bt-hci` itself** (`data_path_direction`/`DATA_PATH_ID_HCI` consts replaced by
   an `IsoDataPathDirection` enum). Fixed call sites in `cig.rs`/`cis.rs`.
5. **Debug-build stack overflow corrupting the heap.** `PacsServer::new` cloning a
   `Vec<u8>` (`pacs.rs`) hit an "unsafe precondition violated" panic reading garbage
   ptr/len. Reproduced clean natively (`cargo test`, unlimited heap/stack) - ruled out a
   real logic bug. Root cause: debug/unoptimized builds have much larger stack frames for
   this deep, fully-synchronous GATT-server-construction call chain, and the stack and the
   heap's static backing array share the same RAM region. Fixed by using `--release`.
6. **`ServerBuilder::add_pacs`'s `Preferred_Presentation_Delay_Min/Max` was hardcoded to a
   degenerate `[0,0]` range** (`bap.rs`). Widened to span the same `[0, 40ms]` range as
   `Presentation_Delay_Max` - turned out not to be the actual blocker for the "Android
   Enables then immediately Releases" symptom (Android still picked 0 either way), but is
   still a real fix worth keeping.
7. **The actual "Enable then immediately Release, never streams" root cause: `nrf-sdc`'s
   `Builder` never reserved any CIG/CIS/ISO controller memory.** Confirmed via `adb`
   HCI-level Android logcat: Android *did* send `LE Create CIS`; our controller rejected
   it with `MEMORY_CAPACITY_EXCEEDED`. `support_cis_central()`/`support_cis_peripheral()`
   only flip a capability bit - `cig_count`/`cis_count`/every ISO buffer count/size all
   default to `0` in the SDC C library, and the Rust `Builder` never exposed a way to set
   them. Added `cig_count()`, `cis_count()`, `iso_buffer_cfg()` to `nrf-sdc`'s `Builder`
   (pushed to `LegitCamper/nrf-sdc@iso-cis-cig-support`, commit `73669c9`), wired into both
   `sink.rs` and `source.rs`'s `build_sdc()`. Needed a `sdc_mem` bump too (8192 -> now
   16384; SDC reports the exact number needed in a boot-time error if it's ever short).
8. **`unreachable!()` panic in `try_hci_get` the moment real ISO traffic arrived.**
   `sdc_hci_get()` returns `SDC_HCI_MSG_TYPE_ISO` (value 8) for incoming ISO data once a
   CIS is actually streaming; the match in `nrf-sdc`'s `try_hci_get` only knew about
   `_DATA`/`_EVT`. Added the missing arm (`nrf-sdc@iso-cis-cig-support`, commit `ff1b3fc`).
9. **`Lc3MonoDecoder::new` needs ~19.9KB (scaler) + 7.5KB (complex) per channel = ~55KB for
   stereo** (`Lc3MonoEncoder` needs ~15.9KB/channel = ~32KB for stereo on the source side).
   The original 16KB heap wasn't even close. Bumped `HEAP_SIZE` to 128KB (`sink.rs`) /
   64KB (`source.rs`) - exact numbers came from `lc3_codec::{Lc3Decoder,Lc3Encoder}::calc_working_buffer_lengths`,
   computed via a throwaway native test, not guessed.

## Current blocker (as of the last flash)

CIS establishes cleanly now (both ASEs reach `Streaming`, confirmed over two separate
phone connections), but **every single ISO SDU comes back marked "Lost"** by the
controller: `on_iso_data`'s debug log shows `data_len=0` and
`IsoDataLoadHeader { iso_sdu_len: 32768, .. }` on every packet. `32768 = 0x8000` - per
the Core spec, `ISO_SDU_Length`'s field actually packs `Packet_Status_Flag` into bits
14-15 alongside the real length in bits 0-13 (this fork's `IsoDataLoadHeader` parser in
`bt-hci` doesn't mask them apart, worth fixing separately but not the root cause here);
`0x8000`'s top two bits are `10` = status **2 = "Lost data"**, length bits all zero.
100% loss across an extended, stable connection looks much more like ISO PDU/SDU
reassembly buffer starvation on our peripheral controller than real RF loss.

**Last action, not yet flash-tested:** bumped `iso_buffer_cfg`'s PDU/SDU buffer counts
in `sink.rs` (`rx_pdu_buffer_per_stream_count` 2->6, `rx_sdu_buffer_count` 4->8, TX side
similarly) to test the buffer-starvation theory, and bumped `sdc_mem` from 10240 to
16384 bytes (SDC reported 13600 needed for the new counts; last build compiles clean but
has not been run on hardware yet).

## Known secondary issue, not yet fixed

`Lc3MonoDecoder`/`Lc3MonoEncoder`'s working buffers are `Box::leak`'d (need `'static`
lifetime, `lc3.rs`'s module doc explains why) - fine for one long-lived session, but
`on_cis_established` allocates fresh ones on every reconnect rather than reusing old
ones, so each reconnect permanently burns another ~55KB (sink) / ~32KB (source) of heap.
Already observed this OOM the heap after 3 reconnects with the old 128KB budget. Bumping
`HEAP_SIZE` further buys more reconnects but doesn't fix the leak - worth revisiting once
the Lost-SDU issue is sorted, probably by having `CisManager` reuse a decoder/encoder
already allocated for a given `ase_id`/channel instead of allocating fresh each time.

## Next steps

1. Flash the current build (`cargo run --release --bin sink`), reconnect from the phone,
   check whether SDUs are still marked Lost.
2. If still Lost: consider whether NSE/BN (burst number/sub-events, chosen by the
   central, visible in the QoS negotiation but not something we control) needs matching
   controller-side tuning we're missing, or whether this needs a lower-level look at
   nrf-sdc's ISO reassembly path itself rather than just buffer counts.
3. If it starts working: watch for the LED actually reacting to loud vs quiet audio
   (peak-driven PWM duty on P0.2), and then circle back to the decoder-leak issue above
   before doing any extended/repeated-reconnect testing.
4. Fix `IsoDataLoadHeader`'s `iso_sdu_len` field to mask off `Packet_Status_Flag` and
   expose it separately (in the `bt-hci` fork) - not the root cause of the current issue,
   but is a real correctness bug that will silently misreport length on real data too if
   left as-is (any future packet with a valid length >= 0x4000 would already display
   nonsense, and downstream code has no way to distinguish "lost" from "long" without it).

## External fork changes (already committed + pushed)

- `LegitCamper/nrf-sdc@iso-cis-cig-support`:
  - `73669c9` - expose `cig_count`/`cis_count`/`iso_buffer_cfg` in the `Builder`.
  - `ff1b3fc` - handle `SDC_HCI_MSG_TYPE_ISO` in `try_hci_get`.
- `LegitCamper/trouble@nrf54-bt-hci-alloc` (`679b3fd7`) - cherry-picked upstream PR #634's
  `alloc_buf()`/`Controller::Buffer<'_>` adoption onto current `embassy-rs/trouble` main.
