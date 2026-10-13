"""Build and flash firmware onto one board.

Two firmware sources:

- trouble-audio: cargo, from examples/nRF54L15_Connect_Kit/ (must be --release; debug overflows
  the stack during GATT server construction).
- Zephyr: west, from ~/zephyrproject/zephyr.

Both are flashed with probe-rs, not each toolchain's own runner: the Connect Kit board.cmake
selects `pyocd`, which is not installed here, and probe-rs already knows nRF54L15. Each board is
addressed by full probe serial.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

from devices import CHIP, SPEED, Board, HardwareError

REPO = Path(__file__).resolve().parent.parent
NRF_EXAMPLE_DIR = REPO / "examples" / "nRF54L15_Connect_Kit"

ZEPHYR_BASE = Path(os.environ.get("ZEPHYR_BASE", Path.home() / "zephyrproject" / "zephyr"))
ZEPHYR_VENV = Path(
    os.environ.get("ZEPHYR_VENV", Path.home() / "zephyrproject" / ".venv")
)
# Makerdiary's custom board Kconfig builds BT images without a working controller. The official
# nRF54L15 DK target has the same SoC/radio memory map and boots correctly on Connect Kit hardware;
# Zephyr scenarios use RTT, so board-specific UART/LED pins do not matter.
ZEPHYR_BOARD = "nrf54l15dk/nrf54l15/cpuapp"
ZEPHYR_SDK = Path(
    os.environ.get("ZEPHYR_SDK_INSTALL_DIR", Path.home() / "zephyr-sdk-0.17.0")
)

# `west build` writes here; board and overlays key each sample's build directory.
BUILD_ROOT = REPO / "hwtest" / "build"


def _run(cmd: list[str], cwd: Path, env: dict | None = None, timeout: int = 1800) -> None:
    printable = " ".join(cmd)
    print(f"  $ {printable}", flush=True)
    try:
        result = subprocess.run(cmd, cwd=cwd, env=env, timeout=timeout)
    except FileNotFoundError as exc:
        raise HardwareError(f"command not found: {cmd[0]}") from exc
    except subprocess.TimeoutExpired as exc:
        raise HardwareError(f"command timed out after {timeout}s: {printable}") from exc
    if result.returncode != 0:
        raise HardwareError(f"command failed ({result.returncode}): {printable}")


def build_trouble_audio(binary: str, env_extra: dict | None = None) -> Path:
    """Build a trouble-audio example binary; returns the ELF path.

    The path comes from cargo's own JSON output rather than being guessed: this crate is a
    workspace member, so artifacts land in the workspace-root target dir, but stale ELFs from
    older per-directory builds can still sit in examples/nRF54L15_Connect_Kit/target and would
    silently flash the wrong firmware.
    """
    print(f"[build] trouble-audio --bin {binary}")
    cmd = [
        "cargo", "build", "--release", "--bin", binary,
        "--message-format", "json-render-diagnostics",
    ]
    print(f"  $ {' '.join(cmd)}", flush=True)
    try:
        result = subprocess.run(
            cmd,
            cwd=NRF_EXAMPLE_DIR,
            capture_output=True,
            text=True,
            timeout=2400,
            env={**os.environ, **(env_extra or {})},
        )
    except FileNotFoundError as exc:
        raise HardwareError("cargo not found on PATH") from exc
    except subprocess.TimeoutExpired as exc:
        raise HardwareError(f"cargo build --bin {binary} timed out") from exc
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise HardwareError(f"cargo build --bin {binary} failed ({result.returncode})")

    executable: str | None = None
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("executable") and message.get("target", {}).get("name") == binary:
            executable = message["executable"]

    if executable is None:
        raise HardwareError(f"cargo reported no executable for --bin {binary}")
    return Path(executable)


def build_zephyr(sample: str, extra_conf: list[str] | None = None) -> Path:
    """Build a Zephyr sample for the Connect Kit; returns the ELF path.

    `sample` is relative to the Zephyr tree, e.g. "samples/bluetooth/bap_unicast_server".
    """
    west = ZEPHYR_VENV / "bin" / "west"
    if not west.exists():
        raise HardwareError(
            f"west not found at {west}; set ZEPHYR_VENV to the zephyrproject venv"
        )
    if not ZEPHYR_SDK.exists():
        raise HardwareError(f"Zephyr SDK not found at {ZEPHYR_SDK}")

    source = ZEPHYR_BASE / sample
    if not source.exists():
        raise HardwareError(f"no such Zephyr sample: {source}")

    conf = extra_conf or []
    cache_input = "\0".join([ZEPHYR_BOARD, *conf])
    cache_key = hashlib.sha256(cache_input.encode()).hexdigest()[:8]
    build_dir = BUILD_ROOT / f"{sample.replace('/', '_')}_{cache_key}"
    env = dict(os.environ)
    env["ZEPHYR_BASE"] = str(ZEPHYR_BASE)
    env["ZEPHYR_TOOLCHAIN_VARIANT"] = "zephyr"
    env["ZEPHYR_SDK_INSTALL_DIR"] = str(ZEPHYR_SDK)
    # The SDK ships dtc; Zephyr also wants it on PATH.
    sdk_bin = ZEPHYR_SDK / "sysroots" / "x86_64-pokysdk-linux" / "usr" / "bin"
    env["PATH"] = f"{ZEPHYR_VENV / 'bin'}:{sdk_bin}:{env.get('PATH', '')}"

    cmd = [str(west), "build", "-b", ZEPHYR_BOARD, "-d", str(build_dir), str(source)]
    if conf:
        cmd += ["--", f"-DEXTRA_CONF_FILE={';'.join(conf)}"]

    print(f"[build] zephyr {sample}")
    _run(cmd, cwd=ZEPHYR_BASE, env=env)

    elf = build_dir / "zephyr" / "zephyr.elf"
    if not elf.exists():
        raise HardwareError(f"west build produced no ELF at {elf}")
    return elf


def flash(board: Board, elf: Path) -> None:
    """Flash an ELF and leave the target reset and halted-free (running)."""
    print(f"[flash] {board.role} ({board.serial}) <- {elf.name}")
    _run(
        [
            "probe-rs",
            "download",
            "--chip",
            CHIP,
            "--probe",
            board.probe,
            "--speed",
            SPEED,
            "--disable-double-buffering",
            str(elf),
        ],
        cwd=REPO,
        timeout=300,
    )


def reset(board: Board) -> None:
    _run(
        [
            "probe-rs",
            "reset",
            "--chip",
            CHIP,
            "--probe",
            board.probe,
            "--speed",
            SPEED,
        ],
        cwd=REPO,
        timeout=120,
    )
