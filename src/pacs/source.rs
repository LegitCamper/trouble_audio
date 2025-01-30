use bt_hci::uuid::{characteristic, service};
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::*;

pub async fn source_client<'a, C: Controller, const MAX_SERVICES: usize, const L2CAP_MTU: usize>(
    client: &GattClient<'a, C, MAX_SERVICES, L2CAP_MTU>,
) {
    #[cfg(feature = "defmt")]
    info!("Looking for pacs service");

    let services = client
        .services_by_uuid(&Uuid::from(service::PUBLISHED_AUDIO_CAPABILITIES))
        .await
        .unwrap();
    if services.is_empty() {
        #[cfg(feature = "defmt")]
        info!("Pacs not supported by server");
        return;
    }
    let service = services.first().unwrap().clone();

    // Sink PAC characteristic containing one or more PAC records
    let sink_pac: Characteristic<u8> = client
        .characteristic_by_uuid(&service, &Uuid::from(characteristic::SINK_PAC))
        .await
        .unwrap();

    let mut sink_pac_listener = client.subscribe(&sink_pac, true).await.unwrap();

    // Sink Audio Locations characteristic
    let sink_audio_locations: Characteristic<u8> = client
        .characteristic_by_uuid(&service, &Uuid::from(characteristic::SINK_AUDIO_LOCATIONS))
        .await
        .unwrap();

    let mut sink_audio_locations_listener =
        client.subscribe(&sink_audio_locations, true).await.unwrap();

    // Supported Audio Contexts characteristic
    let supported_audio_contexts: Characteristic<u8> = client
        .characteristic_by_uuid(
            &service,
            &Uuid::from(characteristic::SUPPORTED_AUDIO_CONTEXTS),
        )
        .await
        .unwrap();

    let mut supported_audio_contexts = client
        .subscribe(&supported_audio_contexts, true)
        .await
        .unwrap();

    select(
        select(
            async {
                loop {
                    let data = sink_pac_listener.next().await;
                    info!(
                        "Got notification: {:?} (val: {})",
                        data.as_ref(),
                        data.as_ref()[0]
                    );
                }
            },
            async {
                loop {
                    let data = sink_audio_locations_listener.next().await;
                    info!(
                        "Got notification: {:?} (val: {})",
                        data.as_ref(),
                        data.as_ref()[0]
                    );
                }
            },
        ),
        async {
            loop {
                let data = supported_audio_contexts.next().await;
                info!(
                    "Got notification: {:?} (val: {})",
                    data.as_ref(),
                    data.as_ref()[0]
                );
            }
        },
    )
    .await;
}

pub fn source_server<'a, C: Controller, const MAX_SERVICES: usize>(
    pacs: &super::PacsSource,
    gatt_event: &GattEvent<'a, 'a>,
) {
    match gatt_event {
        GattEvent::Read(event) => {
            if event.handle() == pacs.source_pac.handle {
            } else if event.handle() == pacs.source_audio_locations.handle {
            } else if event.handle() == pacs.available_audio_contexts.handle {
            }
        }
        GattEvent::Write(event) => {
            if event.handle() == pacs.source_pac.handle {
            } else if event.handle() == pacs.source_audio_locations.handle {
            } else if event.handle() == pacs.available_audio_contexts.handle {
            }
            #[cfg(feature = "defmt")]
            info!(
                "[gatt] Write Event to Level Characteristic: {:?}",
                event.data()
            );
        }
    }
}
