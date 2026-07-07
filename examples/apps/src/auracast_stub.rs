//! Demonstrates the `Auracast` (broadcast) scenario shape - and its current limits.
//!
//! [`trouble_audio::scenario::Scenario::auracast_sink`]/[`Scenario::auracast_source`] exist so
//! this API's shape won't need to change once broadcast support exists, but
//! [`trouble_audio::scenario::Scenario::run`] returns
//! [`trouble_audio::scenario::ScenarioError::NotImplemented`] immediately for them today - no
//! `LE Create BIG`/periodic advertising support exists anywhere in this crate or the underlying
//! `trouble-host` fork yet. This example exists to show handling that error, not to demonstrate a
//! working broadcast receiver.

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use trouble_audio::{
    cis::CisManager,
    iso::{LeAcceptCisRequest, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetupIsoDataPath},
    scenario::{Scenario, ScenarioError},
};
use trouble_host::prelude::*;

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 3;
pub const MAX_ASES: usize = 1;

/// Attempts to run an Auracast broadcast sink. Always returns
/// `Err(ScenarioError::NotImplemented(..))` today, without touching `controller` at all - callers
/// should treat that as "not supported yet", not a transient failure worth retrying.
pub async fn run<C>(controller: C, cis_manager: &CisManager<NoopRawMutex, MAX_ASES>) -> ScenarioError
where
    C: Controller
        + ControllerCmdAsync<LeAcceptCisRequest>
        + ControllerCmdSync<LeRejectCisRequest>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeSetHostFeature>
        + ControllerCmdSync<LeReadLocalSupportedFeatures>,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xfc]);
    let scenario = Scenario::auracast_sink("Ble Auracast Sink");

    match scenario
        .run::<C, NoopRawMutex, MAX_ASES, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX>(
            controller,
            address,
            IoCapabilities::NoInputNoOutput,
            None,
            cis_manager,
        )
        .await
    {
        Ok(never) => match never {},
        Err(e) => {
            #[cfg(feature = "defmt")]
            defmt::warn!("[auracast_stub] Auracast isn't implemented yet");
            #[cfg(feature = "log")]
            log::warn!("[auracast_stub] Auracast isn't implemented yet: {:?}", e);
            e
        }
    }
}
