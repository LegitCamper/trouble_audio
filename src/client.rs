use heapless::Vec;
use trouble_host::{
    gap::CentralConfig,
    gatt::{GattClient, ServiceHandle},
    prelude::{
        appearance, gatt_server, AdStructure, AddrKind, Address, Advertisement, BdAddr, Central,
        ConnectConfig, Connection, ConnectionEvent, GapConfig, Peripheral, PeripheralConfig,
        ScanConfig, BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE,
    },
    BleHostError, Controller, Stack,
};

use super::pacs::{Pacs, PACS_UUID};

const NUMBER_OF_SERVICES: usize = 10;

/// LE Audio GATT Client
#[cfg(feature = "client")]
pub struct LEAudioGattClient<'a, C>
where
    C: bt_hci::controller::Controller,
{
    client: GattClient<'a, C, NUMBER_OF_SERVICES, 24>,
    services: Vec<ServiceHandle, NUMBER_OF_SERVICES>,
}

impl<'a, C> LEAudioGattClient<'a, C>
where
    C: bt_hci::controller::Controller,
{
    pub async fn new(stack: &'a Stack<'a, C>, conn: &'a Connection<'a>) -> Self {
        let client = GattClient::<C, NUMBER_OF_SERVICES, 24>::new(stack, conn)
            .await
            .unwrap();
        let services = client.services_by_uuid(&PACS_UUID).await.unwrap();

        Self { client, services }
    }
}
