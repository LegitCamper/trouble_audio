use bt_hci::uuid::service::*;
#[cfg(feature = "defmt")]
use defmt::*;
use embassy_futures::{join::join_array, select::select};
use embassy_time::Timer;
use static_cell::StaticCell;
use trouble_host::{
    gatt::GattClient,
    prelude::{
        AdStructure, Advertisement, Connection, Peripheral, Uuid, BR_EDR_NOT_SUPPORTED,
        LE_GENERAL_DISCOVERABLE,
    },
    BleHostError, Controller, Stack,
};

use super::pacs::pacs_gatt_client;

pub(crate) const NUMBER_OF_SERVICES: usize = 10;

pub async fn run_client<'a, C: Controller>(
    name: &'a str,
    stack: &'a Stack<'a, C>,
    peripheral: &mut Peripheral<'a, C>,
) {
    static SERVICES: StaticCell<[Uuid; 1]> = StaticCell::new();
    let services = SERVICES.init([Uuid::from(PUBLISHED_AUDIO_CAPABILITIES)]);

    loop {
        match advertise(name, services, peripheral).await {
            Ok(conn) => {
                let client = GattClient::<C, NUMBER_OF_SERVICES, 24>::new(stack, &conn)
                    .await
                    .unwrap();

                select(client.task(), join_array([pacs_gatt_client(&client)])).await;
            }
            Err(e) => {
                #[cfg(feature = "defmt")]
                let e = defmt::Debug2Format(&e);
                #[cfg(feature = "defmt")]
                defmt::panic!("[adv] error: {:?}", e);
            }
        }
    }
}

/// Create an advertiser to use to connect to a BLE Central, and wait for it to connect.
async fn advertise<'a, C: Controller>(
    name: &'a str,
    services: &'a [Uuid],
    peripheral: &mut Peripheral<'a, C>,
) -> Result<Connection<'a>, BleHostError<C::Error>> {
    let mut advertiser_data = [0; 31];
    AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids16(services),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut advertiser_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..],
                scan_data: &[],
            },
        )
        .await?;
    #[cfg(feature = "defmt")]
    info!("[adv] advertising");
    let conn = advertiser.accept().await?;
    #[cfg(feature = "defmt")]
    info!("[adv] connection established");
    Ok(conn)
}
