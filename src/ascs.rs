//! Audio Stream Control Service
//!
//! This service exposes an interface for Audio Stream Endpoints (ASEs),
//! which enables clients to discover, configure, establish,and
//! control the ASEs and their associated unicast Audio Streams.

use core::{mem::size_of, slice};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{connection::PhySet, prelude::*, types::gatt_traits::*};

#[cfg(feature = "defmt")]
use defmt::{assert, info, warn};

use crate::{CodecId, LeAudioServerService, MAX_SERVICES};

/// A Gatt service for controlling unicast audio streams
pub struct AscsServer<const ATT_MTU: usize> {
    handle: trouble_host::attribute::AttributeHandle,
    sink_ase: Option<Characteristic<Ase>>,
    source_ase: Option<Characteristic<Ase>>,
    ase_control_point: Characteristic<Ase>,
}

pub const ASCS_ATTRIBUTES: usize = 6;

impl<const ATT_MTU: usize> AscsServer<ATT_MTU> {
    /// Create a new Ascs Gatt Service
    ///
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        sink_ase: Option<(Ase, &'a mut [u8])>,
        source_ase: Option<(Ase, &'a mut [u8])>,
        ase_control_point: (Ase, &'a mut [u8]),
    ) -> Self {
        let mut service = table.add_service(Service::new(service::AUDIO_STREAM_CONTROL));

        let sink_ase_char = match sink_ase {
            Some((sink_ase, store)) => {
                #[cfg(feature = "defmt")]
                assert!(store.len() >= ATT_MTU);

                Some(
                    service
                        .add_characteristic(
                            characteristic::SINK_ASE,
                            &[CharacteristicProp::Read, CharacteristicProp::Notify],
                            sink_ase,
                            store,
                        )
                        .build(),
                )
            }
            None => None,
        };

        let source_ase_char = match source_ase {
            Some((source_ase, store)) => {
                #[cfg(feature = "defmt")]
                assert!(store.len() >= ATT_MTU);

                Some(
                    service
                        .add_characteristic(
                            characteristic::SOURCE_ASE,
                            &[CharacteristicProp::Read, CharacteristicProp::Notify],
                            source_ase,
                            store,
                        )
                        .build(),
                )
            }
            None => None,
        };

        #[cfg(feature = "defmt")]
        assert!(ase_control_point.1.len() >= ATT_MTU);

        let ase_control_point_char = service
            .add_characteristic(
                characteristic::ASE_CONTROL_POINT,
                &[
                    CharacteristicProp::Write,
                    CharacteristicProp::WriteWithoutResponse,
                    CharacteristicProp::Notify,
                ],
                ase_control_point.0,
                ase_control_point.1,
            )
            .build();

        Self {
            handle: service.build(),
            sink_ase: sink_ase_char,
            source_ase: source_ase_char,
            ase_control_point: ase_control_point_char,
        }
    }
}

impl<const ATT_MTU: usize> LeAudioServerService for AscsServer<ATT_MTU> {
    fn handle_read_event(&self, event: &ReadEvent) -> Option<Result<(), AttErrorCode>> {
        if let Some(sink_ase) = &self.sink_ase {
            if event.handle() == sink_ase.handle {
                return Some(Ok(()));
            }
        }

        if let Some(source_ase) = &self.source_ase {
            if event.handle() == source_ase.handle {
                return Some(Ok(()));
            }
        }

        if event.handle() == self.ase_control_point.handle {
            return Some(Ok(()));
        }

        None
    }

    fn handle_write_event(&self, event: &WriteEvent) -> Option<Result<(), AttErrorCode>> {
        todo!()
    }
}

#[derive(Default)]
pub struct Ase {
    /// Identifier of this ASE, assigned by the server.
    pub id: u8,
    state_id: u8,
    /// State of the ASE with respect to the ASE state machine
    pub state: AseState,
}

impl FixedGattValue for Ase {
    const SIZE: usize = size_of::<Ase>();

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AseType {
    Source,
    Sink,
    All, // Covers cases where the operation is valid for all types
}

#[derive(Default)]
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

impl AseState {
    /// Transition the ASE state based on the operation, initiator, and ASE type.
    pub fn transition(
        self,
        operation: AseControlOperation,
        initiator: InitiatingDevice,
        ase_type: AseType,
    ) -> AseState {
        use AseControlOperation::*;
        use AseState::*;
        use AseType::*;
        use InitiatingDevice::*;

        match (self, operation, initiator, ase_type) {
            // Idle state transitions
            (Idle, ConfigCodec, ClientOrServer, All) => CodecConfigured(Default::default()),

            // CodecConfigured state transitions
            (CodecConfigured(_), ConfigCodec, ClientOrServer, All) => {
                CodecConfigured(Default::default())
            }
            (CodecConfigured(_), Release, ClientOrServer, All) => Releasing,
            (CodecConfigured(_), ConfigQos, Client, All) => QosConfigured(Default::default()),

            // QosConfigured state transitions
            (QosConfigured(_), ConfigCodec, ClientOrServer, All) => {
                CodecConfigured(Default::default())
            }
            (QosConfigured(_), ConfigQos, Client, All) => QosConfigured(Default::default()),
            (QosConfigured(_), Release, ClientOrServer, All) => Releasing,
            (QosConfigured(_), Enable, Client, All) => Enabling(Default::default()),

            // Enabling state transitions
            (Enabling(_), Release, ClientOrServer, All) => Releasing,
            (Enabling(_), UpdateMetadata, ClientOrServer, All) => Enabling(Default::default()),
            (Enabling(_), Disable, ClientOrServer, Source) => Disabling(Default::default()),
            (Enabling(_), Disable, ClientOrServer, Sink) => QosConfigured(Default::default()),
            (Enabling(_), ReceiverStartReady, Client, Source) => Streaming(Default::default()),
            (Enabling(_), ReceiverStartReady, Server, Sink) => Streaming(Default::default()),

            // Streaming state transitions
            (Streaming(_), UpdateMetadata, ClientOrServer, All) => Streaming(Default::default()),
            (Streaming(_), Disable, ClientOrServer, Source) => Disabling(Default::default()),
            (Streaming(_), Disable, ClientOrServer, Sink) => QosConfigured(Default::default()),
            (Streaming(_), Release, ClientOrServer, All) => Releasing,

            // Disabling state transitions
            (Disabling(_), ReceiverStopReady, Client, Source) => QosConfigured(Default::default()),
            (Disabling(_), Release, ClientOrServer, Source) => Releasing,

            // Releasing state transitions
            (Releasing, Released, Server, All) => Idle,
            // (Releasing, ReleasedCaching(_), Server, All) => CodecConfigured::default(),
            _ => {
                #[cfg(feature = "defmt")]
                warn!("Invalid transition state");
                Disabling(Default::default())
            }
        }
    }
}

/// Additional Ase parameters for the State::CodedConfigured
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
#[derive(Default)]
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
