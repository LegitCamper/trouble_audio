use bt_hci::controller::ExternalController;
use bt_hci_linux::Transport;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use linux_examples::{bond_store::sink_bond_store, pipewire_sink};
use trouble_audio::cis::CisManager;
use trouble_audio_example_apps::basic_audio_sink::{self, MAX_ASES};

/// Only sampling frequency the sink example's PAC advertises, so it's the only one a real
/// central can negotiate - the PipeWire stream is set up for it once, up front, rather than
/// renegotiated per connection.
const SAMPLE_RATE_HZ: u32 = 48_000;

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

    let playback = pipewire_sink::spawn_playback(SAMPLE_RATE_HZ);
    let cis_manager = CisManager::<NoopRawMutex, MAX_ASES>::new();
    let bond_store = sink_bond_store();

    let play_decoded_audio = async {
        loop {
            let pcm = cis_manager.receive_pcm().await;
            if playback.send(pcm).is_err() {
                log::error!("[audio_sink] PipeWire playback thread died");
            }
        }
    };

    tokio::select! {
        r = basic_audio_sink::run(controller, &cis_manager, Some(&bond_store)) => r,
        _ = play_decoded_audio => unreachable!("loops forever"),
    }
}
