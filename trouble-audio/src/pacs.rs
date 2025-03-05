//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use super::{generic_audio::*, CodecId, LeAudioServerService};
use bt_hci::uuid::{characteristic, service};
use core::slice;
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::{prelude::*, types::gatt_traits::*};

use super::MAX_SERVICES;
#[cfg(feature = "defmt")]
use defmt::assert;

/// A Gatt service client for reading exposed Capabilities of an audio server
pub struct PacsClient {
    handle: ServiceHandle,
    pub sink_pac: Option<Characteristic<PAC>>,
    pub sink_audio_locations: Option<Characteristic<AudioLocation>>,
    pub source_pac: Option<Characteristic<PAC>>,
    pub source_audio_locations: Option<Characteristic<AudioLocation>>,
    pub supported_audio_contexts: Characteristic<AudioContexts>,
    pub available_audio_contexts: Characteristic<AudioContexts>,
}

impl PacsClient {
    pub async fn new<'a, T: Controller, const MAX_SERVICES: usize, const L2CAP_MTU: usize>(
        client: &'a mut GattClient<'a, T, MAX_SERVICES, L2CAP_MTU>,
    ) -> Self {
        let services = client
            .services_by_uuid(&Uuid::new_short(
                service::PUBLISHED_AUDIO_CAPABILITIES.into(),
            ))
            .await
            .unwrap();
        let handle = services.first().unwrap();

        let sink_pac = client
            .characteristic_by_uuid(&handle, &Uuid::new_short(characteristic::SINK_PAC.into()))
            .await
            .ok();

        let sink_audio_locations = client
            .characteristic_by_uuid(
                &handle,
                &Uuid::new_short(characteristic::SINK_AUDIO_LOCATIONS.into()),
            )
            .await
            .ok();

        let source_pac = client
            .characteristic_by_uuid(&handle, &Uuid::new_short(characteristic::SOURCE_PAC.into()))
            .await
            .ok();

        let source_audio_locations = client
            .characteristic_by_uuid(
                &handle,
                &Uuid::new_short(characteristic::SOURCE_AUDIO_LOCATIONS.into()),
            )
            .await
            .ok();

        let supported_audio_contexts = client
            .characteristic_by_uuid(
                &handle,
                &Uuid::new_short(characteristic::SUPPORTED_AUDIO_CONTEXTS.into()),
            )
            .await
            .expect("The server Must support SUPPORTED_AUDIO_CONTEXTS");

        let available_audio_contexts = client
            .characteristic_by_uuid(
                &handle,
                &Uuid::new_short(characteristic::AVAILABLE_AUDIO_CONTEXTS.into()),
            )
            .await
            .expect("The server Must support AVAILABLE_AUDIO_CONTEXTS");

        Self {
            handle: handle.clone(),
            sink_pac,
            sink_audio_locations,
            source_pac,
            source_audio_locations,
            supported_audio_contexts,
            available_audio_contexts,
        }
    }
    // TODO: handle subscriptions
}

/// A Gatt service server exposing Capabilities of an audio device
pub struct PacsServer<const ATT_MTU: usize> {
    handle: u16,
    sink_pac: Option<Characteristic<PAC>>,
    sink_audio_locations: Option<Characteristic<AudioLocation>>,
    source_pac: Option<Characteristic<PAC>>,
    source_audio_locations: Option<Characteristic<AudioLocation>>,
    supported_audio_contexts: Characteristic<AudioContexts>,
    available_audio_contexts: Characteristic<AudioContexts>,
}

pub const PACS_ATTRIBUTES: usize = 13;

impl<const ATT_MTU: usize> PacsServer<ATT_MTU> {
    /// Create a new PAC Gatt Service
    ///
    /// If you enable a pac, you must also enable the corresponding location
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        sink_pac: Option<&'a PAC>,
        sink_audio_locations: Option<(AudioLocation, &'a mut [u8])>,
        source_pac: Option<&'a PAC>,
        source_audio_locations: Option<(AudioLocation, &'a mut [u8])>,
        supported_audio_contexts: &'a AudioContexts,
        available_audio_contexts: &'a AudioContexts,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::PUBLISHED_AUDIO_CAPABILITIES));

        let sink_pac_char = match sink_pac {
            Some(sink_pac) => Some(
                service
                    .add_characteristic_ro(characteristic::SINK_PAC, sink_pac)
                    .build(),
            ),
            None => None,
        };

        let sink_audio_locations_char = match sink_audio_locations {
            Some((sink_audio_locations, store)) => {
                #[cfg(feature = "defmt")]
                assert!(store.len() >= ATT_MTU);

                Some(
                    service
                        .add_characteristic(
                            characteristic::SINK_AUDIO_LOCATIONS,
                            &[
                                CharacteristicProp::Read,
                                CharacteristicProp::Notify,
                                CharacteristicProp::Write,
                            ],
                            sink_audio_locations,
                            store,
                        )
                        .build(),
                )
            }
            None => None,
        };

        let source_pac_char = match source_pac {
            Some(source_pac) => Some(
                service
                    .add_characteristic_ro(characteristic::SOURCE_PAC, source_pac)
                    .build(),
            ),
            None => None,
        };

        let source_audio_locations_char = match source_audio_locations {
            Some((source_audio_locations, store)) => {
                #[cfg(feature = "defmt")]
                assert!(store.len() >= ATT_MTU);

                Some(
                    service
                        .add_characteristic(
                            characteristic::SOURCE_AUDIO_LOCATIONS,
                            &[
                                CharacteristicProp::Read,
                                CharacteristicProp::Notify,
                                CharacteristicProp::Write,
                            ],
                            source_audio_locations,
                            store,
                        )
                        .build(),
                )
            }
            None => None,
        };

        let supported_audio_contexts_char = service
            .add_characteristic_ro(
                characteristic::SUPPORTED_AUDIO_CONTEXTS,
                supported_audio_contexts,
            )
            .build();

        let available_audio_contexts_char = service
            .add_characteristic_ro(
                characteristic::AVAILABLE_AUDIO_CONTEXTS,
                available_audio_contexts,
            )
            .build();

        Self {
            handle: service.build(),
            sink_pac: sink_pac_char,
            sink_audio_locations: sink_audio_locations_char,
            source_pac: source_pac_char,
            source_audio_locations: source_audio_locations_char,
            supported_audio_contexts: supported_audio_contexts_char,
            available_audio_contexts: available_audio_contexts_char,
        }
    }
}

impl<const ATT_MTU: usize> LeAudioServerService for PacsServer<ATT_MTU> {
    fn handle_read_event(
        &self,
        event: &ReadEvent,
    ) -> Option<Result<(), trouble_host::prelude::AttErrorCode>> {
        if let Some(sink_pac) = &self.sink_pac {
            if event.handle() == sink_pac.handle {
                return Some(Ok(()));
            }
            if let Some(sink_audio_locations) = &self.sink_audio_locations {
                if event.handle() == sink_audio_locations.handle {
                    return Some(Ok(()));
                }
            }
        }

        if let Some(source_pac) = &self.source_pac {
            if event.handle() == source_pac.handle {
                return Some(Ok(()));
            }
            if let Some(source_audio_locations) = &self.source_audio_locations {
                if event.handle() == source_audio_locations.handle {
                    return Some(Ok(()));
                }
            }
        }

        if event.handle() == self.supported_audio_contexts.handle {
            return Some(Ok(()));
        }

        if event.handle() == self.available_audio_contexts.handle {
            return Some(Ok(()));
        }

        None
    }

    fn handle_write_event(
        &self,
        event: &WriteEvent,
    ) -> Option<Result<(), trouble_host::prelude::AttErrorCode>> {
        if let Some(sink_pac) = &self.sink_pac {
            if event.handle() == sink_pac.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
            if let Some(sink_audio_locations) = &self.sink_audio_locations {
                if event.handle() == sink_audio_locations.handle {
                    if event.data().len() == size_of::<AudioLocation>() {
                        if let Ok(data) = event.value(sink_audio_locations) {
                            if data.bits() <= AudioLocation::RightSurround.bits() {
                                return Some(Ok(()));
                            }
                        }
                    };
                    return Some(Err(AttErrorCode::WRITE_REQUEST_REJECTED));
                }
            }
        }

        if let Some(source_pac) = &self.source_pac {
            if event.handle() == source_pac.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
            if let Some(source_audio_locations) = &self.source_audio_locations {
                if event.handle() == source_audio_locations.handle {
                    if event.data().len() == size_of::<AudioLocation>() {
                        if let Ok(data) = event.value(source_audio_locations) {
                            if data.bits() <= AudioLocation::RightSurround.bits() {
                                return Some(Ok(()));
                            }
                        }
                    };
                    return Some(Err(AttErrorCode::WRITE_REQUEST_REJECTED));
                }
            }
        }

        if event.handle() == self.supported_audio_contexts.handle {
            return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
        }

        if event.handle() == self.available_audio_contexts.handle {
            return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
        }

        None
    }
}

// A set of parameter values that denote server audio capabilities.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone)]
pub struct PACRecord {
    pub codec_id: Vec<CodecId, 5>,
    pub codec_specific_capabilities: Vec<CodecSpecificCapabilities, 5>, // cap only has 5 elemenhts
    pub metadata: Vec<Metadata, 13>, // Metadata only has 13 elements
}

// 5 may be too small
const MAX_NUMBER_PAC_RECORDS: usize = 5;

/// The Sink Audio Locations characteristic i
/// The Source PAC characteristic is used to expose PAC records when the server supports transmission of audio data.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub struct PAC {
    number_of_pac_records: u8,
    pac_records: Vec<PACRecord, MAX_NUMBER_PAC_RECORDS>,
}

impl PAC {
    pub fn new(records: Vec<PACRecord, MAX_NUMBER_PAC_RECORDS>) -> Self {
        Self {
            number_of_pac_records: records.len() as u8,
            pac_records: records,
        }
    }
}

impl FromGatt for PAC {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() < Self::MIN_SIZE || data.len() > Self::MAX_SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }
}
impl AsGatt for PAC {
    const MIN_SIZE: usize = size_of::<PACRecord>() + 1;
    const MAX_SIZE: usize = size_of::<PAC>();
    fn as_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::MAX_SIZE) }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub struct AudioContexts {
    /// Bitmask of audio data Context Type values for reception.
    pub sink_contexts: ContextType,
    /// Bitmask of audio data Context Type values for transmission.
    pub source_contexts: ContextType,
}

impl FixedGattValue for AudioContexts {
    const SIZE: usize = size_of::<Self>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn as_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}
