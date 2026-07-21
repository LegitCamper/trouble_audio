# Trouble on Linux, over HCI-UART

Same as `../linux` (raw HCI socket against a local Bluetooth adapter), but talks to the
controller over a serial link instead - useful when the controller is a separate board (e.g. an
nRF52840 dongle) rather than an adapter BlueZ/the kernel already knows about. Bond storage
(`FileBondStore`) and PipeWire playback are reused from `linux-examples` rather than duplicated.

## Controller firmware

These examples need a board running an HCI-UART controller firmware. They've been tested with an
nRF52840 dongle running Zephyr's [`hci_uart`
sample](https://docs.zephyrproject.org/latest/samples/bluetooth/hci_uart/README.html):

```bash
west build -p always -b nrf52840dongle samples/bluetooth/hci_uart
```

Unlike `../linux`, there's no `sudo systemctl stop bluetooth` / `setcap` dance - the board just
needs to show up as a serial device (e.g. `/dev/ttyACM0`), and the process needs read/write access
to it (typically via the `dialout`/`uucp` group, depending on distro).

## Running the Application

If you have two boards you can run the two examples below for a full source -> sink test.

Run a linux sink
```bash
PORT=/dev/ttyACM0
cargo build --bin audio_sink
RUST_LOG=debug target/debug/audio_sink $PORT
```

Run a linux source
```bash
PORT=/dev/ttyACM1
cargo build --bin audio_source
RUST_LOG=debug target/debug/audio_source $PORT
```

## Baud rate

Both binaries default to 1,000,000 baud, matching Zephyr's `hci_uart` sample default
(`CONFIG_BT_HCI_UART_BAUDRATE`). Edit `BAUD_RATE` in the relevant `src/bin/*.rs` if your
controller firmware uses a different rate.
