use embassy_futures::select::select;
use trouble_host::{gatt::GattClient, prelude::PacketPool, Controller};

pub trait LeAudioClientService {}

/// Runs the `GattClient`'s required background task. Callers issuing requests through `client`
/// must run this concurrently (e.g. via `select`) for those requests to complete.
pub async fn run_client<C: Controller, P: PacketPool>(client: &GattClient<'_, C, P, 10>) {
    select(client.task(), async {}).await;
}
