//! Audio Stream Control Service
//!
//! This service exposes an interface for Audio Stream Endpoints (ASEs),
//! which enables clients to discover, configure, establish,and
//! control the ASEs and their associated unicast Audio Streams.

use core::{mem::size_of_val, slice};
use defmt::*;
use trouble_host::{prelude::*, scan::PhySet, types::gatt_traits::*, Error};

use crate::CodedId;

/// Audio Stream Control Service
#[gatt_service(uuid = 0x184E)]
pub struct AudioStreamControlService {
    /// Sink PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BC4", read, notify)]
    sink_ase: Ase,

    /// Sink PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BC5", read, notify)]
    source_ase: Ase,

    /// Sink PAC characteristic containing one or more PAC records
    #[characteristic(uuid = "2BC6", write, write_without_response, notify)]
    ase_control_point: Ase,
}

pub struct Ase {
    /// Identifier of this ASE, assigned by the server.
    pub id: u8,
    /// State of the ASE with respect to the ASE state machine
    pub state: AseState,
    // pub params: AseParams, the params are encoded in the state
}

/// Represents the ASE Control Operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AseControlOperation {
    ConfigCodec,
    ConfigQos,
    Enable,
    Release,
    UpdateMetadata,
    Disable,
    ReceiverStartReady,
    ReceiverStopReady,
    ReleasedNoCaching,
    ReleasedCaching,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AseState {
    Idle,
    CodecConfigured(AseParamsCodecConfigured),
    QosConfigured(AseParamsQoSConfigured),
    Enabling(AseParamsOther),
    Streaming(AseParamsOther),
    Disabling(AseParamsOther),
    Releasing,
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
            (Idle, ConfigCodec, ClientOrServer, All) => CodecConfigured,

            // CodecConfigured state transitions
            (CodecConfigured, ConfigCodec, ClientOrServer, All) => CodecConfigured,
            (CodecConfigured, Release, ClientOrServer, All) => Releasing,
            (CodecConfigured, ConfigQos, Client, All) => QosConfigured,

            // QosConfigured state transitions
            (QosConfigured, ConfigCodec, ClientOrServer, All) => CodecConfigured,
            (QosConfigured, ConfigQos, Client, All) => QosConfigured,
            (QosConfigured, Release, ClientOrServer, All) => Releasing,
            (QosConfigured, Enable, Client, All) => Enabling,

            // Enabling state transitions
            (Enabling, Release, ClientOrServer, All) => Releasing,
            (Enabling, UpdateMetadata, ClientOrServer, All) => Enabling,
            (Enabling, Disable, ClientOrServer, Source) => Disabling,
            (Enabling, Disable, ClientOrServer, Sink) => QosConfigured,
            (Enabling, ReceiverStartReady, Client, Source) => Streaming,
            (Enabling, ReceiverStartReady, Server, Sink) => Streaming,

            // Streaming state transitions
            (Streaming, UpdateMetadata, ClientOrServer, All) => Streaming,
            (Streaming, Disable, ClientOrServer, Source) => Disabling,
            (Streaming, Disable, ClientOrServer, Sink) => QosConfigured,
            (Streaming, Release, ClientOrServer, All) => Releasing,

            // Disabling state transitions
            (Disabling, ReceiverStopReady, Client, Source) => QosConfigured,
            (Disabling, Release, ClientOrServer, Source) => Releasing,

            // Releasing state transitions
            (Releasing, ReleasedNoCaching, Server, All) => Idle,
            (Releasing, ReleasedCaching, Server, All) => CodecConfigured,

            _ => {
                warn!("Invalid transition state");
                Disabling
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
    pub codec_id: CodedId,
    /// Length of the Codec_Specific_Configuration field
    pub codec_specific_configuration_length: u8,
    /// Codec specific configuration for this ASE
    pub codec_specific_configuration: Option<&'static [u8]>,
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

/// Additional Ase parameters for the State::Enabling, State::Steaming, or State::Disabled
pub struct AseParamsOther {
    pub cig_id: u8,
    pub cis_id: u8,
    pub metadata: Option<u64>,
}
