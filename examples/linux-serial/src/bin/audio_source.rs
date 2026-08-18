//! Same as `linux`'s `audio_source`, but talks to the controller over an HCI-UART serial link
//! instead of a raw HCI socket - see `audio_sink.rs` and the crate README for details.

use bt_hci::controller::ExternalController;
use bt_hci_serial::SerialTransport;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use linux_examples::bond_store::source_bond_store;
use tokio::time::Duration;
use tokio_serial::{DataBits, Parity, SerialStream, StopBits};
use trouble_audio_example_apps::{basic_audio_sink, basic_audio_source};

/// This example's own address - distinct from the sink's fixed address so the two don't collide.
const OUR_ADDRESS: [u8; 6] = [0xff, 0x8f, 0x1c, 0x05, 0xe4, 0xff];

const BAUD_RATE: u32 = 1_000_000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), std::io::Error> {
    env_logger::init();
    let port_path = match std::env::args().collect::<Vec<_>>()[..] {
        [_, ref path] => path.clone(),
        _ => {
            panic!("Provide the serial port (e.g. /dev/ttyACM0) as the one and only command line argument.")
        }
    };

    let mut port = SerialStream::open(
        &tokio_serial::new(&port_path, BAUD_RATE)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One),
    )
    .expect("failed to open serial port");

    tokio::time::sleep(Duration::from_secs(1)).await;
    loop {
        let mut buf = [0; 1];
        match port.try_read(&mut buf[..]) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            _ => {}
        }
    }

    let (reader, writer) = tokio::io::split(port);
    let reader = embedded_io_adapters::tokio_1::FromTokio::new(reader);
    let writer = embedded_io_adapters::tokio_1::FromTokio::new(writer);
    let transport: SerialTransport<NoopRawMutex, _, _> = SerialTransport::new(reader, writer);
    let controller = ExternalController::<_, 8>::new(transport);
    let bond_store = source_bond_store();

    basic_audio_source::run(controller, OUR_ADDRESS, basic_audio_sink::ADDRESS, Some(&bond_store)).await
}
