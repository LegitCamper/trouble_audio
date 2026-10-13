# Handoff: LE Audio hardware conformance (session 3)

## Status

| Scenario | State |
| --- | --- |
| `unicast-self` | PASS |
| `unicast-sink-vs-zephyr-client` | PASS (through decoded PCM) |
| `unicast-source-vs-zephyr-server` | PASS (through received ISO audio) |
| `auracast-self` | FAIL - sink never discovers the broadcast |
| Auracast vs Zephyr `bap_broadcast_sink` | not started |
| Auracast vs Zephyr `bap_broadcast_source` | not started |

Harness: `~/zephyrproject/.venv/bin/python hwtest/run.py <scenario>`, scenarios in
`hwtest/scenarios/*.toml`, logs in `hwtest/logs/`. Never run host `cargo test` concurrently with
embedded builds (`critical-section` feature unification breaks).

Boards: A `820D9A5F0FC04A038AA34` (`/dev/ttyACM2`), B `820D9A5F0FDB93520E1DB` (`/dev/ttyACM0`).
Zephyr tree is at `v4.1.0-rc3-134-g3d0a7dab1a9` and is now **clean** - no sample or controller
edits remain. All Zephyr fixture workarounds live in `hwtest/zephyr/rtt.conf`.

Board bring-up is flaky: `probe-rs download` sometimes leaves board B hung right after `start`.
A stale `probe-rs` process holding a probe also shows up as
`capture did not become ready within 60 seconds`. Check `pgrep -af probe-rs` and re-run before
believing a failure.

## What this session fixed

### Unicast sink vs Zephyr client (was blocked at CIG creation)

1. `LE Set CIG Parameters` rejected `0x12`. Not a zero P->C interval (the original hypothesis was
   wrong). Controller computed a 13056 us transport latency against the requested 10000 us
   Max_Transport_Latency for two sequential CISes. No longer relevant - see 3.
2. `hci_le_cis_established: status 0x1e`. Root cause: `bap_unicast_client`'s prj.conf omits
   `CONFIG_BT_CTLR_ISO_TX_BUFFER_SIZE`, so it defaults to 9 and `LL_CIS_OCTETS_TX_MAX` becomes
   `9 - 8 = 1`. The CIG was built with Max_PDU 1 / BN 4, which the peripheral rejects.
   Fix: `CONFIG_BT_CTLR_ISO_TX_BUFFER_SIZE=255` in `hwtest/zephyr/rtt.conf`.
3. `hci_le_cis_established: status 0x20`. The sink's SDC lacked 2M PHY while Zephyr asks for
   `c_phy 2`. Fix: `.support_le_2m_phy()` in `examples/nRF54L15_Connect_Kit/src/bin/sink.rs`
   (alongside the pre-existing `.support_dle_peripheral()` and `.support_phy_update_peripheral()`).
   With 2M available the latency problem from 1 disappeared - no sample patch needed.
4. Second stream never connected; `LE Setup ISO Data Path` returned `0x0c`. Defaults are
   `CONFIG_BT_CTLR_ISOAL_SOURCES=1` / `..._SINKS=1`, one per CIS is required.
   Fix: both set to 2 in `hwtest/zephyr/rtt.conf`.
5. `Streams connected` was being shredded by RTT overflow. Removed
   `CONFIG_BT_ISO_LOG_LEVEL_DBG=y`. Keep BAP at `INF`; BAP DBG + ISO DBG together drop lines.

### Unicast source vs Zephyr server (new, now passing)

- `bap_unicast_server` advertises 39 bytes of ext adv data but the controller defaults to a
  31-byte buffer, so `bt_le_ext_adv_set_data()` failed `-33` and it never advertised.
  Fix: `CONFIG_BT_CTLR_ADV_DATA_LEN_MAX=191` in `hwtest/zephyr/rtt.conf`.
- The server advertises with extended advertising only. The source now uses
  `central.connect_ext()` (`examples/apps/src/basic_audio_source.rs`) and the binary enables
  `.support_ext_scan()`, `.support_ext_central()`, `.support_le_2m_phy()` (AUX_ADV_IND rides the
  2M secondary PHY).
- Target address is now build-configurable: `TROUBLE_AUDIO_TARGET` (`option_env!`, parsed by
  `target_address()` in `src/bin/source.rs`), defaulting to `basic_audio_sink::ADDRESS`. The
  harness passes it via a new per-board `env` table in the scenario TOML
  (`hwtest/flash.py::build_trouble_audio(binary, env_extra)`).
- Scenario uses `c0:fd:94:b5:ae:d6`, board B's Zephyr random identity (derived from FICR, stable
  per board; the sample logs it as `bt_hci_core: Identity: ...`). **If the Zephyr board changes,
  this address must change.**
- No new ASE Control Point client ops were needed. Phase 4's expected gap (Receiver Start Ready,
  Disable, Receiver Stop Ready, Update Metadata, Release) never surfaced: the trace is sink-only
  and the sample performs no teardown. Phase 5's attribute-capacity changes also never surfaced.
  Leave both undone unless a later foreign-peer trace demands them.

### Auracast (new binaries, source side working)

New: `examples/nRF54L15_Connect_Kit/src/bin/broadcast_source.rs` and `broadcast_sink.rs`, thin
wrappers over the existing `run_auracast_source` / `run_auracast_sink` in that crate's `lib.rs`.

- `nrf-sdc`'s `Builder` has **no setter** for BIG/BIS counts, which default to 0, so
  `LE Create BIG` failed `Connection Rejected due to Limited Resources`. Both binaries call a
  local `set_big_counts()` that pokes `SDC_CFG_TYPE_BIG_COUNT` / `BIS_SOURCE_COUNT` /
  `BIS_SINK_COUNT` through raw `sdc_cfg_set`. It **must run after `Builder::new()`**, because
  `sdc_init` resets the resource configuration. Marked `ponytail:` - replace when `nrf-sdc`
  exposes typed setters.
- `auracast_source`'s `BroadcastConfig.adv_handle` was 1 but only one adv set is configured;
  changed to 0 (`examples/apps/src/auracast_source.rs`).
- `../trouble/host/src/host.rs` never unmasked the BIG / periodic-sync LE events, so
  `LE Create BIG` and `LE BIG Create Sync` appeared to hang. Added, under `#[cfg(feature = "iso")]`:
  `enable_le_create_big_complete`, `enable_le_terminate_big_complete`,
  `enable_le_big_sync_established`, `enable_le_big_sync_lost`,
  `enable_le_periodic_adv_sync_established_v1`, `enable_le_periodic_adv_report_v1`.
  This is a second independent upstream-worthy Trouble fix.
- Result: board A reaches `[auracast_source] both BIS ready`.

## Current blocker: `auracast-self`, sink never discovers

`hwtest/logs/auracast-self-20261011-210801.log` (temporary diagnostic, since removed, printed
every ext adv report the sink saw). The sink receives a steady stream of reports but **every one
has `sid 255`** - i.e. no Advertising SID, so they are legacy/unrelated advertisers, never our
source's extended set. `parse_broadcast_audio_announcement` therefore never matches.

Candidate causes, cheapest first:

1. The sink's `ScanConfig::default()` has `phys: PhySet::M1`, so `LE Set Extended Scan Parameters`
   only scans the 1M primary PHY. That is correct for primary channel ADV_EXT_IND, but confirm the
   source's `LeSetExtAdvParams` primary PHY (`PhyKind::Le1M`, secondary `Le2M`) is actually being
   accepted, and that `.support_le_2m_phy()` on the sink lets it follow the aux pointer.
2. The sink may need `support_le_periodic_sync()` **and** extended scan buffer capacity for aux
   packets - currently `.scan_buffer_cfg(2)` / `.periodic_sync_buffer_cfg(2)`. Try raising.
3. Verify the source is really transmitting: the sink's own reports prove the radio works, so
   sniff from the other direction - temporarily point `broadcast_sink` at a permissive filter and
   log `report.addr`, looking for the source's `[0x51, 0x52, 0x53, 0x54, 0x55, 0xc0]`
   (`run_auracast_source` in `examples/nRF54L15_Connect_Kit/src/lib.rs`).
4. Check whether `LeSetExtAdvEnable`'s `AdvEventProps::new()` (non-connectable, non-scannable,
   non-legacy) is actually being encoded as extended; if it fell back to legacy the payload would
   be truncated and carry no SID.

## Remaining work

1. Fix `auracast-self` per above; markers already in `hwtest/scenarios/auracast-self.toml`
   (`both BIS ready`, `broadcast discovered`, `decoded BIS`).
2. Then add `auracast-source-vs-zephyr-sink.toml` (A = `broadcast_source`,
   B = `samples/bluetooth/bap_broadcast_sink`) and `auracast-sink-vs-zephyr-source.toml`
   (A = `broadcast_sink`, B = `samples/bluetooth/bap_broadcast_source`). Expect more missing
   `CONFIG_BT_CTLR_*` capacity knobs in `hwtest/zephyr/rtt.conf`, in the same spirit as the
   unicast ones; each needs a comment explaining the failure it fixes. Validate periodic
   advertising, BASE encode/parse (including fragmented BASE), BIG create/sync, BIS data path and
   LC3 payload reception. Defer BASS orchestration.
3. Commit and push the three dependency forks, then re-pin. See "Upstream merge (session 4)"
   below for what each one now contains and for the exact pins to restore.
   **Do not ship the `path = "../trouble/host"` dependency** or the local-path `[patch.crates-io]`
   entries. Do not discard `Cargo.lock`; it was already modified before this work.
4. Run `hwtest/run.py --all` once Auracast is green.

## Verification sequence (run sequentially)

`/tmp` runs out of quota easily (the agent task logs under `/tmp/claude-1000` are ~20G);
`cargo clean` an old `CARGO_TARGET_DIR` before blaming the code for a
`Disk quota exceeded (os error 122)`.

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/tmp/tr-t cargo +1.97 test \
  --manifest-path ../trouble/host/Cargo.toml --lib --features security \
  --target x86_64-unknown-linux-gnu                 # 84 passed
cd examples/nRF54L15_Connect_Kit && \
  cargo check --release --bin sink --bin source --bin broadcast_source --bin broadcast_sink
python -m py_compile hwtest/devices.py hwtest/flash.py hwtest/run.py
cd trouble-audio && cargo test --lib --no-default-features --features log   # 173 passed
git diff --check; git -C ../trouble diff --check
```

## Upstream merge (session 4)

All three forks were rebased onto their upstreams and the whole stack re-verified. All three are
on **`main`**, committed and **pushed** to `github.com/LegitCamper/*`. The root `Cargo.toml` pins
each by rev (see "Pins" below).

### What moved

- **`../bt-hci`** - fork `main` now sits on upstream `be70cad` (version **0.11.0**). Upstream grew
  its own BIG commands, so the fork's `big` commit is redundant; the only fork-specific patch left
  are `1731927`, dropping the `C::Return: FixedSizeValue` bound on `ExternalController`
  (still not upstream, still needed for the variable-length `LE Set CIG Parameters` return), and
  `a3b9b97`, re-adding `CisConfigTest::nse` (see the regression note below). Fork `main` =
  `a3b9b9785ef6e8269db31e1431f7d0710a6265cc`.
- **`../nrf-sdc`** - fork `main` sits on upstream `e44e619` plus `df44feb` (bt-hci 0.11), plus
  fork commit `6cca531`. **The ISO work is NOT upstream** (an earlier version of this document
  wrongly said it was): `6cca531` carries all of it and is load-bearing - the `Builder`
  `cig_count()`/`cis_count()`/`iso_buffer_cfg()` methods, `SDC_HCI_MSG_TYPE_ISO` dispatch in
  `try_hci_get`, the `LeAcceptCisRequest`/`LeRejectCisRequest`/`LeRemoveCig`/
  `LeRemoveIsoDataPath`/`LeCreateBig`/`LeTerminateBig`/`LeBigTerminateSync` `sdc_cmd!`s, and the
  hand-written `ControllerCmdSync`/`Async` impls for `LeSetCigParameters[Test]`, `LeCreateCis`,
  `LeBigCreateSync` and `LeSetupIsoDataPath` (their SDC C structs end in a flexible array member,
  so `sdc_cmd!`'s size-equality assertion cannot hold). Without it nothing ISO compiles.
  Fork `main` = `6cca5312fe1d17f922cc92db2948b28edaec0e11`.
- **`../trouble`** - fork `main` contains upstream `062293d` plus the three `feat(iso)`/`fix(iso)`
  commits, then `3f33e69` (bt-hci 0.11 migration, including the ISO/BIG event mask) and `1f154ca`
  (pairing address-kind normalization). Fork `main` =
  `1f154caf2c4e1fb46bfcec0677c8545457c9b85b`.

### Breaking changes absorbed for bt-hci 0.11

- `LeSetHostFeature`/`V2` take a typed `LeHostFeature` and a `bool`, not a raw bit index and `u8`.
  Call sites now use `LeHostFeature::CONN_ISO_STREAM` etc. (`../trouble` `host.rs`,
  `trouble-audio/src/cis.rs`).
- `Disconnect` and `LeFrameSpaceUpdate` are now **async** commands (they complete with Command
  Status, not Command Complete). `ControllerCmdSync<..>` bounds became `ControllerCmdAsync<..>`
  and the call sites use `async_command`/`command_async`. This is the correct HCI semantics.
- `LeCreateCis::new` takes the `CigId` explicitly, so `CigAction::CreateCis` carries a `cig_id`.
- `LeBigCreateSync::new` derives Num_BIS from the `bis` slice; the explicit count argument is gone.
- **Upstream regression fixed in the fork**: upstream's `CisConfigTest` lost its `nse` field, but
  NSE[i] is mandatory between CIS_ID[i] and Max_SDU_C_To_P[i] (Core 6 Vol 4 Part E §7.8.98), so
  omitting it shifts every later field by a byte and corrupts `LE Set CIG Parameters Test` - the
  exact command the `nrf-sdc` feature depends on. Re-added in `bt-hci/src/param/le.rs`; worth
  sending upstream.

### Toolchain

Upstream `trouble-host` now pulls `embassy-crypto`, which needs `cfg_select` - newer than the old
`nightly-2026-10-07` pin, so `rust-toolchain` is now the floating `nightly`. On recent
nightlies `generic_const_exprs` ICEs the next-generation trait solver (`rustc_infer`), so
`examples/nRF54L15_Connect_Kit/.cargo/config.toml` sets `rustflags = ["-Znext-solver=no"]`.
Re-pin to a dated nightly once both settle.

### Pins

The forks are pushed and the root `Cargo.toml` pins each by rev - no local `path` deps remain:

```toml
trouble-host     = { git = ".../trouble",  rev = "1f154caf2c4e1fb46bfcec0677c8545457c9b85b" }
bt-hci           = { git = ".../bt-hci",   rev = "a3b9b9785ef6e8269db31e1431f7d0710a6265cc" }
bt-hci-transport = { git = ".../bt-hci",   rev = "a3b9b9785ef6e8269db31e1431f7d0710a6265cc" }
nrf-sdc          = { git = ".../nrf-sdc",  rev = "6cca5312fe1d17f922cc92db2948b28edaec0e11" }
nrf-mpsl         = { git = ".../nrf-sdc",  rev = "6cca5312fe1d17f922cc92db2948b28edaec0e11" }
```

Each fork also has `backup/main-pre-upstream-merge` (and, on `bt-hci`,
`backup/*-pre-trailer-strip`) pushed, so the rebases and history rewrites are reversible.

Every example crate's `bt-hci` dependency moved `0.10` -> `0.11` to match.

### Still unverified

`examples/linux` and `examples/linux-serial` were not re-checked after the 0.11 bump (they need
`pipewire` and ran into the `/tmp` quota, not a code error). `hwtest` was not re-run: no board
scenario has executed since the merge, so `auracast-self` is still the open failure below.

## Auracast discovery: narrowing from session 3

`trouble-host`'s `ScanConfig::default()` is `phys: PhySet::M1` and `create_phy_params` maps that to
the 1M primary PHY only, which is correct for ADV_EXT_IND - so candidate 1 in the list above is
**not** the bug by itself. The source advertises with `AdvEventProps::new()` (no legacy bit) and
sets `PhyKind::Le1M` primary / `Le2M` secondary, and the sink does enable `.support_le_2m_phy()`.
The `sid 255` reports in the old log are therefore consistent with the sink seeing *only* legacy
advertisers - i.e. the source's extended set likely never started. Note the source's own
`TEMPDIAG` showed `ext_adv_enable`/`per_adv_enable`/`create_big` all issued and `both BIS ready`,
so start the next session by confirming at the controller level that the extended set is really
on air (candidate 3's permissive-filter sniff is the cheapest test), rather than re-auditing the
sink's scan parameters.

### Session 5 update

- Added `auracast-source-vs-zephyr-sink.toml` and `auracast-sink-vs-zephyr-source.toml`; both
  Zephyr fixtures build cleanly with `hwtest/zephyr/rtt.conf`.
- Zephyr's `bap_broadcast_sink` also times out at `sem_broadcaster_found` against our source.
  This independently confirms the source's Broadcast Audio Announcement is not observable on air;
  the failure is not specific to `trouble-host` scanning or announcement parsing.
- Moving the source's AUX advertising PHY from 2M to 1M made no difference and was reverted.
- Zephyr's `bap_broadcast_source` reaches `Broadcast source started`, but its controller rejects
  `LE Setup ISO Data Path` (`opcode 0x206e`, status `0x02`) and subsequent ISO sends return `-128`.
  Effective Kconfig already has two broadcast streams, `CONFIG_BT_CTLR_ISOAL_SOURCES=2`, and a
  255-byte ISO TX buffer, so this is a separate Zephyr-controller fixture blocker.
- Physical board `820D9A5F0FDB93520E1DB` consistently hangs inside `embassy_nrf::init` for either
  Rust broadcast binary, while the same board boots Zephyr. `probe-rs erase` and USB interface
  reset do not recover it; a real board power cycle is still required.
- `broadcast_sink.rs` had temporary internal-clock/debug settings inconsistent with every other
  Connect Kit binary and with its MPSL XTAL declaration. It now uses external HF/LF crystals;
  the sink boots on the healthy board, but the bad board still needs power cycling.
- After power cycling, both Rust binaries boot. A staged source run proved Zephyr discovers the
  Broadcast Audio Announcement and establishes PA sync while the source waits before `LE Create
  BIG`; once BIG creation runs, later scanners no longer see the extended advertiser. Even before
  BIG creation, no periodic advertising reports/BASE reach Zephyr, although all HCI commands
  return success. This narrows the remaining source blocker to Nordic SDC periodic advertising /
  BIG coexistence rather than announcement encoding or scan filtering.
- The raw BIG/BIS `sdc_cfg_set` calls now run after every `sdc_support_*` call, as required by the
  Nordic SDC API, and before the typed resource config calls. This ordering fix is retained in both
  broadcast binaries, though it did not by itself make periodic reports appear on air.

## Security

`hwtest/logs/unicast-sink-vs-zephyr-client-20261011-172525.log` contains a raw ephemeral LTK from
an earlier session. Logs are gitignored but still sensitive on disk - do not publish it. Secret
logging has been removed from `../trouble` and from `hwtest/zephyr/rtt.conf`. The recent
`TEMPDIAG` lines in `hwtest/logs/auracast-self-*.log` are harmless (payload lengths and SIDs
only); the source-side `TEMPDIAG` prints have been removed from `trouble-audio/src/big.rs` and
`big_sink.rs`.

## Dirty files

`trouble_audio`:
`Cargo.lock`, `Cargo.toml`, `examples/apps/src/auracast_source.rs`,
`examples/apps/src/basic_audio_source.rs`, `examples/apps/src/sink.rs`,
`examples/nRF54L15_Connect_Kit/.cargo/config.toml`, `examples/nRF54L15_Connect_Kit/src/bin/sink.rs`,
`examples/nRF54L15_Connect_Kit/src/bin/source.rs`, `trouble-audio/src/big.rs`,
`trouble-audio/src/big_sink.rs`, `trouble-audio/src/iso_rx.rs`, plus untracked
`examples/nRF54L15_Connect_Kit/src/bin/broadcast_{source,sink}.rs` and `hwtest/`.

Plus, from session 4: `rust-toolchain`, `trouble-audio/src/{cig,cis,big_sink}.rs`,
`trouble-audio/Cargo.toml` and every `examples/*/Cargo.toml` (bt-hci 0.11).

`../trouble`, `../bt-hci`, `../nrf-sdc`: all **clean** - committed and pushed, see
"Upstream merge (session 4)" above.

`~/zephyrproject/zephyr`: clean apart from pre-existing unrelated untracked files
(`boards/makerdiary/nrf54l15_connectkit/`, `samples/bluetooth/hci_usb/overlay-*.conf`).

### Session 6 update

- `../nrf-sdc` origin is the user's `LegitCamper/nrf-sdc` fork. Added typed
  `Builder::{big_count,bis_source_count,bis_sink_count}` setters, committed as
  `1b62e96edbd59e937f374ea9eb4d9a3b1f80b95a`, pushed to `origin/main`, re-pinned the workspace,
  and removed both binaries' duplicated raw `set_big_counts()` workaround.
- `LeSetPeriodicAdvData` serialization is correct: the 46-byte BASE command reaches Nordic as
  handle 0, operation 3 (Complete), length 46, first AD byte 45. Splitting First/Last fragments,
  moving the command after PA enable, delaying BIG 90 seconds, and changing periodic event length
  did not help. Those experiments were reverted.
- Source periodic advertising was valid. Zephyr's sink controller silently discarded reports over
  its default `CONFIG_BT_CTLR_SCAN_DATA_LEN_MAX=31`; a 31-byte diagnostic produced hundreds of PA
  reports while 32 bytes produced none. `hwtest/zephyr/broadcast-sink.conf` now sets 191, and the
  test reaches `BASE received`.
- Both Zephyr broadcast samples default to two ISO streams total but only one stream per BIG.
  `CONFIG_BT_CTLR_SYNC_ISO_STREAM_MAX=2` fixes the sink's second BIS data-path rejection;
  `CONFIG_BT_CTLR_ADV_ISO_STREAM_MAX=2` in `broadcast-source.conf` fixes the source's opcode
  `0x206e status 0x02`. After the latter fix Zephyr reports sent ISO packets successfully.
- Remaining source-to-Zephyr validation needs a board power cycle: repeated reflashes made PA sync
  establishment flaky. Remaining reciprocal blocker is Rust `broadcast_sink`: it initializes and
  scans but never discovers Zephyr's broadcast announcement, while Zephyr source is streaming.
- Follow-up hardware validation found Nordic SDC needs a startup guard between enabling periodic
  advertising and `LE Create BIG`: 500 ms still timed out on later PA sync, while 2 seconds passed
  the complete source-to-Zephyr flow twice (announcement, PA, BASE, both BIS, incoming audio). The
  retained guard is marked `ponytail:` until the controller exposes a first-PA-event signal.
- Rust sink diagnostics received thousands of legacy extended-report events (SID 0xFF) but zero
  reports for Zephyr's extended advertiser. Delaying Zephyr source startup by 12 seconds did not
  change that. A passive-scan experiment was started but board B disconnected before validation,
  so it was reverted. Board B repeatedly falls off USB after probe-rs shutdown errors.

### Session 7 update

- Reciprocal periodic sync root cause fixed. Nordic SDC uses the active extended-scan procedure to
  acquire periodic advertiser SyncInfo; calling `ScanSession::stop()` immediately after discovery
  or immediately after `LE Periodic Advertising Create Sync` aborts the pending synchronization.
  `BigSink` now exposes `wait_periodic_sync()`, and the Auracast sink keeps scanning until that
  signal reports establishment before stopping the scan.
- Increasing `.scan_buffer_cfg(2)` to `.scan_buffer_cfg(8)` remains required in the nRF54L15 sink;
  dense legacy advertising otherwise starves the AUX report carrying the broadcast announcement.
- The Connect Kit broadcast sink heap increased from 64 KiB to 96 KiB. Two LC3 decoder allocations
  exhausted 64 KiB during `auracast-self` (`memory allocation of 19884 bytes failed`).
- Final hardware passes:
  - `auracast-sink-vs-zephyr-source-20261013-142106.log`
  - `auracast-source-vs-zephyr-sink-20261013-141902.log`
  - `auracast-self-20261013-142030.log`
- Final embedded broadcast binaries compile, library tests pass (173 tests), `git diff --check`
  passes, `../nrf-sdc` is clean, and Zephyr has only the previously documented untracked files.

### Session 8: preset matrix and quantitative SDU metrics

New library code:
- `trouble-audio/src/stream_stats.rs`: `SduStats` classifies every SDU sequence number into
  received / controller-flagged-lost / inferred-missing / duplicate / reordered, tracks the worst
  missing burst, and reports `loss_per_mille()`. 16-bit sequence wrap is handled explicitly.
- `trouble-audio/src/presets.rs`: named BAP/GMAP presets (`48_2_1`, `48_1_1`, `48_1_gs`,
  `32_1_gs`) with SDU interval, LC3 frame size, RTN, transport latency and presentation delay.
- `BigSink` now owns per-BIS `SduStats`, exposed through `BigSink::bis_stats(bis_index)`.

Examples:
- `auracast_source::run` takes an `AudioPreset` and an optional Broadcast_Code; the tone generator
  derives frame size, octets per frame and ticker period from the preset (7.5 ms presets use a
  microsecond ticker).
- `auracast_sink` logs `stats bis=... loss_permille=...` every 10 seconds.
- Preset and Broadcast_Code are selected at build time via `TROUBLE_AUDIO_PRESET` and
  `TROUBLE_AUDIO_BROADCAST_CODE`; `build.rs` declares `rerun-if-env-changed` for both, which was
  verified by confirming the gaming firmware contains `48_1_gs` while the default contains
  `48_2_1`.
- Connect Kit broadcast sink heap is 96 KiB (two LC3 decoders exhausted 64 KiB).

Harness:
- `hwtest/run.py` scenarios may now declare `max_metrics` / `min_metrics`. Any `key=value` pair in
  a log line is parsed, the worst value per metric is retained, and a metric that is asserted on
  but never observed fails the scenario (silence must not pass).

New scenarios:
- `auracast-sink-quality-vs-zephyr-source`: PASS. Worst metrics `loss_permille=1`, `burst=0`,
  `received=887` (`hwtest/logs/auracast-sink-quality-vs-zephyr-source-20261013-164532.log`).
- `auracast-gaming-source-vs-zephyr-sink`: PASS with GMAP 48_1_gs, 7.5 ms SDUs, RTN 1
  (`hwtest/logs/auracast-gaming-source-vs-zephyr-sink-20261013-164708.log`).
- `auracast-self-quality`, `auracast-gaming-self`, `auracast-encrypted-self`: written but
  **not yet validated** - board B (`820D9A5F0FDB93520E1DB`) again hangs inside `embassy_nrf::init`
  for Rust firmware and needs a physical power cycle. `probe-rs reset` does not recover it. The
  same sink firmware passes on board A, so this is the known board fault, not a regression.

Regression checks after the change: `auracast-source-vs-zephyr-sink` PASS,
`auracast-sink-vs-zephyr-source` PASS, library tests 184 passed, both embedded binaries compile
for the default and `32_1_gs` presets.

Remaining from the earlier gap analysis (not started): bidirectional CIS (`cig.rs` still pins
`rtn_p_to_c: 0`), GMAP service-level hardware tests, BASS/Broadcast Assistant, latency measurement
rig, long soak, and full config-matrix sweep.

### Session 9: unicast preset plumbing and 7.5 ms controller limit

- Unicast source now takes the shared `AudioPreset`; codec configuration, QoS, CIG timing, LC3
  frame size, tone generation, and transmit ticker derive from the selected preset.
- Added `unicast-gaming-source-vs-zephyr-server` for GMAP `48_1_gs` at 7.5 ms.
- Fixed two nrf-sdc adapter bugs on the user fork: `LE Create CIS` now strips bt-hci's synthetic
  CIG handle before calling the SDC C ABI, and failed CIS-established events no longer panic while
  decoding zeroed invalid PHY fields.
- `unicast-source-vs-zephyr-server` passes end-to-end after those fixes
  (`hwtest/logs/unicast-source-vs-zephyr-server-20261013-173209.log`).
- `unicast-gaming-source-vs-zephyr-server` reaches CIG creation without parser panics, but both CIS
  fail with status `0x0d` (limited resources) at 7.5 ms. Trying 2M PHY did not change the result and
  was reverted. Next work is an SDC-compatible low-level CIG Test schedule or mono gaming topology.
- Library tests pass (185 tests), and default embedded source/sink release checks pass.
