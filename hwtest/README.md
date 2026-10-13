# `hwtest` - hardware-in-the-loop conformance tests

Scripted pass/fail tests that run real LE Audio over real radios on two nRF54L15 Connect Kits.

The point is **conformance, not self-consistency**. The 173 host unit tests in `trouble-audio/`
drive the stack with synthetic HCI events, and the two-board tests in the example README had
our own code on both ends - a source and a sink that share a misreading of the spec agree
perfectly. These scenarios put Zephyr's known-conformant LE Audio stack on the other side of the
radio instead.

## Requirements

Nothing to install. The harness runs on the Python in Zephyr's own venv, which already has
`pyserial`, `west` and `ninja`:

```sh
~/zephyrproject/.venv/bin/python hwtest/run.py --list
```

- Two nRF54L15 Connect Kits over USB (`probe-rs list` should show two CMSIS-DAP probes).
- `probe-rs` on `PATH`. Both boards are addressed by **full probe serial** - they share
  VID:PID `2fe3:0204`, so a bare VID:PID selector is ambiguous and probe-rs refuses it.
- For Zephyr scenarios: a Zephyr tree and SDK. Defaults are `~/zephyrproject` and
  `~/zephyr-sdk-0.17.0`; override with `ZEPHYR_BASE`, `ZEPHYR_VENV`, `ZEPHYR_SDK_INSTALL_DIR`.

Zephyr images are built for `nrf54l15dk/nrf54l15/cpuapp` and flashed with `probe-rs`. Makerdiary's custom board target currently builds Bluetooth images without a working controller. Official DK target uses the same SoC/radio memory map; RTT capture avoids board-specific UART and LED pins.

## Running

```sh
PY=~/zephyrproject/.venv/bin/python

$PY hwtest/run.py --list              # what exists
$PY hwtest/run.py unicast-self        # one scenario
$PY hwtest/run.py --all               # everything; nonzero exit on any failure
$PY hwtest/devices.py                 # just show the discovered boards
```

Transcripts are always written to `hwtest/logs/<scenario>-<timestamp>.log`, pass or fail. A
conformance failure is only useful if the trace survives.

## Roles

Boards are addressed as roles `A` and `B`, assigned by sorted probe serial so repeat runs stay
comparable. Pin them explicitly if you want a specific physical board in a specific role:

```sh
HWTEST_SERIAL_A=820D9A5F0FC04A038AA34 HWTEST_SERIAL_B=820D9A5F0FDB93520E1DB $PY hwtest/run.py --all
```

Each Connect Kit exposes two CDC-ACM ports from its onboard nRF52820 interface MCU. Per the board
DTS, `cdc_acm_uart0` is bridged to the nRF54L15's `uart30` (P0.0/P0.1) and `cdc_acm_uart1` is the
interface MCU's own shell, so the console is always the lower USB interface number. `devices.py`
resolves this per board; don't hardcode `/dev/ttyACM*`, it moves.

## Writing a scenario

One TOML file per scenario in `scenarios/`. Each board gets firmware plus the markers its log
stream must and must not contain:

```toml
description = "what this proves"
timeout = 120              # seconds of capture before giving up

[boards.A]
firmware = "trouble-audio" # cargo, from examples/nRF54L15_Connect_Kit/
bin = "source"
require = ["connected and encrypted", "both channels streaming"]
forbid  = ["Config QoS failed", "panicked"]

[boards.B]
firmware = "zephyr"        # west, from the Zephyr tree
sample = "samples/bluetooth/bap_unicast_server"
capture = "rtt"           # optional; Zephyr defaults to CDC serial
extra_conf = ["/absolute/path/to/rtt.conf"]
baud = 115200              # serial capture only
require = ["ASE state"]
```

`require`/`forbid` entries are Python regexes matched per line. Once every `require` is seen, capture continues for `settle` seconds (default 3) to catch immediate failures. Any `forbid` fails the run immediately.

**Prefer markers the firmware already prints.** Scrape existing log lines rather than
instrumenting broadly - add a marker to firmware only where pass and fail are genuinely
indistinguishable in the existing output.

Capture method follows firmware kind automatically: trouble-audio streams defmt over RTT through `probe-rs run --non-interactive`; Zephyr defaults to bridged CDC serial. Set `capture = "rtt"` and provide an RTT console overlay for Zephyr images where DK-target UART pins do not match Connect Kit hardware.

## Scenario order

`unicast-self` is the baseline and must pass before any Zephyr scenario is trusted - it
reproduces what the example README already claims works, so that a later failure can be
attributed to non-conformance rather than a broken harness.

## Known-suspect behaviors

Things already documented in the source that these tests are meant to finally check. Each is a
place where two boards running our own code would never disagree:

- `bap.rs:100` - presentation-delay range hardcoded non-degenerate as an Android interop
  workaround; `preferred_retransmission_number: 13` and `max_transport_latency: 100` likewise
  hardcoded.
- `cig.rs:105` - the `nrf-sdc` `LE Set CIG Parameters Test` workaround drops per-CIS
  retransmission control (`nse: 1, bn: 1`, no RTN). Does a conformant peer accept that QoS?
- `cis.rs:607` - fragmented ISO SDUs are **dropped** in passthrough mode. Our source never
  fragments; a foreign one may.
- `ase_client.rs` - implements 3 of 8 ASE Control Point opcodes. Our *server* claims all 8, but
  nothing has ever sent it the other 5 over a radio.
- Nine modules carry guessed assigned numbers with "verify before relying on the exact values
  for certification" comments (`aics.rs:10`, `csis.rs:22`, `bass.rs:8`, `has.rs:6`, `ots.rs:15`,
  `mcs.rs:30`, `tbs.rs:12`, `gmas.rs:7`, `tmas.rs:7`). A wrong UUID is invisible to self-testing
  and fatal against any real peer.
- `has.rs:9` - Preset Control Point response uses `notify_raw`; the spec requires Indicate.
