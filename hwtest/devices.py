"""Discovery and role assignment for the attached nRF54L15 Connect Kits.

Each Connect Kit presents one CMSIS-DAP probe plus two CDC-ACM interfaces from its onboard
nRF52820 interface MCU. Per the board DTS
(boards/makerdiary/nrf54l15_connectkit/nrf54l15_connectkit_nrf52820.dts), `cdc_acm_uart0` is
bridged to the nRF54L15's own `uart30` (P0.0/P0.1) and `cdc_acm_uart1` is the interface MCU's
shell. The bridged one is always the lower USB interface number, so console = bInterfaceNumber 01.

Roles are "A" and "B"; which physical board gets which is fixed by sorted serial so repeat runs
stay comparable. Override with HWTEST_SERIAL_A / HWTEST_SERIAL_B.
"""

from __future__ import annotations

import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

VID_PID = "2fe3:0204"
CHIP = "nRF54L15"
SPEED = "4000"

# The nRF52820 bridges the nRF54L15's uart30 to its first CDC interface.
CONSOLE_USB_INTERFACE = "01"


class HardwareError(RuntimeError):
    pass


@dataclass(frozen=True)
class Board:
    role: str
    serial: str
    console: str | None

    @property
    def probe(self) -> str:
        """probe-rs --probe selector. The full serial is required: both boards answer to
        2fe3:0204 and probe-rs refuses an ambiguous selector."""
        return f"{VID_PID}:{self.serial}"

    def require_console(self) -> str:
        if self.console is None:
            raise HardwareError(
                f"board {self.role} ({self.serial}) has no CDC console; "
                f"a UART-reading scenario cannot run against it"
            )
        return self.console


def list_probe_serials() -> list[str]:
    try:
        out = subprocess.run(
            ["probe-rs", "list"], capture_output=True, text=True, timeout=30
        ).stdout
    except FileNotFoundError as exc:
        raise HardwareError("probe-rs not found on PATH") from exc
    except subprocess.TimeoutExpired as exc:
        raise HardwareError("`probe-rs list` timed out") from exc

    # e.g. "[0]: IFMCU CMSIS-DAP -- 2fe3:0204-0:820D9A5F0FC04A038AA34 (CMSIS-DAP)"
    serials = re.findall(rf"{re.escape(VID_PID)}-\d+:(\S+)", out)
    return sorted(set(serials))


def _udev_property(device: str, name: str) -> str | None:
    out = subprocess.run(
        ["udevadm", "info", "-q", "property", "-n", device],
        capture_output=True,
        text=True,
        timeout=15,
    ).stdout
    for line in out.splitlines():
        key, _, value = line.partition("=")
        if key == name:
            return value
    return None


def console_for_serial(serial: str) -> str | None:
    """The /dev/ttyACM* carrying this board's nRF54L15 console, or None."""
    for device in sorted(Path("/dev").glob("ttyACM*")):
        path = str(device)
        if _udev_property(path, "ID_SERIAL_SHORT") != serial:
            continue
        if _udev_property(path, "ID_USB_INTERFACE_NUM") == CONSOLE_USB_INTERFACE:
            return path
    return None


def discover(required: int = 2) -> dict[str, Board]:
    """Map role -> Board. Raises HardwareError unless `required` boards are present."""
    serials = list_probe_serials()

    roles = ["A", "B", "C", "D"][:required]
    pinned = {
        role: os.environ.get(f"HWTEST_SERIAL_{role}")
        for role in roles
        if os.environ.get(f"HWTEST_SERIAL_{role}")
    }
    if len(set(pinned.values())) != len(pinned):
        raise HardwareError(f"HWTEST_SERIAL_* assigns one probe to multiple roles: {pinned}")

    missing = [serial for serial in pinned.values() if serial not in serials]
    if missing:
        raise HardwareError(
            f"HWTEST_SERIAL_* names probe(s) that are not attached: {missing}; "
            f"attached: {serials}"
        )

    remaining = iter(serial for serial in serials if serial not in pinned.values())
    assigned: dict[str, str] = {}
    for role in roles:
        serial = pinned.get(role)
        if serial is None:
            serial = next(remaining, None)
        if serial is None:
            raise HardwareError(
                f"need {required} nRF54L15 Connect Kit probe(s), found {len(serials)}: {serials}. "
                f"Check USB connections."
            )
        assigned[role] = serial

    return {
        role: Board(role=role, serial=serial, console=console_for_serial(serial))
        for role, serial in assigned.items()
    }


if __name__ == "__main__":
    for role, board in discover().items():
        print(f"{role}: serial={board.serial} probe={board.probe} console={board.console}")
