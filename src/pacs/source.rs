use bt_hci::uuid::{characteristic, service};
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::*;

pub async fn source_client<'a, C: Controller, const MAX_SERVICES: usize, const L2CAP_MTU: usize>(
    client: &GattClient<'a, C, MAX_SERVICES, L2CAP_MTU>,
) {
    if let Ok(services) = client
        .services_by_uuid(&Uuid::from(service::PUBLISHED_AUDIO_CAPABILITIES))
        .await
    {
        if services.is_empty() {
            return;
        }
        let service = services.first().unwrap().clone();

        let sink_pac_task = async {
            if let Ok(sink_pac) = client
                .characteristic_by_uuid::<super::PAC>(
                    &service,
                    &Uuid::from(characteristic::SINK_PAC),
                )
                .await
            {
                let mut sink_pac_listener = client.subscribe(&sink_pac, true).await.unwrap();
                loop {
                    let data = sink_pac_listener.next().await;
                    info!(
                        "Got notification: {:?} (val: {})",
                        data.as_ref(),
                        data.as_ref()
                    );
                }
            } else {
                return;
            }
        };

        let sink_audio_locations_task = async {
            if let Ok(sink_audio_locations) = client
                .characteristic_by_uuid::<super::AudioLocation>(
                    &service,
                    &Uuid::from(characteristic::SINK_AUDIO_LOCATIONS),
                )
                .await
            {
                let mut sink_audio_locations_listener =
                    client.subscribe(&sink_audio_locations, true).await.unwrap();
                loop {
                    let data = sink_audio_locations_listener.next().await;
                    info!(
                        "Got notification: {:?} (val: {})",
                        data.as_ref(),
                        data.as_ref()
                    );
                }
            } else {
                return;
            }
        };

        let contexts_task = async {
            if let Ok(contexts) = client
                .characteristic_by_uuid::<super::AudioContexts>(
                    &service,
                    &Uuid::from(characteristic::SUPPORTED_AUDIO_CONTEXTS),
                )
                .await
            {
                let mut contexts_listener = client.subscribe(&contexts, true).await.unwrap();
                loop {
                    let data = contexts_listener.next().await;
                    info!(
                        "Got notification: {:?} (val: {})",
                        data.as_ref(),
                        data.as_ref()
                    );
                }
            } else {
                return;
            }
        };

        select(
            select(sink_pac_task, sink_audio_locations_task),
            contexts_task,
        )
        .await;
    }
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
