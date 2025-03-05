//! Audio Stream Control Service
//!
//! This service exposes an interface for Audio Stream Endpoints (ASEs),
//! which enables clients to discover, configure, establish,and
//! control the ASEs and their associated unicast Audio Streams.

use core::{mem::size_of, slice};
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use static_cell::StaticCell;
use trouble_host::{connection::PhySet, prelude::*, types::gatt_traits::*};

#[cfg(feature = "defmt")]
use defmt::{assert, info, warn};

use crate::{CodecId, LeAudioServerService, MAX_SERVICES};

/// A Gatt service for controlling unicast audio streams
///
/// MAX_ASES is the max number of sink ases and source ases the device supports
/// MAX_CONNECTIONS is the max number clients each ase can handle
pub struct AscsServer<const MAX_ASES: usize, const MAX_CONNECTIONS: usize> {
    handle: u16,
    ase_control_point: Characteristic<AseControlOpcode>,
    ases: Vec<Vec<Characteristic<AseType>, MAX_CONNECTIONS>, MAX_ASES>,
}

impl<const MAX_ASES: usize, const MAX_CONNECTIONS: usize> AscsServer<MAX_ASES, MAX_CONNECTIONS> {
    /// Create a new Ascs Gatt Service
    ///
    /// MAX_ASES is the number of audio stream endpoints you wish to support PER client/connection
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        ases: Vec<AseType, MAX_ASES>,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::AUDIO_STREAM_CONTROL));

        static CONTROL_STORE: StaticCell<[u8; 90]> = StaticCell::new();
        let ase_control_point_char = service
            .add_characteristic(
                characteristic::ASE_CONTROL_POINT,
                &[
                    CharacteristicProp::Write,
                    CharacteristicProp::WriteWithoutResponse,
                    CharacteristicProp::Notify,
                ],
                AseControlOpcode::Disable,
                CONTROL_STORE.init([0; 90]),
            )
            .build();

        let mut ase_chars = Vec::new();
        for ase in ases.iter() {
            let mut ases_handles = Vec::new();
            for _ in 0..MAX_CONNECTIONS {
                static ASE_STORE: StaticCell<[u8; 90]> = StaticCell::new();
                ases_handles.push(match ase {
                    AseType::Source(_) => service
                        .add_characteristic(
                            characteristic::SOURCE_ASE,
                            &[CharacteristicProp::Read, CharacteristicProp::Notify],
                            ase.clone(),
                            ASE_STORE.init([0; 90]),
                        )
                        .build(),
                    AseType::Sink(_) => service
                        .add_characteristic(
                            characteristic::SINK_ASE,
                            &[CharacteristicProp::Read, CharacteristicProp::Notify],
                            ase.clone(),
                            ASE_STORE.init([0; 90]),
                        )
                        .build(),
                });
            }
            ase_chars
                .push(ases_handles)
                .map_err(|_| "Adding ASE endpoint exceeded MAX_SERVICES")
                .unwrap()
        }

        Self {
            handle: service.build(),
            ase_control_point: ase_control_point_char,
            ases: ase_chars,
        }
    }
}

impl<const MAX_ASES: usize, const MAX_CONNECTIONS: usize> LeAudioServerService
    for AscsServer<MAX_ASES, MAX_CONNECTIONS>
{
    fn handle_read_event(&self, event: &ReadEvent) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.ase_control_point.handle {
            return Some(Err(AttErrorCode::WRITE_REQUEST_REJECTED));
        }
        for ase in self.ases.iter() {
            // TODO: need to retrieve which ase belongs to each client
            for client_ase in ase {
                if event.handle() == client_ase.handle {
                    return Some(Ok(()));
                }
            }
        }

        None
    }

    fn handle_write_event(&self, event: &WriteEvent) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.ase_control_point.handle {
            return Some(Ok(()));
        }
        for ase in self.ases.iter() {
            for client_ase in ase {
                if event.handle() == client_ase.handle {
                    return Some(Err(AttErrorCode::WRITE_REQUEST_REJECTED));
                }
            }
        }

        None
    }
}

#[derive(Default, Clone)]
pub struct Ase {
    /// Identifier of this ASE, assigned by the server.
    pub id: u8,
    state_id: u8,
    /// State of the ASE with respect to the ASE state machine
    pub state: AseState,
}

impl Ase {
    pub fn new(id: u8) -> Self {
        Self {
            id,
            state_id: 0,
            state: AseState::Idle,
        }
    }
}

/// Represents the ASE Control Operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AseControlOperation {
    ConfigCodec = 1,
    ConfigQos = 2,
    Enable = 3,
    ReceiverStartReady = 4,
    ReceiverStopReady = 5,
    Disable = 6,
    UpdateMetadata = 7,
    Release = 8,
    Released,
}

/// Represents the device initiating the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitiatingDevice {
    Client,
    Server,
    ClientOrServer, // Covers cases where either can initiate
}

/// Represents the ASE Type (Sink or Source).
#[derive(Clone)]
pub enum AseType {
    Source(Ase),
    Sink(Ase),
}

impl FixedGattValue for AseType {
    const SIZE: usize = size_of::<Ase>();

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

#[derive(Default, Clone)]
#[repr(u8)]
pub enum AseState {
    #[default]
    Idle = 0,
    CodecConfigured(AseParamsCodecConfigured) = 1,
    QosConfigured(AseParamsQoSConfigured) = 2,
    Enabling(AseParamsOther) = 3,
    Streaming(AseParamsOther) = 4,
    Disabling(AseParamsOther) = 5,
    Releasing = 6,
    RFU,
}

/// Additional Ase parameters for the State::CodedConfigured
#[derive(Clone)]
pub struct AseParamsCodecConfigured {
    /// Server support for unframed ISOAL PDUs
    pub framing: u8,
    /// Server preferred value for the PHY parameter
    pub preferred_phy: PhySet,
    /// Server preferred value for the Retransmission_Number parameter
    pub preferred_retransmission_number: u8,
    /// Maximum server supported value for the Max_Transport_Latency parameter (in milliseconds)
    pub max_transport_latency: u16,
    /// Minimum server supported Presentation_Delay (in microseconds)
    pub presentation_delay_min: u32,
    /// Maximum server supported Presentation_Delay (in microseconds)
    pub presentation_delay_max: u32,
    /// Server preferred minimum Presentation_Delay (in microseconds)
    pub preferred_presentation_delay_min: u32,
    /// Server preferred maximum Presentation_Delay (in microseconds)
    pub preferred_presentation_delay_max: u32,
    /// Codec ID
    pub codec_id: CodecId,
    /// Length of the Codec_Specific_Configuration field
    pub codec_specific_configuration_length: u8,
    /// Codec specific configuration for this ASE
    pub codec_specific_configuration: Option<&'static [u8]>,
}

impl Default for AseParamsCodecConfigured {
    fn default() -> Self {
        Self {
            framing: Default::default(),
            preferred_phy: PhySet::M2,
            preferred_retransmission_number: Default::default(),
            max_transport_latency: Default::default(),
            presentation_delay_min: Default::default(),
            presentation_delay_max: Default::default(),
            preferred_presentation_delay_min: Default::default(),
            preferred_presentation_delay_max: Default::default(),
            codec_id: Default::default(),
            codec_specific_configuration_length: Default::default(),
            codec_specific_configuration: Default::default(),
        }
    }
}

/// Additional Ase parameters for the State::QoSConfigured
#[derive(Clone)]
pub struct AseParamsQoSConfigured {
    pub cig_id: u8,
    pub cis_id: u8,
    pub sdu_interval: [u8; 3],
    pub framing: u8,
    pub phy: PhySet,
    pub max_sdu: u16,
    pub retransmission_number: u8,
    pub max_transport_latency: u16,
    pub presentation_delay: [u8; 3],
}

impl Default for AseParamsQoSConfigured {
    fn default() -> Self {
        Self {
            cig_id: Default::default(),
            cis_id: Default::default(),
            sdu_interval: Default::default(),
            framing: Default::default(),
            phy: PhySet::M2,
            max_sdu: Default::default(),
            retransmission_number: Default::default(),
            max_transport_latency: Default::default(),
            presentation_delay: Default::default(),
        }
    }
}

/// Additional Ase parameters for the State::Enabling, State::Steaming, or State::Disabled
#[derive(Default, Clone)]
pub struct AseParamsOther {
    pub cig_id: u8,
    pub cis_id: u8,
    pub metadata: Option<u64>,
}

#[repr(u8)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AseControlOpcode {
    ConfigCodec = 0x01,        // Configures codec parameters
    ConfigQoS = 0x02,          // Configures preferred CIS parameters
    Enable = 0x03,             // Applies codec parameters and starts coupling
    ReceiverStartReady = 0x04, // Signals readiness to receive audio data
    Disable = 0x05,            // Decouples a Source ASE or Sink ASE
    ReceiverStopReady = 0x06,  // Signals readiness to stop receiving audio data
    UpdateMetadata = 0x07,     // Updates metadata for one or more ASEs
    Release = 0x08,            // Releases resources associated with an ASE
    Released = 0x09,           // Transitions ASE to Idle or Codec Configured state
    Rfu = 0xFF,                // Reserved for future use
}

impl FixedGattValue for AseControlOpcode {
    const SIZE: usize = 1;

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
