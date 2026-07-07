//! A standalone LE Audio microphone peripheral, built on [`trouble_audio::scenario::Scenario`]
//! rather than hand-assembling PACS/ASCS like `basic_audio_sink.rs` does - this is the
//! higher-level equivalent for a source-only device.
//!
//! **Known gap** (see `trouble_audio::scenario`'s module doc): this negotiates a real Source ASE
//! and exposes a working Microphone Control Service mute toggle, but no encoded mic audio is
//! actually sent over the air yet - `CisManager` constructs an LC3 encoder but nothing sends ISO
//! data out. This example is useful for exercising the GATT/pairing/CIS-negotiation side of a mic
//! today, not for capturing real audio.

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
/// Max number of Sink/Source ASEs this device exposes (one Source ASE).
pub const MAX_ASES: usize = 1;

/// Runs the microphone peripheral forever on the given controller. Enables Microphone Control
/// Service (mute) support - see the module doc comment for what does/doesn't actually work yet.
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
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xfe]);

    let scenario = Scenario::microphone("Ble Audio Mic").with_microphone_control();

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
            defmt::error!("[microphone] scenario error");
            #[cfg(feature = "log")]
            log::error!("[microphone] scenario error");
            panic!("microphone scenario failed to start")
        }
    }
}
