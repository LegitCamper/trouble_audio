# nRF54L15 LE Audio sink - testing log

Goal: nRF54L15 Connect Kit as an LE Audio unicast sink (peripheral), receiving real
stereo audio from a Samsung Galaxy S23+ over CIS/ISO, with no speaker on the board -
proof of life is `sink.rs`'s `drive_led` task turning decoded PCM peak amplitude into an
on/off signal on the Green LED (P0.2 - the only LED the nRF54L15 itself can drive; the
RGB LED next to it belongs to the separate nRF52820 interface MCU; see "gave up on
`SimplePwm`" below for why this is on/off rather than PWM brightness).

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

## Fixed: decoder/encoder leak on reconnect (was OOM-crashing on track skip)

`Lc3MonoDecoder`/`Lc3MonoEncoder`'s working buffers are `Box::leak`'d (need `'static`
lifetime, `lc3.rs`'s module doc explains why) - fine for one long-lived session, but
`on_cis_established` used to allocate fresh ones on *every* reconnect rather than reusing
old ones, so each reconnect permanently burned another ~55KB (sink, stereo) / ~32KB
(source) of heap. This is exactly what was hitting the `handle_alloc_error` panic (`memory
allocation of 19884 bytes failed`, in `Lc3MonoDecoder::new`, backtrace through
`on_cis_established`) seen after skipping a track a few times: skip -> Android tears down
and re-establishes the CIS -> another ~27564 bytes leaked per channel -> heap exhausted
within a handful of reconnects even at the 128KB budget.

Fixed in `cis.rs`: `Codec` now tags each variant with the `AudioParams` it was built for,
and `on_cis_established` reuses the existing codec already sitting in that ASE's slot when
a reconnect renegotiates the exact same Sampling_Frequency/Frame_Duration (the
overwhelmingly common case - the params only actually change if the phone renegotiates a
different codec config). Covered by a regression test
(`cis::tests::reconnecting_with_the_same_audio_params_reuses_the_existing_decoder`) that
installs a byte-counting global allocator and asserts zero bytes are allocated on the
second `on_cis_established` call. Reconnecting with genuinely different audio params still
allocates a new codec (and leaks the old one) - fine for now since that's rare, revisit
only if it turns out to matter in practice.

## Investigated: LED never blinks / decoding looks "stuck on" for ~10s after pause

A capture from a later session (still on the buffer-starvation workaround from the section
above) showed `on_iso_data`'s debug log outputting `data_len=0` on essentially *every* ISO
event for the sampled window, each one still fed straight into `decoder.decode()` and
enqueued onto `pcm_out` - `lc3-codec`'s `decode_frame` never actually errors out on a
short/empty `buf_in`, it silently falls back to packet-loss-concealment (near-silent
output) and returns `Ok(())`, so these show up in the log as ordinary `decoded ase_id=...
peak=0` frames rather than anything visibly wrong. Two observable symptoms trace back to
this:

- **No LED blink**: a `pcm_out` capacity of 4 left almost no headroom - concealment frames
  from every empty/lost event filled the queue as fast as `drive_led` could drain it, so
  real (non-silent) frames were getting dropped by `[cis] on_iso_data: pcm_out channel
  full, dropping frame` about as often as they arrived. Bumped `pcm_out` to 16 slots (~80ms
  of headroom at stereo/10ms rates) as a mitigation - this reduces the drop rate but
  doesn't address why so many events are empty in the first place (see below).
- **~10s "still decoding" after pause**: consistent with the CIS staying established and
  the controller continuing to deliver periodic (empty) ISO events for a while after the
  peer actually stops sending real audio, each of which we still decode-and-enqueue as if
  it were valid.

## Fixed: LED never lit - gave up on `SimplePwm`, drive the LED as plain GPIO instead

Diagnosed with a boot-time self-test added to `sink.rs`: 3 blinks via a plain
`embassy_nrf::gpio::Output` on P0.2 (immediately after `embassy_nrf::init`, before BLE/PWM
touch anything), followed by 3 more blinks through the real `SimplePwm` path. The plain-GPIO
stage always blinked fine (confirmed twice) - P0.2/the LED/the wiring are all good. The PWM
stage never lit the LED, with *either* polarity:

1. First suspected `DutyCycle::normal` specifically: its documented polarity is "output high
   while the counter is at or above the duty value", which maps `peak=0` (silence) to
   *permanently on* and full-scale audio to *permanently off* - backwards from the intended
   "louder = brighter", and `embassy-nrf`'s own first-party `nrf54l15-app` PWM example
   (`examples/nrf54l15-app/src/bin/pwm.rs`) uses `DutyCycle::inverted` for this exact chip/PWM
   instance instead. Switched both the self-test and `drive_led` to `::inverted`.
2. Re-tested: still nothing, not even the boot-time PWM self-test blinks. So it isn't a
   polarity nuance - `SimplePwm` on `PWM20`/P0.2 doesn't drive the pin at all on this
   hardware/embassy-nrf revision, for reasons not yet root-caused (chip is very recently
   supported in embassy-nrf; could be a driver bug specific to this PWM instance/pin
   combination, a missing clock/domain prerequisite PWM needs that GPIO doesn't, or something
   else - didn't dig further once the workaround below proved reliable).

Given a plain `Output` on the identical pin reliably works, gave up on `SimplePwm` for this
proof-of-life LED rather than continuing to chase an unexplained hardware/driver issue.
`drive_led` now drives P0.2 directly as a thresholded on/off GPIO (on above
`LED_ON_THRESHOLD = 512` peak out of 32767, off below) instead of PWM brightness - loses smooth
dimming, but that was never the point; the point is proving decoded audio is flowing, and a
binary on/off signal does that fine. `PWM20`/`SimplePwm` are no longer used anywhere in this
example. Revisit only if brightness-proportional feedback actually becomes worth the yak-shave.

## Added: persist the BLE bond across reflashes/restarts

Previously both `sink.rs` and `source.rs` passed `None` for `bond_store`, so every reflash or
power cycle meant re-pairing from the phone (the security manager starts empty each boot, but
the phone still remembers the old LTK and won't offer to pair again on its own).

Added `bond_store::rram_bond_store`, backed by a page of on-chip RRAM (`memory.x` now reserves
the last 4KB of flash as `BOND_STORAGE`, `0x0017C000`-`0x0017CFFF` - shrunk `FLASH` from 1524K to
1520K to make room, no change in total usable memory, just a repartition). Wired into both
`sink.rs` and `source.rs`'s `main`.

Only one bond is ever kept (matches this crate's single-active-connection model already
documented on `BondStore`) - a new phone pairing just overwrites the slot, no separate "forget"
step needed for the "someone else's phone tries to pair" case.

The actual encode/decode/error-handling logic (`postcard`-encode a `BondInformation`, tolerate
missing/corrupt data without crashing) now lives once in
`trouble_audio_example_apps::bond_store::EncodedBondStore`, shared across every platform example
rather than duplicated per platform - each platform only supplies a pair of raw-bytes-in/
raw-bytes-out closures (`examples/linux/src/bond_store.rs` for a file, this crate's
`bond_store.rs` for RRAM). This replaced the Linux example's previous JSON-via-`serde_json`
`FileBondStore` (now `sink_bond_store()`/`source_bond_store()` functions) - the on-disk format
changed (postcard binary, not pretty JSON) and the default filename dropped its `.json`
extension, so an existing bond file from before this change won't be found; one-time re-pair
needed there too, same as everywhere else after this lands.

The underlying "why is `data_len` 0 on nearly every packet" question is still open and
looks like the same root cause as the "Current blocker" section above - the
`iso_buffer_cfg` bump documented there evidently did **not** resolve it on real hardware
(this capture is from *after* that change shipped). Two concrete next steps, both outside
this repo (they live in the `nrf-sdc`/`bt-hci` forks under `[patch.crates-io]`):

1. Fix `IsoDataLoadHeader`'s `iso_sdu_len` masking (`bt-hci` fork, flagged below) so
   `on_iso_data` can actually tell "genuinely lost" apart from "valid but short/empty" -
   right now there's no way to distinguish them from this crate's side, which blocks
   diagnosing this further.
2. Once that's in, check whether the still-empty events are marked Lost (points back at
   SDC ISO buffer/timing config) or arriving with a "valid" status despite 0 bytes (points
   at a parsing bug in `IsoPacket`/`IsoDataLoadHeader` itself, or at NSE/BN-driven
   duplicate delivery - the ~3.4ms spacing observed between consecutive `on_iso_data` calls
   in the capture is faster than the negotiated 10ms SDU interval, which is itself worth
   explaining).

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
