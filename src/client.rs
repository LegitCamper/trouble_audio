use embassy_futures::join::join_array;
use trouble_host::{gatt::GattClient, prelude::Connection, Stack};

use super::pacs::pacs_gatt_client;

pub(crate) const NUMBER_OF_SERVICES: usize = 10;

pub async fn run_client<'a, C>(stack: &'a Stack<'a, C>, conn: &'a Connection<'a>)
where
    C: bt_hci::controller::Controller,
{
    let client = GattClient::<C, NUMBER_OF_SERVICES, 24>::new(stack, conn)
        .await
        .unwrap();

    join_array([
        pacs_gatt_client(&client), //
    ])
    .await;
}
