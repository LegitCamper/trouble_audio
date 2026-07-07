//! An LE Audio headset-with-microphone peripheral (sink + source), built on
//! [`trouble_audio::scenario::Scenario`] with volume control and mic mute enabled - the kind of
//! device a phone headset or gaming headset would be.
//!
//! **Known gap** (see `trouble_audio::scenario`'s module doc): the Source ASE (mic) side
//! negotiates correctly and MICS mute control works, but no encoded mic audio is actually sent
//! over the air yet.

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use trouble_audio::{
    cis::CisManager,
    iso::{LeAcceptCisRequest, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetupIsoDataPath},
    le_audio::BondStore,
    scenario::Scenario,
};
use trouble_host::prelude::*;

/// Max number of connections. This crate's `AscsServer` models a single active connection.
const CONNECTIONS_MAX: usize = 1;
/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC
/// Max number of Sink/Source ASEs this device exposes (one Sink ASE + one Source ASE).
pub const MAX_ASES: usize = 2;

/// Runs the headset-with-mic peripheral forever on the given controller.
///
/// `cis_manager` is caller-owned so the caller can concurrently drain
/// [`CisManager::receive_pcm`] for decoded playback audio, same as `basic_audio_sink::run`.
pub async fn run<C>(controller: C, cis_manager: &CisManager<NoopRawMutex, MAX_ASES>, bond_store: Option<&dyn BondStore>) -> !
where
    C: Controller
        + ControllerCmdAsync<LeAcceptCisRequest>
        + ControllerCmdSync<LeRejectCisRequest>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xfd]);

    let scenario = Scenario::headset_with_mic("Ble Audio Headset")
        .with_volume_control()
        .with_microphone_control();

    match scenario
        .run::<C, NoopRawMutex, MAX_ASES, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX>(
            controller,
            address,
            IoCapabilities::NoInputNoOutput,
            bond_store,
            cis_manager,
        )
        .await
    {
        Ok(never) => match never {},
        Err(_e) => {
            #[cfg(feature = "defmt")]
            defmt::error!("[headset_with_mic] scenario error");
            #[cfg(feature = "log")]
            log::error!("[headset_with_mic] scenario error");
            panic!("headset_with_mic scenario failed to start")
        }
    }
}
