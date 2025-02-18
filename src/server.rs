#[cfg(feature = "defmt")]
use defmt::*;

use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{GattData, GattEvent, ReadEvent, WriteEvent},
    prelude::{AttErrorCode, AttributeServer, AttributeTable, GattValue},
};

use crate::{
    generic_audio::AudioLocation,
    pacs::{AudioContexts, PacsServer, PAC, PACS_ATTRIBUTES},
};

pub const MAX_SERVICES: usize = 4 // att
     + PACS_ATTRIBUTES // pacs
  ;

pub trait LeAudioServerService {
    fn handle_read_event(&self, event: &ReadEvent) -> Option<Result<(), AttErrorCode>>;
    fn handle_write_event(&self, event: &WriteEvent) -> Option<Result<(), AttErrorCode>>;
}

pub struct ServerBuilder<'a, const ATT_MTU: usize, M: RawMutex> {
    table: AttributeTable<'a, M, MAX_SERVICES>,
    pacs: Option<PacsServer<ATT_MTU>>,
}

impl<'a, const ATT_MTU: usize, M: RawMutex> ServerBuilder<'a, ATT_MTU, M> {
    const STORAGE_SIZE: usize = MAX_SERVICES * ATT_MTU;

    pub fn new(
        name_id: &'a impl GattValue,
        appearance: &'a impl GattValue,
        storage: &'a mut [u8],
    ) -> Self {
        #[cfg(feature = "defmt")]
        if storage.len() < Self::STORAGE_SIZE {
            defmt::panic!(
                "storage len: {}, but needs to be {}",
                storage.len(),
                Self::STORAGE_SIZE
            );
        }

        let mut table: AttributeTable<'_, M, MAX_SERVICES> = AttributeTable::new();
        let mut svc = table.add_service(trouble_host::attribute::Service::new(0x1800u16));
        let _ = svc.add_characteristic_ro(0x2a00u16, name_id);
        let _ = svc.add_characteristic_ro(0x2a01u16, appearance);
        svc.build();

        // Generic attribute service (mandatory)
        table.add_service(trouble_host::attribute::Service::new(0x1801u16));

        Self { table, pacs: None }
    }

    pub fn build(self) -> Server<'a, ATT_MTU, M> {
        Server {
            server: AttributeServer::<M, MAX_SERVICES>::new(self.table),
            pacs: self.pacs.expect("Pacs is a mandatory service"),
        }
    }

    pub fn add_pacs(
        &mut self,
        sink_pac: Option<(PAC, &'a mut [u8])>,
        sink_audio_locations: Option<(AudioLocation, &'a mut [u8])>,
        source_pac: Option<(PAC, &'a mut [u8])>,
        source_audio_locations: Option<(AudioLocation, &'a mut [u8])>,
        supported_audio_contexts: (AudioContexts, &'a mut [u8]),
        available_audio_contexts: (AudioContexts, &'a mut [u8]),
    ) {
        let pacs = PacsServer::<ATT_MTU>::new(
            &mut self.table,
            sink_pac,
            sink_audio_locations,
            source_pac,
            source_audio_locations,
            supported_audio_contexts,
            available_audio_contexts,
        );
        self.pacs = Some(pacs);
    }
}

pub struct Server<'a, const ATT_MTU: usize, M: RawMutex> {
    server: AttributeServer<'a, M, MAX_SERVICES>,
    pacs: PacsServer<ATT_MTU>,
}

impl<const ATT_MTU: usize, M: RawMutex> Server<'_, ATT_MTU, M> {
    pub async fn process(&self, gatt_data: GattData<'_>) {
        match gatt_data.process(&self.server).await {
            Ok(data) => {
                if let Some(event) = data {
                    if let Some(resp) = match event {
                        GattEvent::Read(ref read_event) => self.pacs.handle_read_event(&read_event),
                        GattEvent::Write(ref write_event) => {
                            self.pacs.handle_write_event(&write_event)
                        }
                    } {
                        if let Err(err) = resp {
                            event.reject(err).unwrap().send().await
                        } else {
                            event.accept().unwrap().send().await
                        };
                    } else {
                        #[cfg(feature = "defmt")]
                        warn!("[le audio] There was no known handler to handle this event");
                        event
                            .reject(AttErrorCode::INVALID_HANDLE)
                            .unwrap()
                            .send()
                            .await;
                    }
                }
            }
            Err(e) => {
                #[cfg(feature = "defmt")]
                warn!("[le audio] error processing event: {:?}", e);
            }
        }
    }
}
