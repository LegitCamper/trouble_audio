//! Audio Stream Control Service
//!
//! This service exposes an interface for Audio Stream Endpoints (ASEs),
//! which enables clients to discover, configure, establish,and
//! control the ASEs and their associated unicast Audio Streams.

use core::{mem::size_of_val, slice};
use defmt::*;
use trouble_host::{prelude::*, scan::PhySet, types::gatt_traits::*, Error};

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
    #[characteristic(uuid = "2BC6", read, write, notify)]
    ase_control_point: Ase,
}

pub struct Ase {
    /// Identifier of this ASE, assigned by the server.
    pub id: u8,
    /// State of the ASE with respect to the ASE state machine
    pub state: AseState,
    pub params: AseParams,
}

pub struct AseParams {
    /// Server support for unframed ISOAL PDUs
    pub farming: bool,
    /// Server preferred value for the PHY parameter to be written by the
    /// client for this ASE in the Config QoS operation defined in Section 5.2
    preferred_phy: PhySet,
    /// Server preferred value for the Retransmission_Number parameter
    /// to be written by the client for this ASE in the Config
    /// QoS operation defined in Section 5.2. The Retransmission_Number
    /// parameter is defined in Volume 4, Part E, Section 7.8.97 in [1].
    /// If the server expresses a value for Preferred_Retransmission_Number
    /// that is not 0xFF, the server shall support all values of Retransmission_Number
    /// up to and including Preferred_Retransmission_Number.
    preferred_retransmission_number: u8,
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
    Idle = 0x00,
    CodecConfigured = 0x01,
    QosConfigured = 0x02,
    Enabling = 0x03,
    Streaming = 0x04,
    Disabling = 0x05,
    Releasing = 0x06,
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
