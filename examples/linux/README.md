# Trouble on Linux

## Running the Application

To allow the user-space application to bind to the raw HCI socket, you must release the controller from the system Bluetooth daemon and grant the binary network capabilities.

Run the following command chain to configure the controller (`hci1`), set capabilities, and start the sink:

```bash
cargo build --bin audio_sink
sudo systemctl stop bluetooth && \
sudo btmgmt -i hci1 power off && \
sudo setcap cap_net_raw,cap_net_admin+eip target/debug/audio_sink && \
RUST_LOG=debug target/debug/audio_sink 1
```

> **Note:** If your controller is on a different index (e.g., `hci0`), replace both `hci1` and the trailing launch argument `1` with your device's index.
