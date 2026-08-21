# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LE Audio (unicast and Auracast source/sink) on top of `trouble-host`: the full set of LE Audio
GATT services, ASE Control Point, CIG/CIS/ISO setup, and LC3 encode/decode. `no_std` + `alloc` —
the consuming binary must install a global allocator. Pinned nightly toolchain (`rust-toolchain`);
uses `generic_const_exprs`.

## Commands

```sh
# Tests (host). A log backend feature is REQUIRED — plain `cargo test` fails to link
# because of defmt. Run from trouble-audio/:
cargo test --lib --features log

# Single test:
cargo test --lib --features log <test_name>

# Miri (leaks ignored because lc3.rs deliberately leaks its working buffers):
MIRIFLAGS=-Zmiri-ignore-leaks cargo +nightly miri test --lib --no-default-features --features log
```

Embedded examples build from their own directory — each has a `.cargo/config.toml` setting the
target and runner (`rp-pico-w`: thumbv6m + `elf2uf2-rs` BOOTSEL drag-and-drop, logs over
USB-serial; `nRF54L15_Connect_Kit`: thumbv8m + `probe-rs`, defmt/RTT). `cargo build` at the
workspace root will try to build them for the host and fail; check the core crate with
`cargo check --features log` from `trouble-audio/` instead.

## Comment style

- Terse by default. A comment exists to state a constraint the code can't show: wire formats (cite
  the spec section, e.g. "Core 6 Vol 4 Part E §5.4.5"), controller quirks/workarounds, safety or
  leak invariants, why the non-obvious choice was made.
- If a comment needs a paragraph to explain a function, the code probably needs restructuring —
  fix the code, don't write the essay. The one sanctioned home for longer prose is a module-level
  `//!` doc covering genuine protocol-level complexity (see `cig.rs`, `iso_tx.rs` for the ceiling).
- Never narrate what the next line does, where code was copied from, or why a change is correct.
- `examples/` gets more leeway — those comments teach.

## Other conventions

- Dual logging: `defmt` and `log` are mutually exclusive cargo features; gate with
  `#[cfg_attr(feature = "defmt", derive(defmt::Format))]` etc. Never `derive(defmt::Format)` on a
  struct containing a `bitflags!` field — it breaks the build.
- `alloc` (`Vec`, etc.) is for the variable-length LE Audio structures (PAC records,
  codec-specific config, metadata); use `heapless` for fixed-capacity data.
- The `nrf-sdc` feature switches CIG setup to Nordic's documented `LE Set CIG Parameters Test`
  workaround. It must stay opt-in — it's wrong for every other controller.
- Root `Cargo.toml` pins `trouble-host` to the user's own fork and `[patch]`es it (plus
  `nrf-sdc`/`nrf-mpsl`) to local checkouts at `../trouble` and `../nrf-sdc`; embassy is pinned to
  a specific rev. Each pin has a manifest comment explaining why — don't bump or "clean up" these
  without reading it. The forks are the user's own and free to edit directly.

## Architecture

Three layers:

1. **`trouble-audio/`** — the core crate. One module per GATT service (`pacs`, `ascs`, `vcs`,
   `mics`, `csis`, `mcs`, `tbs`, `ots`, `has`, `bass`, `gmas`, `tmas`, `aics`, `vocs`), each
   exporting a `*Server` and an `*_ATTRIBUTES` count. `server.rs`'s `ServerBuilder`/`Server`
   aggregates them into one attribute table and dispatches `GattEvent`s; `client.rs` is the
   central-side mirror. `prelude` re-exports the common types (name-colliding optional-service
   types are deliberately excluded).
2. **Streaming path** — `ase_client.rs` drives the ASE Control Point from the central;
   `cig.rs` (`CigManager`, central/source) and `cis.rs` (`CisManager`, peripheral/sink) are
   mirrors of each other for CIG/CIS/ISO-data-path setup. Both follow the same split: the manager
   implements `EventHandler` for `RxRunner::run_with_handler`, but anything needing an awaited HCI
   command is deferred to a `drive_*` function that must be polled concurrently. `iso_tx.rs`
   hand-encodes outgoing HCI ISO packets (bt-hci has no outbound constructor); `lc3.rs` wraps the
   codec.
3. **`examples/apps`** — platform-agnostic sink/source/bond-store logic shared by every platform
   example; the per-platform crates (`linux`, `linux-serial`, `rp-pico-w`,
   `nRF54L15_Connect_Kit`) only supply the HCI transport and real audio I/O.
