//! ## Published Audio Capabilities Service
//!
//! The Published Audio Capabilities (PACS) service exposes
//! server audio capabilities and audio availability, allowing discovery by clients.

use super::generic_audio::*;
#[cfg(feature = "server")]
use super::LEAudioGattServer;

use core::slice;
use trouble_host::{prelude::*, types::gatt_traits::*};

#[cfg(feature = "client")]
use bt_hci::uuid::{
    characteristic::{
        AVAILABLE_AUDIO_CONTEXTS, SINK_AUDIO_LOCATIONS, SINK_PAC, SOURCE_AUDIO_LOCATIONS,
        SOURCE_PAC, SUPPORTED_AUDIO_CONTEXTS,
    },
    service::PUBLISHED_AUDIO_CAPABILITIES,
};

#[cfg(feature = "defmt")]
use defmt::*;

use crate::CodecId;

#[cfg(feature = "server")]
pub(crate) async fn pacs_gatt_server(
    _server: &LEAudioGattServer<'_>,
    _connection_data: GattData<'_>,
) {
    #[cfg(feature = "defmt")]
    match _connection_data.process(_server).await {
        Ok(_) => {}
        Err(_e) => {
            #[cfg(feature = "defmt")]
            warn!("[gatt] error processing event: {:?}", _e);
        }
    }
}

#[cfg(feature = "client")]
pub(crate) async fn pacs_gatt_client<'a, C>(
    client: &GattClient<'a, C, { crate::client::NUMBER_OF_SERVICES }, 24>,
) where
    C: bt_hci::controller::Controller,
{
    let services = client
        .services_by_uuid(&Uuid::from(PUBLISHED_AUDIO_CAPABILITIES))
        .await
        .unwrap();
    let service = services.first().unwrap().clone();

    // Sink PAC characteristic containing one or more PAC records
    let sink_pac: Characteristic<u8> = client
        .characteristic_by_uuid(&service, &Uuid::from(SINK_PAC))
        .await
        .unwrap();

    let mut sink_pac_listener = client.subscribe(&sink_pac, true).await.unwrap();

    // Sink Audio Locations characteristic
    let sink_audio_locations: Characteristic<u8> = client
        .characteristic_by_uuid(&service, &Uuid::from(SINK_AUDIO_LOCATIONS))
        .await
        .unwrap();

    let mut sink_audio_locations_listener =
        client.subscribe(&sink_audio_locations, true).await.unwrap();

    // Supported Audio Contexts characteristic
    let supported_audio_contexts: Characteristic<u8> = client
        .characteristic_by_uuid(&service, &Uuid::from(SUPPORTED_AUDIO_CONTEXTS))
        .await
        .unwrap();

    let mut supported_audio_contexts = client
        .subscribe(&supported_audio_contexts, true)
        .await
        .unwrap();

    embassy_futures::join::join(
        embassy_futures::join::join(
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

/// Published Audio Capabilities Service
#[cfg(feature = "server")]
#[gatt_service(uuid = bt_hci::uuid::service::PUBLISHED_AUDIO_CAPABILITIES)]
pub struct Pacs {
    /// Source PAC characteristic containing one or more PAC records
    #[characteristic(uuid = SOURCE_PAC, read, notify)]
    pub source_pac: PAC,

    /// Source Audio Locations characteristic
    #[characteristic(uuid = SOURCE_AUDIO_LOCATIONS, read, notify, write)]
    pub source_audio_locations: AudioLocation,

    /// Available Audio Contexts characteristic
    #[characteristic(uuid = AVAILABLE_AUDIO_CONTEXTS, read, notify)]
    pub available_audio_contexts: AudioContexts,
}

/// A set of parameter values that denote server audio capabilities.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default)]
pub struct PACRecord {
    pub codec_id: CodecId,
    pub codec_specific_capabilities: &'static [CodecSpecificCapabilities],
    pub metadata: &'static [Metadata],
}

/// The Sink Audio Locations characteristic i
/// The Source PAC characteristic is used to expose PAC records when the server supports transmission of audio data.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Default, Debug)]
pub struct PAC {
    number_of_pac_records: u8,
    pac_records: &'static [PACRecord],
}

impl PAC {
    fn new(records: &'static [PACRecord]) -> Self {
        Self {
            number_of_pac_records: records.len() as u8,
            pac_records: records,
        }
    }
}

impl FixedGattValue for PAC {
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
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
    const SIZE: usize = size_of::<PACRecord>();

    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != Self::SIZE {
            Err(FromGattError::InvalidLength)
        } else {
            unsafe { Ok((data.as_ptr() as *const Self).read_unaligned()) }
        }
    }

    fn to_gatt(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, Self::SIZE) }
    }
}
