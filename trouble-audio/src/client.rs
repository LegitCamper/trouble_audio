use embassy_futures::select::select;
use trouble_host::{gatt::GattClient, Controller};

pub trait LeAudioClientService {}

pub async fn run_client<C: Controller, const L2CAP_MTU: usize>(
    client: &GattClient<'_, C, 10, L2CAP_MTU>,
) {
    select(client.task(), async {
        // pacs::sink_client(&client)
    })
    .await;
}
