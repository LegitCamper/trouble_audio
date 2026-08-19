//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use alloc::vec::Vec;

use super::{generic_audio::*, CodecId, DiscoveryError, LeAudioServerService, MAX_SERVICES};
use bt_hci::uuid::{characteristic, service};
use core::mem::size_of;
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

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
    /// Discovers the PACS service and its characteristics on an already-connected `GattClient`.
    pub async fn new<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Result<Self, DiscoveryError<T::Error>> {
        let services = client.services_by_uuid(&Uuid::from(service::PUBLISHED_AUDIO_CAPABILITIES)).await?;
        let handle = services.first().ok_or(DiscoveryError::ServiceNotFound)?;

        let sink_pac = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SINK_PAC))
            .await
            .ok();

        let sink_audio_locations = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SINK_AUDIO_LOCATIONS))
            .await
            .ok();

        let source_pac = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SOURCE_PAC))
            .await
            .ok();

        let source_audio_locations = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SOURCE_AUDIO_LOCATIONS))
            .await
            .ok();

        let supported_audio_contexts = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SUPPORTED_AUDIO_CONTEXTS))
            .await
            .map_err(|_| DiscoveryError::CharacteristicNotFound)?;

        let available_audio_contexts = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::AVAILABLE_AUDIO_CONTEXTS))
            .await
            .map_err(|_| DiscoveryError::CharacteristicNotFound)?;

        Ok(Self {
            handle: handle.clone(),
            sink_pac,
            sink_audio_locations,
            source_pac,
            source_audio_locations,
            supported_audio_contexts,
            available_audio_contexts,
        })
    }
    // TODO: handle subscriptions
}

/// A Gatt service server exposing Capabilities of an audio device
pub struct PacsServer {
    handle: u16,
    sink_pac: Option<Characteristic<PAC>>,
    sink_audio_locations: Option<Characteristic<AudioLocation>>,
    source_pac: Option<Characteristic<PAC>>,
    source_audio_locations: Option<Characteristic<AudioLocation>>,
    supported_audio_contexts: Characteristic<AudioContexts>,
    available_audio_contexts: Characteristic<AudioContexts>,
}

pub const PACS_ATTRIBUTES: usize = 16;

impl PacsServer {
    /// Create a new PAC Gatt Service
    ///
    /// If you enable a pac, you must also enable the corresponding location
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        sink_pac: Option<(&'a PAC, &'a mut [u8])>,
        sink_audio_locations: Option<(&'a AudioLocation, &'a mut [u8])>,
        source_pac: Option<(&'a PAC, &'a mut [u8])>,
        source_audio_locations: Option<(&'a AudioLocation, &'a mut [u8])>,
        supported_audio_contexts: &'a AudioContexts,
        available_audio_contexts: &'a AudioContexts,
        available_audio_contexts_store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::PUBLISHED_AUDIO_CAPABILITIES));

        // The Common Audio Profile requires an encrypted link for every PACS/ASCS
        // characteristic, so every characteristic added by this service requires at least
        // `PermissionLevel::EncryptionRequired` for both read and write. Per the PACS spec, PAC
        // characteristics must support Notify (their records can change at runtime) - Android's
        // LE Audio client actively disconnects devices that lack this (see the analogous, but
        // fatal for Android, `AVAILABLE_AUDIO_CONTEXTS` case below).
        let sink_pac_char = match sink_pac {
            Some((sink_pac, store)) => Some(
                service
                    .add_characteristic(
                        characteristic::SINK_PAC,
                        &[CharacteristicProp::Read, CharacteristicProp::Notify],
                        sink_pac.clone(),
                        store,
                    )
                    .read_permission(PermissionLevel::EncryptionRequired)
                    .build(),
            ),
            None => None,
        };

        let sink_audio_locations_char = match sink_audio_locations {
            Some((sink_audio_locations, store)) => Some(
                service
                    .add_characteristic(
                        characteristic::SINK_AUDIO_LOCATIONS,
                        &[
                            CharacteristicProp::Read,
                            CharacteristicProp::Notify,
                            CharacteristicProp::Write,
                        ],
                        *sink_audio_locations,
                        store,
                    )
                    .read_permission(PermissionLevel::EncryptionRequired)
                    .write_permission(PermissionLevel::EncryptionRequired)
                    .build(),
            ),
            None => None,
        };

        let source_pac_char = match source_pac {
            Some((source_pac, store)) => Some(
                service
                    .add_characteristic(
                        characteristic::SOURCE_PAC,
                        &[CharacteristicProp::Read, CharacteristicProp::Notify],
                        source_pac.clone(),
                        store,
                    )
                    .read_permission(PermissionLevel::EncryptionRequired)
                    .build(),
            ),
            None => None,
        };

        let source_audio_locations_char = match source_audio_locations {
            Some((source_audio_locations, store)) => Some(
                service
                    .add_characteristic(
                        characteristic::SOURCE_AUDIO_LOCATIONS,
                        &[
                            CharacteristicProp::Read,
                            CharacteristicProp::Notify,
                            CharacteristicProp::Write,
                        ],
                        *source_audio_locations,
                        store,
                    )
                    .read_permission(PermissionLevel::EncryptionRequired)
                    .write_permission(PermissionLevel::EncryptionRequired)
                    .build(),
            ),
            None => None,
        };

        let supported_audio_contexts_char = service
            .add_characteristic_ro(characteristic::SUPPORTED_AUDIO_CONTEXTS, supported_audio_contexts)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        // Per the PACS spec, Available_Audio_Contexts must support Notify (its value can change
        // at runtime) - unlike this, Android's LE Audio client actively disconnects devices whose
        // Available_Audio_Contexts characteristic has no CCC descriptor.
        let available_audio_contexts_char = service
            .add_characteristic(
                characteristic::AVAILABLE_AUDIO_CONTEXTS,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                *available_audio_contexts,
                available_audio_contexts_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
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

impl<P: PacketPool> LeAudioServerService<P> for PacsServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), trouble_host::prelude::AttErrorCode>> {
        if let Some(sink_pac) = &self.sink_pac {
            if event.handle() == sink_pac.handle {
                return Some(Ok(()));
            }
        }
        if let Some(sink_audio_locations) = &self.sink_audio_locations {
            if event.handle() == sink_audio_locations.handle {
                return Some(Ok(()));
            }
        }

        if let Some(source_pac) = &self.source_pac {
            if event.handle() == source_pac.handle {
                return Some(Ok(()));
            }
        }
        if let Some(source_audio_locations) = &self.source_audio_locations {
            if event.handle() == source_audio_locations.handle {
                return Some(Ok(()));
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

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), trouble_host::prelude::AttErrorCode>> {
        if let Some(sink_pac) = &self.sink_pac {
            if event.handle() == sink_pac.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
        }
        if let Some(sink_audio_locations) = &self.sink_audio_locations {
            if event.handle() == sink_audio_locations.handle {
                return Some(match event.value(sink_audio_locations) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
                });
            }
        }

        if let Some(source_pac) = &self.source_pac {
            if event.handle() == source_pac.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
        }
        if let Some(source_audio_locations) = &self.source_audio_locations {
            if event.handle() == source_audio_locations.handle {
                return Some(match event.value(source_audio_locations) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
                });
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

/// A single PAC record: one supported codec, its capabilities, and the metadata
/// preferences that go with it. Wire format (all fields back-to-back, LE):
/// Codec_ID (5) | Codec_Specific_Capabilities_Length (1) | Codec_Specific_Capabilities (variable)
/// | Metadata_Length (1) | Metadata (variable).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone)]
pub struct PACRecord {
    pub codec_id: CodecId,
    pub codec_specific_capabilities: Vec<CodecSpecificCapabilities>,
    pub metadata: Vec<Metadata>,
}

impl PACRecord {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.codec_id.to_le_bytes());

        let caps = encode_list(&self.codec_specific_capabilities);
        out.push(caps.len() as u8);
        out.extend_from_slice(&caps);

        let metadata = encode_list(&self.metadata);
        out.push(metadata.len() as u8);
        out.extend_from_slice(&metadata);
    }

    /// Decodes one record from the front of `data`, returning it along with the number of
    /// bytes consumed.
    fn decode(data: &[u8]) -> Result<(Self, usize), FromGattError> {
        if data.len() < CodecId::SIZE + 1 {
            return Err(FromGattError::InvalidLength);
        }
        let codec_id = CodecId::from_le_bytes(data[..CodecId::SIZE].try_into().unwrap());
        let mut pos = CodecId::SIZE;

        let caps_len = *data.get(pos).ok_or(FromGattError::InvalidLength)? as usize;
        pos += 1;
        let caps_end = pos.checked_add(caps_len).ok_or(FromGattError::InvalidLength)?;
        let codec_specific_capabilities = decode_list(data.get(pos..caps_end).ok_or(FromGattError::InvalidLength)?)?;
        pos = caps_end;

        let metadata_len = *data.get(pos).ok_or(FromGattError::InvalidLength)? as usize;
        pos += 1;
        let metadata_end = pos.checked_add(metadata_len).ok_or(FromGattError::InvalidLength)?;
        let metadata = decode_list(data.get(pos..metadata_end).ok_or(FromGattError::InvalidLength)?)?;
        pos = metadata_end;

        Ok((
            Self {
                codec_id,
                codec_specific_capabilities,
                metadata,
            },
            pos,
        ))
    }
}

// 5 may be too small
const MAX_NUMBER_PAC_RECORDS: usize = 5;

/// The Sink PAC / Source PAC characteristic value: Number_of_PAC_records (1 byte) followed by
/// that many back-to-back [`PACRecord`]s. Stored pre-encoded since the value is variable-length.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct PAC {
    encoded: Vec<u8>,
}

impl Default for PAC {
    fn default() -> Self {
        Self::new(&[])
    }
}

impl PAC {
    /// Encodes a PAC characteristic value from its records.
    pub fn new(records: &[PACRecord]) -> Self {
        assert!(records.len() <= MAX_NUMBER_PAC_RECORDS);
        let mut encoded = Vec::new();
        encoded.push(records.len() as u8);
        for record in records {
            record.encode(&mut encoded);
        }
        Self { encoded }
    }

    /// Decodes this PAC characteristic value into its records.
    pub fn records(&self) -> Result<Vec<PACRecord>, FromGattError> {
        let data = &self.encoded;
        let count = *data.first().ok_or(FromGattError::InvalidLength)? as usize;
        let mut pos = 1;
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let (record, consumed) = PACRecord::decode(data.get(pos..).ok_or(FromGattError::InvalidLength)?)?;
            records.push(record);
            pos += consumed;
        }
        Ok(records)
    }
}

impl AsGatt for PAC {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for PAC {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let pac = Self { encoded: Vec::from(data) };
        // Validate structure eagerly so a malformed write is rejected up front.
        pac.records()?;
        Ok(pac)
    }
}

/// The Supported/Available_Audio_Contexts characteristic value: Sink_Contexts (2 bytes, LE)
/// followed by Source_Contexts (2 bytes, LE).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug, Clone, Copy)]
#[repr(C)]
pub struct AudioContexts {
    /// Bitmask of audio data Context Type values for reception.
    pub sink_contexts: ContextType,
    /// Bitmask of audio data Context Type values for transmission.
    pub source_contexts: ContextType,
}

impl AsGatt for AudioContexts {
    const MIN_SIZE: usize = 4;
    const MAX_SIZE: usize = 4;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]` guarantees `sink_contexts` then `source_contexts` with no
        // padding (both fields are `#[repr(transparent)]` over u16), matching the wire layout.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

impl FromGatt for AudioContexts {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 4 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            sink_contexts: ContextType::from_bits_truncate(u16::from_le_bytes([data[0], data[1]])),
            source_contexts: ContextType::from_bits_truncate(u16::from_le_bytes([data[2], data[3]])),
        })
    }
}

impl FixedGattValue for AudioContexts {
    const SIZE: usize = 4;
}
