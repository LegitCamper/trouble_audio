use core::slice::ChunksExactMut;
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::{
    gatt::{GattData, GattEvent, ReadEvent, WriteEvent},
    prelude::{AsGatt, AttErrorCode, AttributeServer, AttributeTable},
};

#[cfg(feature = "defmt")]
use defmt::*;

use crate::{
    ascs::{AscsServer, AseType},
    generic_audio::AudioLocation,
    pacs::{AudioContexts, PacsServer, PAC, PACS_ATTRIBUTES},
};

pub const MAX_SERVICES: usize = 4 // att
     + PACS_ATTRIBUTES  
     + 15 // ascs
     ;

pub trait LeAudioServerService {
    fn handle_read_event(&self, event: &ReadEvent) -> Option<Result<(), AttErrorCode>>;
    fn handle_write_event(&self, event: &WriteEvent) -> Option<Result<(), AttErrorCode>>;
}

// pub struct ServerStorage<'a, const ATT_MTU: usize, const MAX_SERVICES: usize> {
//     storage: [u8],
//     count: usize,
// }

// impl<'a, const ATT_MTU: usize> ServerStorage<'a, ATT_MTU> {
//     pub fn new(storage: &'a mut [u8]) -> Self {
//         Self {
//             storage: storage.chunks_exact_mut(ATT_MTU),
//             count: 0,
//         }
//     }
//     fn next(&mut self) -> Option<&'a mut [u8]> {
//         let chunk = self.storage.nth(self.count);
//         self.count += 1;
//         chunk
//     }
// }

pub struct ServerBuilder<
    'a,
    const ATT_MTU: usize,
    const MAX_ASES: usize,
    const MAX_CONNECTIONS: usize,
    M,
> where
    M: RawMutex,
{
    table: AttributeTable<'a, M, MAX_SERVICES>,
    // storage: &'a mut ServerStorage<'a, ATT_MTU>,
    pacs: Option<PacsServer<ATT_MTU>>,
    ascs: Option<AscsServer<MAX_ASES, MAX_CONNECTIONS>>,
}

impl<'a, const ATT_MTU: usize, const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M>
    ServerBuilder<'a, ATT_MTU, MAX_ASES, MAX_CONNECTIONS, M>
where
    M: RawMutex,
{
    const STORAGE_SIZE: usize = MAX_SERVICES * ATT_MTU;

    pub fn new(
        name_id: &'a impl AsGatt,
        appearance: &'a impl AsGatt,
        // storage: &'a mut ServerStorage<'a, ATT_MTU>,
    ) -> Self {
        let mut table: AttributeTable<'_, M, MAX_SERVICES> = AttributeTable::new();
        let mut svc = table.add_service(trouble_host::attribute::Service::new(0x1800u16));
        let _ = svc.add_characteristic_ro(0x2a00u16, name_id);
        let _ = svc.add_characteristic_ro(0x2a01u16, appearance);
        svc.build();

        // Generic attribute service (mandatory)
        table.add_service(trouble_host::attribute::Service::new(0x1801u16));

        Self {
            table,
            // storage,
            pacs: None,
            ascs: None,
        }
    }

    pub fn build(self) -> Server<'a, ATT_MTU, MAX_ASES, MAX_CONNECTIONS, M> {
        Server {
            server: AttributeServer::<M, MAX_SERVICES>::new(self.table),
            pacs: self.pacs.expect("Pacs is a mandatory service"),
            ascs: self.ascs,
        }
    }

    pub fn add_pacs(
        mut self,
        sink_pac: Option<&'a PAC>,
        sink_audio_locations: Option<(&'a AudioLocation, &'a mut [u8])>,
        source_pac: Option<&'a PAC>,
        source_audio_locations: Option<(&'a AudioLocation, &'a mut [u8])>,
        supported_audio_contexts: &'a AudioContexts,
        available_audio_contexts: &'a AudioContexts,
    ) -> Self {
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
        self
    }

    pub fn add_ascs(mut self, ases: Vec<AseType, MAX_ASES>) -> Self
    {
        let ascs = AscsServer::new(&mut self.table, ases);
        self.ascs = Some(ascs);
        self
    }
}

pub struct Server<'a, const ATT_MTU: usize, const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M>
where
    M: RawMutex,
{
    server: AttributeServer<'a, M, MAX_SERVICES>,
    pacs: PacsServer<ATT_MTU>,
    ascs: Option<AscsServer<MAX_ASES, MAX_CONNECTIONS>>,
}

impl<const ATT_MTU: usize, const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M>
    Server<'_, ATT_MTU, MAX_ASES, MAX_CONNECTIONS, M>
where
    M: RawMutex,
{
    pub async fn process(&self, gatt_data: GattData<'_>) {
        match gatt_data.process(&self.server).await {
            Ok(data) => {
                if let Some(event) = data {
                    if let Some(resp) = match event {
                        GattEvent::Read(ref event) => self.handle_read(event),
                        GattEvent::Write(ref event) => self.handle_write(event),
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

    fn handle_read(&self, event: &ReadEvent) -> Option<Result<(), AttErrorCode>> {
        if let Some(res) = self.pacs.handle_read_event(event) {
            Some(res)
        } else if let Some(ascs) = &self.ascs {
            if let Some(res) = ascs.handle_read_event(event) {
                Some(res)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn handle_write(&self, event: &WriteEvent) -> Option<Result<(), AttErrorCode>> {
        if let Some(res) = self.pacs.handle_write_event(event) {
            Some(res)
        } else if let Some(ascs) = &self.ascs {
            if let Some(res) = ascs.handle_write_event(event) {
                Some(res)
            } else {
                None
            }
        } else {
            None
        }
    }
}
