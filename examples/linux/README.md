# Trouble on Linux

## Running the Application

To allow the user-space application to bind to the raw HCI socket, you must release the controller from the system Bluetooth daemon and grant the binary network capabilities.

If your have two bluetooth interfaces you can run the two examples below for a full source -> sink test

```bash
sudo systemctl stop bluetooth
sudo btmgmt info # shows the bluetooth interfaces
```

Run a linux sink
```bash
HCI=0
cargo build --bin audio_sink
sudo btmgmt -i hci$HCI power off
sudo setcap cap_net_raw,cap_net_admin+eip target/debug/audio_sink
RUST_LOG=debug target/debug/audio_sink $HCI
```

Run a linux source
```bash
HCI=1
cargo build --bin audio_source
sudo btmgmt -i hci$HCI power off
sudo setcap cap_net_raw,cap_net_admin+eip target/debug/audio_source
RUST_LOG=debug target/debug/audio_source $HCI
```
