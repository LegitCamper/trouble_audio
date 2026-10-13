"""Hardware-in-the-loop scenario runner.

Flashes both boards, captures each one's log stream concurrently, and scans for required and
forbidden markers until the scenario's timeout. Exits nonzero on failure. Full transcripts are
always written to hwtest/logs/, pass or fail - a conformance failure is only useful if the trace
survives.

Capture per board is selected by scenario:
  - trouble-audio: defmt over RTT, via `probe-rs run --non-interactive`.
  - Zephyr: bridged CDC console by default, or RTT with `capture = "rtt"`.

Usage:
    python hwtest/run.py <scenario>
    python hwtest/run.py --all
    python hwtest/run.py --list
"""

from __future__ import annotations

import argparse
import queue
import re
import subprocess
import sys
import threading
import time
import tomllib
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

import serial

import flash
from devices import CHIP, SPEED, Board, HardwareError, discover

HWTEST = Path(__file__).resolve().parent
SCENARIO_DIR = HWTEST / "scenarios"
LOG_DIR = HWTEST / "logs"


@dataclass
class Expectation:
    """What a single board's log stream must and must not contain."""

    role: str
    require: list[str] = field(default_factory=list)
    forbid: list[str] = field(default_factory=list)
    # Numeric assertions over `key=value` pairs found anywhere in a board's log lines. Markers
    # alone cannot tell a clean stream from one dropping a third of its SDUs.
    max_metrics: dict[str, int] = field(default_factory=dict)
    min_metrics: dict[str, int] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self._required = [(p, re.compile(p)) for p in self.require]
        self._forbidden = [(p, re.compile(p)) for p in self.forbid]
        self.satisfied: set[str] = set()
        self.violated: list[tuple[str, str]] = []
        tracked = set(self.max_metrics) | set(self.min_metrics)
        self._metrics = {
            name: re.compile(rf"\b{re.escape(name)}=(\d+)\b") for name in tracked
        }
        # Worst value seen per metric, so one good sample cannot mask a bad one.
        self.metric_worst: dict[str, int] = {}
        self.metric_failures: list[str] = []

    def observe(self, line: str) -> None:
        for pattern, rx in self._required:
            if pattern not in self.satisfied and rx.search(line):
                self.satisfied.add(pattern)
                print(f"  [{self.role}] ✓ {pattern}", flush=True)
        for pattern, rx in self._forbidden:
            if rx.search(line):
                self.violated.append((pattern, line.rstrip()))
                print(f"  [{self.role}] ✗ FORBIDDEN {pattern}: {line.rstrip()}", flush=True)
        for name, rx in self._metrics.items():
            for raw in rx.findall(line):
                self._observe_metric(name, int(raw), line)

    def _observe_metric(self, name: str, value: int, line: str) -> None:
        limit = self.max_metrics.get(name)
        if limit is not None:
            worst = max(self.metric_worst.get(name, value), value)
            self.metric_worst[name] = worst
            if value > limit:
                failure = f"{name}={value} exceeds max {limit}: {line.rstrip()}"
                self.metric_failures.append(failure)
                print(f"  [{self.role}] ✗ METRIC {failure}", flush=True)
            return
        floor = self.min_metrics.get(name)
        if floor is not None:
            worst = min(self.metric_worst.get(name, value), value)
            self.metric_worst[name] = worst
            if value < floor:
                failure = f"{name}={value} below min {floor}: {line.rstrip()}"
                self.metric_failures.append(failure)
                print(f"  [{self.role}] ✗ METRIC {failure}", flush=True)

    @property
    def missing(self) -> list[str]:
        return [p for p, _ in self._required if p not in self.satisfied]

    @property
    def unseen_metrics(self) -> list[str]:
        """Metrics the scenario asserts on that never appeared; silence must not pass."""
        return [name for name in self._metrics if name not in self.metric_worst]

    @property
    def complete(self) -> bool:
        return not self.missing and not self.unseen_metrics


class Capture(threading.Thread):
    """Base for a log source that pushes (role, line) onto a shared queue."""

    def __init__(self, role: str, sink: queue.Queue, stop: threading.Event) -> None:
        super().__init__(daemon=True)
        self.role = role
        self.sink = sink
        self.stop = stop
        self.ready = threading.Event()
        self.error: BaseException | None = None

    def emit(self, line: str) -> None:
        self.sink.put((self.role, line))


class RttCapture(Capture):
    """`probe-rs run`: flash/reset target and stream its RTT output."""

    def __init__(
        self, role: str, board: Board, elf: Path, sink: queue.Queue, stop: threading.Event
    ) -> None:
        super().__init__(role, sink, stop)
        self.board = board
        self.elf = elf
        self.proc: subprocess.Popen | None = None

    def run(self) -> None:
        cmd = [
            "probe-rs", "run",
            "--chip", CHIP,
            "--probe", self.board.probe,
            "--speed", SPEED,
            "--disable-double-buffering",
            "--non-interactive",
            "--no-location",
            str(self.elf),
        ]
        try:
            self.proc = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            )
            assert self.proc.stdout is not None
            for line in self.proc.stdout:
                self.emit(line)
                if "Finished" in line:
                    self.ready.set()
                if self.stop.is_set():
                    break
            returncode = self.proc.wait()
            if returncode != 0 and not self.stop.is_set():
                self.error = HardwareError(f"probe-rs run exited with status {returncode}")
        except BaseException as exc:  # noqa: BLE001 - reported to the main thread
            self.error = exc
        finally:
            self.terminate()

    def terminate(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()


class SerialCapture(Capture):
    """Zephyr's console over the board's bridged CDC port."""

    def __init__(
        self, role: str, device: str, baud: int, sink: queue.Queue, stop: threading.Event
    ) -> None:
        super().__init__(role, sink, stop)
        self.device = device
        self.baud = baud

    def run(self) -> None:
        buffer = b""
        try:
            with serial.Serial(self.device, self.baud, timeout=0.5) as port:
                port.dtr = True
                self.ready.set()
                while not self.stop.is_set():
                    chunk = port.read(256)
                    if not chunk:
                        continue
                    buffer += chunk
                    *lines, buffer = buffer.split(b"\n")
                    for raw in lines:
                        self.emit(raw.decode("utf-8", "replace") + "\n")
        except BaseException as exc:  # noqa: BLE001
            self.error = exc
        finally:
            if buffer:
                self.emit(buffer.decode("utf-8", "replace") + "\n")


def load_scenario(name: str) -> dict:
    path = SCENARIO_DIR / f"{name}.toml"
    if not path.exists():
        raise HardwareError(f"no scenario {name!r} at {path}")
    with path.open("rb") as handle:
        return tomllib.load(handle)


def list_scenarios() -> list[str]:
    return sorted(p.stem for p in SCENARIO_DIR.glob("*.toml"))


def _build(spec: dict) -> Path:
    kind = spec.get("firmware")
    if kind == "trouble-audio":
        return flash.build_trouble_audio(spec["bin"], spec.get("env"))
    if kind == "zephyr":
        return flash.build_zephyr(spec["sample"], spec.get("extra_conf"))
    raise HardwareError(f"unknown firmware kind {kind!r} (want 'trouble-audio' or 'zephyr')")


def run_scenario(name: str, keep_going: bool = False) -> bool:
    scenario = load_scenario(name)
    timeout = scenario.get("timeout", 60)
    settle = scenario.get("settle", 3)
    boards_spec = scenario["boards"]

    print(f"\n=== scenario: {name} ===")
    print(f"    {scenario.get('description', '(no description)')}")

    hardware = discover(required=len(boards_spec))

    # Build everything before touching any board, so a compile error costs no flash cycles.
    elves: dict[str, Path] = {}
    for role, spec in boards_spec.items():
        if role not in hardware:
            raise HardwareError(f"scenario wants board {role!r}; discovered {list(hardware)}")
        elves[role] = _build(spec)

    # UART capture needs Zephyr flashed before opening the serial port. RTT capture uses
    # `probe-rs run`, which performs its own flash/reset.
    for role, spec in boards_spec.items():
        if spec["firmware"] == "zephyr" and spec.get("capture", "serial") == "serial":
            flash.flash(hardware[role], elves[role])

    expectations = {
        role: Expectation(
            role=role,
            require=spec.get("require", []),
            forbid=spec.get("forbid", []),
            max_metrics=spec.get("max_metrics", {}),
            min_metrics=spec.get("min_metrics", {}),
        )
        for role, spec in boards_spec.items()
    }

    sink: queue.Queue = queue.Queue()
    stop = threading.Event()
    captures: list[Capture] = []

    for role, spec in boards_spec.items():
        board = hardware[role]
        if spec["firmware"] == "zephyr" and spec.get("capture", "serial") == "serial":
            captures.append(
                SerialCapture(
                    role, board.require_console(), spec.get("baud", 115200), sink, stop
                )
            )
        else:
            captures.append(RttCapture(role, board, elves[role], sink, stop))

    LOG_DIR.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    log_path = LOG_DIR / f"{name}-{stamp}.log"

    print(f"[run] capturing for up to {timeout}s -> {log_path}")

    with log_path.open("w") as log:
        log.write(f"# scenario: {name}\n# started: {stamp}\n")
        for role, board in hardware.items():
            log.write(f"# board {role}: serial={board.serial} console={board.console}\n")
        log.write("#\n")

        for capture in captures:
            capture.start()
        for capture in captures:
            if not capture.ready.wait(timeout=60):
                capture.error = HardwareError("capture did not become ready within 60 seconds")

        serial_zephyr = [
            role
            for role, spec in boards_spec.items()
            if spec["firmware"] == "zephyr" and spec.get("capture", "serial") == "serial"
        ]
        if serial_zephyr:
            # Interface MCU polls CDC DTR every 500 ms before enabling its target-UART RX path.
            time.sleep(1)
            for role in serial_zephyr:
                flash.reset(hardware[role])

        deadline = time.monotonic() + timeout
        completed_at: float | None = None
        try:
            while time.monotonic() < deadline:
                try:
                    role, line = sink.get(timeout=0.25)
                except queue.Empty:
                    if completed_at is not None and time.monotonic() - completed_at >= settle:
                        break
                    if all(
                        isinstance(c, RttCapture) and not c.is_alive() for c in captures
                    ):
                        print("[run] all capture processes exited")
                        break
                    continue
                log.write(f"[{role}] {line}" if line.endswith("\n") else f"[{role}] {line}\n")
                log.flush()
                expectations[role].observe(line)

                if any(e.violated for e in expectations.values()):
                    print("[run] forbidden marker seen; stopping early")
                    break
                if all(e.complete for e in expectations.values()):
                    if completed_at is None:
                        completed_at = time.monotonic()
                        print(f"[run] all required markers seen; settling for {settle}s")
                    elif time.monotonic() - completed_at >= settle:
                        break
        finally:
            stop.set()
            for capture in captures:
                if isinstance(capture, RttCapture):
                    capture.terminate()
            for capture in captures:
                capture.join(timeout=15)

        # Drain whatever arrived between the last read and shutdown.
        while True:
            try:
                role, line = sink.get_nowait()
            except queue.Empty:
                break
            log.write(f"[{role}] {line}" if line.endswith("\n") else f"[{role}] {line}\n")
            expectations[role].observe(line)

    failures: list[str] = []
    for capture in captures:
        if capture.error is not None:
            failures.append(f"board {capture.role}: capture failed: {capture.error!r}")
    for role, expectation in expectations.items():
        for pattern in expectation.missing:
            failures.append(f"board {role}: never saw required marker: {pattern}")
        for pattern, line in expectation.violated:
            failures.append(f"board {role}: forbidden marker {pattern!r} matched: {line}")
        for metric in expectation.unseen_metrics:
            failures.append(f"board {role}: never saw asserted metric: {metric}")
        for failure in expectation.metric_failures:
            failures.append(f"board {role}: metric out of range: {failure}")
        if expectation.metric_worst:
            worst = " ".join(f"{k}={v}" for k, v in sorted(expectation.metric_worst.items()))
            print(f"  [{role}] worst metrics: {worst}")

    print()
    if failures:
        print(f"FAIL {name}")
        for failure in failures:
            print(f"  - {failure}")
        print(f"  full transcript: {log_path}")
        return False

    print(f"PASS {name}  (transcript: {log_path})")
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario", nargs="?", help="scenario name (see --list)")
    parser.add_argument("--all", action="store_true", help="run every scenario")
    parser.add_argument("--list", action="store_true", help="list scenarios and exit")
    args = parser.parse_args()

    if args.list:
        for name in list_scenarios():
            scenario = load_scenario(name)
            print(f"{name:34s} {scenario.get('description', '')}")
        return 0

    if args.all:
        names = list_scenarios()
        if "unicast-self" in names:
            names.remove("unicast-self")
            names.insert(0, "unicast-self")
    elif args.scenario:
        names = [args.scenario]
    else:
        parser.error("give a scenario name, --all, or --list")

    results: dict[str, bool] = {}
    for name in names:
        try:
            results[name] = run_scenario(name)
        except HardwareError as exc:
            print(f"FAIL {name}: {exc}")
            results[name] = False
        if args.all and name == "unicast-self" and not results[name]:
            print("[run] baseline failed; skipping foreign-peer scenarios")
            break

    if len(results) > 1:
        print("\n=== summary ===")
        for name, ok in results.items():
            print(f"  {'PASS' if ok else 'FAIL'}  {name}")

    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
