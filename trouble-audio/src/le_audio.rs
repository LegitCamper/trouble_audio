//! The GATT *client* side of an LE Audio unicast peripheral: an initiator/hub discovering a
//! remote peripheral's PACS/ASCS. Lighter-weight than a ready-made peripheral runner, since what
//! to *do* with the discovered characteristics is inherently application logic that can't be
//! automated away.
//!
//! There's no equivalent ready-made *peripheral* (GATT server) runner in this crate - that's
//! opinionated glue code (advertise, accept, drive the ASE Control Point state machine, persist
//! bonds, ...) that only really proves out the sink role today, so it lives as example code
//! instead (see the `sink` module in `examples/apps`).

use trouble_host::prelude::*;

use crate::{ascs::AscsClient, pacs::PacsClient};

/// The discovered GATT client side of an LE Audio unicast peripheral: PACS to read its
/// capabilities, ASCS to control its ASEs. Bundles the two discovery calls that would otherwise
/// need repeating by hand; driving actual audio session logic (config codec, enable, etc.) from
/// here is up to the caller, since it depends on what the application is trying to do.
pub struct LeAudioClient {
    pub pacs: PacsClient,
    pub ascs: AscsClient,
}

impl LeAudioClient {
    /// Discovers PACS and ASCS on an already-connected `GattClient`. The returned client's
    /// background task (`client.task()`) must still be run concurrently by the caller (e.g. via
    /// `embassy_futures::select` alongside whatever uses `pacs`/`ascs`) for GATT requests to
    /// complete at all.
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Self {
        let pacs = PacsClient::new(client).await;
        let ascs = AscsClient::new(client).await;
        Self { pacs, ascs }
    }
}
