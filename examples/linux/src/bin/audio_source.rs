//! Plays a generated test tone out to the `audio_sink`/`rp-pico-w` sink example over LE Audio,
//! acting as the central: connects, negotiates both Sink ASEs, and streams. See
//! `trouble_audio_example_apps::basic_audio_source` for the actual discovery/negotiation/
//! streaming logic - this just wires up the Linux raw-HCI transport, same as `audio_sink.rs`.

use bt_hci::controller::ExternalController;
use bt_hci_linux::Transport;
use linux_examples::bond_store::source_bond_store;
use trouble_audio_example_apps::{basic_audio_sink, basic_audio_source};

/// This example's own address - distinct from the sink's fixed address so the two don't collide.
const OUR_ADDRESS: [u8; 6] = [0xff, 0x8f, 0x1c, 0x05, 0xe4, 0xff];

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), std::io::Error> {
    env_logger::init();
    let dev = match std::env::args().collect::<Vec<_>>()[..] {
        [_] => 0,
        [_, ref s] => s.parse::<u16>().expect("Could not parse device number"),
        _ => panic!(
            "Provide the device number as the one and only command line argument, or no arguments to use device 0."
        ),
    };
    let transport = Transport::new(dev)?;
    let controller = ExternalController::<_, 8>::new(transport);
    let bond_store = source_bond_store();

    basic_audio_source::run(controller, OUR_ADDRESS, basic_audio_sink::ADDRESS, Some(&bond_store)).await
}
