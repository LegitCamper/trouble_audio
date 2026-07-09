//! The GATT *client* side of an LE Audio unicast peripheral: an initiator/hub discovering a
//! remote peripheral's PACS/ASCS. Lighter-weight than a ready-made peripheral runner, since what
//! to *do* with the discovered characteristics is application logic that can't be automated away.
//!
//! The complementary peripheral runner lives in `examples/apps`'s `sink` module instead - it only
//! proves out the sink role so far.

use trouble_host::prelude::*;

use crate::{
    aics::AicsClient, ascs::AscsClient, bass::BassClient, csis::CsisClient, gmas::GmasClient, has::HasClient, mcs::McsClient,
    mics::MicsClient, ots::OtsClient, pacs::PacsClient, tbs::TbsClient, tmas::TmasClient, vcs::VcsClient, vocs::VocsClient,
};

/// The discovered GATT client side of an LE Audio unicast peripheral. PACS/ASCS are mandatory, so
/// discovery panics if either is missing; every other service is optional per its own spec, so
/// those fields are `None` when the peer doesn't expose them. Driving actual audio session logic
/// (config codec, enable, mute, ...) is up to the caller.
pub struct LeAudioClient {
    pub pacs: PacsClient,
    pub ascs: AscsClient,
    pub mics: Option<MicsClient>,
    pub vcs: Option<VcsClient>,
    pub csis: Option<CsisClient>,
    pub mcs: Option<McsClient>,
    pub gmas: Option<GmasClient>,
    pub tmas: Option<TmasClient>,
    pub aics: Option<AicsClient>,
    pub vocs: Option<VocsClient>,
    pub has: Option<HasClient>,
    pub bass: Option<BassClient>,
    pub tbs: Option<TbsClient>,
    pub ots: Option<OtsClient>,
}

impl LeAudioClient {
    /// Discovers PACS, ASCS, and every optional LE Audio service present on an already-connected
    /// `GattClient`. The caller must still run `client.task()` concurrently (e.g. via
    /// `embassy_futures::select`) for GATT requests to complete at all.
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Self {
        let pacs = PacsClient::new(client).await;
        let ascs = AscsClient::new(client).await;
        let mics = MicsClient::discover(client).await;
        let vcs = VcsClient::discover(client).await;
        let csis = CsisClient::discover(client).await;
        let mcs = McsClient::discover(client).await;
        let gmas = GmasClient::discover(client).await;
        let tmas = TmasClient::discover(client).await;
        let aics = AicsClient::discover(client).await;
        let vocs = VocsClient::discover(client).await;
        let has = HasClient::discover(client).await;
        let bass = BassClient::discover(client).await;
        let tbs = TbsClient::discover(client).await;
        let ots = OtsClient::discover(client).await;
        Self {
            pacs,
            ascs,
            mics,
            vcs,
            csis,
            mcs,
            gmas,
            tmas,
            aics,
            vocs,
            has,
            bass,
            tbs,
            ots,
        }
    }
}
