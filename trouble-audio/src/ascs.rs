//! Audio Stream Control Service
//!
//! This service exposes an interface for Audio Stream Endpoints (ASEs),
//! which enables clients to discover, configure, establish, and
//! control the ASEs and their associated unicast Audio Streams.
//!
//! Note: ASEs are inherently per-client in the full spec (a client-specific GATT attribute
//! per connection). This implementation models a peripheral serving a single connection at a
//! time (the common case for LE Audio unicast sinks/sources such as earbuds), so each ASE slot
//! is a single, ordinary characteristic rather than a per-connection one.

use alloc::vec::Vec as AVec;
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::{
    connection::PhySet,
    gatt::{ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};


use crate::{CodecId, DiscoveryError, LeAudioServerService, MAX_SERVICES};

fn phy_set_from_u8(value: u8) -> Result<PhySet, FromGattError> {
    match value {
        1 => Ok(PhySet::M1),
        2 => Ok(PhySet::M2),
        3 => Ok(PhySet::M1M2),
        4 => Ok(PhySet::Coded),
        5 => Ok(PhySet::M1Coded),
        6 => Ok(PhySet::M2Coded),
        7 => Ok(PhySet::M1M2Coded),
        _ => Err(FromGattError::InvalidLength),
    }
}

/// Max number of characteristics [`AscsClient`] discovers under the ASCS service: the ASE Control
/// Point plus up to 2 Sink and 2 Source ASEs - matches this crate's stereo-only scope (one ASE
/// per channel), not the spec's general per-connection limit.
const MAX_ASCS_CHARACTERISTICS: usize = 5;

/// A Gatt service client for discovering and controlling an audio server's ASEs.
///
/// ASEs are untyped (`Characteristic<[u8]>`, not `Characteristic<Ase>`) - `characteristic_by_uuid`
/// only ever returns the *first* characteristic matching a UUID, so a server with more than one
/// same-direction ASE (e.g. a 2-ASE stereo sink) needs the UUID-agnostic
/// [`GattClient::characteristics`] instead, filtered here by UUID; that returns
/// `Characteristic<[u8]>`, and `Characteristic::phantom` is private to `trouble-host`, so there's
/// no way to retype the result as `Characteristic<Ase>` from outside it. Decode with
/// [`Ase::from_gatt`] after reading.
pub struct AscsClient {
    handle: ServiceHandle,
    pub ase_control_point: Characteristic<AseControlPointOperation>,
    ases: Vec<Characteristic<[u8]>, MAX_ASCS_CHARACTERISTICS>,
}

impl AscsClient {
    /// Discovers the ASCS service and its characteristics on an already-connected `GattClient`.
    /// Takes `&GattClient` (not `&mut`) since discovery only needs shared access - letting a
    /// caller run this concurrently with `client.task()` (itself `&self`) via `select`/`join`
    /// rather than needing them sequenced.
    pub async fn new<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Result<Self, DiscoveryError<T::Error>> {
        let services = client.services_by_uuid(&Uuid::from(service::AUDIO_STREAM_CONTROL)).await?;
        let handle = services.first().ok_or(DiscoveryError::ServiceNotFound)?;

        let ase_control_point = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::ASE_CONTROL_POINT))
            .await
            .map_err(|_| DiscoveryError::CharacteristicNotFound)?;

        let ases: Vec<Characteristic<[u8]>, MAX_ASCS_CHARACTERISTICS> = client.characteristics(handle).await?;

        // At least one Sink or Source ASE is mandatory.
        if !ases
            .iter()
            .any(|c| c.uuid == Uuid::from(characteristic::SINK_ASE) || c.uuid == Uuid::from(characteristic::SOURCE_ASE))
        {
            return Err(DiscoveryError::CharacteristicNotFound);
        }

        Ok(Self {
            handle: handle.clone(),
            ase_control_point,
            ases,
        })
    }

    /// This server's Sink ASE characteristics (one per channel for a stereo sink).
    pub fn sink_ases(&self) -> impl Iterator<Item = &Characteristic<[u8]>> {
        self.ases.iter().filter(|c| c.uuid == Uuid::from(characteristic::SINK_ASE))
    }

    /// This server's Source ASE characteristics (one per channel for a stereo source).
    pub fn source_ases(&self) -> impl Iterator<Item = &Characteristic<[u8]>> {
        self.ases.iter().filter(|c| c.uuid == Uuid::from(characteristic::SOURCE_ASE))
    }
}

/// Which endpoint direction an ASE slot was created for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AseDirection {
    Source,
    Sink,
}

/// Backing-store size that fits any spec-legal ASE characteristic value: ASE_ID + ASE_State (2)
/// + CodecConfigured's fixed parameters (25) + a maximal Codec_Specific_Configuration (its length
/// field is one octet, so 255).
pub const ASE_STORE_SIZE: usize = 282;

/// Backing-store size that fits any ASE Control Point write: ATT caps a characteristic value at
/// 512 octets, and a maximal multi-ASE Config Codec operation would exceed that - so the ATT cap
/// is the bound. A device only expecting small LC3 configs can pass a smaller store; oversized
/// writes are then rejected at the ATT layer.
pub const ASE_CONTROL_POINT_STORE_SIZE: usize = 512;

/// A Gatt service for controlling unicast audio streams.
///
/// `MAX_ASES` is the max number of sink ASEs plus source ASEs the device supports for its one
/// active connection.
pub struct AscsServer<const MAX_ASES: usize> {
    handle: u16,
    ase_control_point: Characteristic<AseControlPointOperation>,
    ases: Vec<(AseDirection, Characteristic<Ase>), MAX_ASES>,
}

impl<const MAX_ASES: usize> AscsServer<MAX_ASES> {
    /// Create a new Ascs Gatt Service
    ///
    /// `ases` describes the initial (Idle) ASE endpoints to expose, tagged by direction.
    /// `ase_stores` must yield one backing buffer per entry in `ases` (see [`ASE_STORE_SIZE`]/
    /// [`ASE_CONTROL_POINT_STORE_SIZE`] for sizing).
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        ases: Vec<AseType, MAX_ASES>,
        control_point_store: &'a mut [u8],
        ase_stores: impl IntoIterator<Item = &'a mut [u8]>,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::AUDIO_STREAM_CONTROL));

        // The Common Audio Profile requires an encrypted link for the ASE Control Point and
        // every ASE characteristic.
        let ase_control_point_char = service
            .add_characteristic(
                characteristic::ASE_CONTROL_POINT,
                &[
                    CharacteristicProp::Write,
                    CharacteristicProp::WriteWithoutResponse,
                    CharacteristicProp::Notify,
                ],
                AseControlPointOperation::default(),
                control_point_store,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let mut ase_stores = ase_stores.into_iter();
        let mut ase_chars = Vec::new();
        for ase in ases.iter() {
            let (direction, uuid, value) = match ase {
                AseType::Source(ase) => (AseDirection::Source, characteristic::SOURCE_ASE, ase.clone()),
                AseType::Sink(ase) => (AseDirection::Sink, characteristic::SINK_ASE, ase.clone()),
            };
            let store = ase_stores.next().expect("one ASE store per ASE endpoint required");
            let characteristic = service
                .add_characteristic(uuid, &[CharacteristicProp::Read, CharacteristicProp::Notify], value, store)
                .read_permission(PermissionLevel::EncryptionRequired)
                .build();
            ase_chars
                .push((direction, characteristic))
                .map_err(|_| "Adding ASE endpoint exceeded MAX_ASES")
                .unwrap();
        }

        Self {
            handle: service.build(),
            ase_control_point: ase_control_point_char,
            ases: ase_chars,
        }
    }

    /// The configured ASE characteristics, each tagged with the direction it was created for.
    pub fn ases(&self) -> &[(AseDirection, Characteristic<Ase>)] {
        &self.ases
    }

    /// The ASE Control Point characteristic.
    pub fn ase_control_point(&self) -> &Characteristic<AseControlPointOperation> {
        &self.ase_control_point
    }
}

impl<const MAX_ASES: usize, P: PacketPool> LeAudioServerService<P> for AscsServer<MAX_ASES> {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.ase_control_point.handle {
            return Some(Err(AttErrorCode::READ_NOT_PERMITTED));
        }
        for (_, ase) in self.ases.iter() {
            if event.handle() == ase.handle {
                return Some(Ok(()));
            }
        }

        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.ase_control_point.handle {
            return Some(match event.value(&self.ase_control_point) {
                Ok(_) => Ok(()),
                Err(_) => {
                    #[cfg(feature = "defmt")]
                    defmt::warn!("[ascs] malformed ASE Control Point write");
                    Err(AttErrorCode::WRITE_REQUEST_REJECTED)
                }
            });
        }
        for (_, ase) in self.ases.iter() {
            if event.handle() == ase.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
        }

        None
    }
}

/// An Audio Stream Endpoint: identity plus current state. This is the value of the
/// Sink_ASE/Source_ASE characteristics. Wire format: ASE_ID (1) | ASE_State (1) |
/// Additional_ASE_Parameters (variable, depends on ASE_State - see [`AseState`]).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct Ase {
    encoded: AVec<u8>,
}

impl Ase {
    /// Creates a new ASE in the `Idle` state.
    pub fn new(id: u8) -> Self {
        Self::with_state(id, AseState::Idle)
    }

    /// Creates a new ASE in the given state.
    pub fn with_state(id: u8, state: AseState) -> Self {
        let mut encoded = AVec::new();
        encoded.push(id);
        state.encode(&mut encoded);
        Self { encoded }
    }

    /// This ASE's ASE_ID.
    pub fn id(&self) -> u8 {
        self.encoded[0]
    }

    /// Decodes this ASE's current state.
    pub fn state(&self) -> Result<AseState, FromGattError> {
        AseState::decode(self.encoded.get(1..).ok_or(FromGattError::InvalidLength)?)
    }

    /// Transitions this ASE to a new state.
    pub fn set_state(&mut self, state: AseState) {
        *self = Self::with_state(self.id(), state);
    }
}

impl Default for Ase {
    fn default() -> Self {
        Self::new(0)
    }
}

impl AsGatt for Ase {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for Ase {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() < 2 {
            return Err(FromGattError::InvalidLength);
        }
        let ase = Self { encoded: AVec::from(data) };
        ase.state()?;
        Ok(ase)
    }
}

/// Tags an [`Ase`]'s intended direction when building the service. Not itself a GATT value type:
/// the wire format for a Sink_ASE and Source_ASE characteristic is identical (see [`Ase`]); only
/// which characteristic UUID it's exposed under differs.
#[derive(Clone)]
pub enum AseType {
    Source(Ase),
    Sink(Ase),
}

/// State of the ASE with respect to the ASE state machine (Bluetooth ASCS 3.2 "ASE_State" plus
/// per-state Additional_ASE_Parameters). Each state's extra parameters live directly on that
/// variant rather than in a separately-named struct, since the spec doesn't name them as their
/// own structure - only the state values themselves are spec-defined.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Clone)]
pub enum AseState {
    Idle,
    CodecConfigured {
        framing: u8,
        preferred_phy: PhySet,
        preferred_retransmission_number: u8,
        /// Milliseconds.
        max_transport_latency: u16,
        /// Microseconds, 24-bit.
        presentation_delay_min: [u8; 3],
        /// Microseconds, 24-bit.
        presentation_delay_max: [u8; 3],
        /// Microseconds, 24-bit.
        preferred_presentation_delay_min: [u8; 3],
        /// Microseconds, 24-bit.
        preferred_presentation_delay_max: [u8; 3],
        codec_id: CodecId,
        codec_specific_configuration: AVec<u8>,
    },
    QosConfigured {
        cig_id: u8,
        cis_id: u8,
        /// Microseconds, 24-bit.
        sdu_interval: [u8; 3],
        framing: u8,
        phy: PhySet,
        max_sdu: u16,
        retransmission_number: u8,
        max_transport_latency: u16,
        /// Microseconds, 24-bit.
        presentation_delay: [u8; 3],
    },
    Enabling {
        cig_id: u8,
        cis_id: u8,
        metadata: AVec<u8>,
    },
    Streaming {
        cig_id: u8,
        cis_id: u8,
        metadata: AVec<u8>,
    },
    Disabling {
        cig_id: u8,
        cis_id: u8,
        metadata: AVec<u8>,
    },
    Releasing,
}

// `PhySet` (from trouble-host) doesn't implement `Debug`, so this can't be derived.
impl core::fmt::Debug for AseState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Idle => write!(f, "Idle"),
            Self::CodecConfigured {
                framing,
                preferred_phy,
                preferred_retransmission_number,
                max_transport_latency,
                presentation_delay_min,
                presentation_delay_max,
                preferred_presentation_delay_min,
                preferred_presentation_delay_max,
                codec_id,
                codec_specific_configuration,
            } => f
                .debug_struct("CodecConfigured")
                .field("framing", framing)
                .field("preferred_phy", &(*preferred_phy as u8))
                .field("preferred_retransmission_number", preferred_retransmission_number)
                .field("max_transport_latency", max_transport_latency)
                .field("presentation_delay_min", presentation_delay_min)
                .field("presentation_delay_max", presentation_delay_max)
                .field("preferred_presentation_delay_min", preferred_presentation_delay_min)
                .field("preferred_presentation_delay_max", preferred_presentation_delay_max)
                .field("codec_id", codec_id)
                .field("codec_specific_configuration", codec_specific_configuration)
                .finish(),
            Self::QosConfigured {
                cig_id,
                cis_id,
                sdu_interval,
                framing,
                phy,
                max_sdu,
                retransmission_number,
                max_transport_latency,
                presentation_delay,
            } => f
                .debug_struct("QosConfigured")
                .field("cig_id", cig_id)
                .field("cis_id", cis_id)
                .field("sdu_interval", sdu_interval)
                .field("framing", framing)
                .field("phy", &(*phy as u8))
                .field("max_sdu", max_sdu)
                .field("retransmission_number", retransmission_number)
                .field("max_transport_latency", max_transport_latency)
                .field("presentation_delay", presentation_delay)
                .finish(),
            Self::Enabling { cig_id, cis_id, metadata } => f
                .debug_struct("Enabling")
                .field("cig_id", cig_id)
                .field("cis_id", cis_id)
                .field("metadata", metadata)
                .finish(),
            Self::Streaming { cig_id, cis_id, metadata } => f
                .debug_struct("Streaming")
                .field("cig_id", cig_id)
                .field("cis_id", cis_id)
                .field("metadata", metadata)
                .finish(),
            Self::Disabling { cig_id, cis_id, metadata } => f
                .debug_struct("Disabling")
                .field("cig_id", cig_id)
                .field("cis_id", cis_id)
                .field("metadata", metadata)
                .finish(),
            Self::Releasing => write!(f, "Releasing"),
        }
    }
}

impl Default for AseState {
    fn default() -> Self {
        Self::Idle
    }
}

impl AseState {
    fn ase_state_byte(&self) -> u8 {
        match self {
            Self::Idle => 0x00,
            Self::CodecConfigured { .. } => 0x01,
            Self::QosConfigured { .. } => 0x02,
            Self::Enabling { .. } => 0x03,
            Self::Streaming { .. } => 0x04,
            Self::Disabling { .. } => 0x05,
            Self::Releasing => 0x06,
        }
    }

    fn encode(&self, out: &mut AVec<u8>) {
        out.push(self.ase_state_byte());
        match self {
            Self::Idle | Self::Releasing => {}
            Self::CodecConfigured {
                framing,
                preferred_phy,
                preferred_retransmission_number,
                max_transport_latency,
                presentation_delay_min,
                presentation_delay_max,
                preferred_presentation_delay_min,
                preferred_presentation_delay_max,
                codec_id,
                codec_specific_configuration,
            } => {
                out.push(*framing);
                out.push(*preferred_phy as u8);
                out.push(*preferred_retransmission_number);
                out.extend_from_slice(&max_transport_latency.to_le_bytes());
                out.extend_from_slice(presentation_delay_min);
                out.extend_from_slice(presentation_delay_max);
                out.extend_from_slice(preferred_presentation_delay_min);
                out.extend_from_slice(preferred_presentation_delay_max);
                out.extend_from_slice(&codec_id.to_le_bytes());
                out.push(codec_specific_configuration.len() as u8);
                out.extend_from_slice(codec_specific_configuration);
            }
            Self::QosConfigured {
                cig_id,
                cis_id,
                sdu_interval,
                framing,
                phy,
                max_sdu,
                retransmission_number,
                max_transport_latency,
                presentation_delay,
            } => {
                out.push(*cig_id);
                out.push(*cis_id);
                out.extend_from_slice(sdu_interval);
                out.push(*framing);
                out.push(*phy as u8);
                out.extend_from_slice(&max_sdu.to_le_bytes());
                out.push(*retransmission_number);
                out.extend_from_slice(&max_transport_latency.to_le_bytes());
                out.extend_from_slice(presentation_delay);
            }
            Self::Enabling { cig_id, cis_id, metadata }
            | Self::Streaming { cig_id, cis_id, metadata }
            | Self::Disabling { cig_id, cis_id, metadata } => {
                out.push(*cig_id);
                out.push(*cis_id);
                out.push(metadata.len() as u8);
                out.extend_from_slice(metadata);
            }
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let state_byte = *data.first().ok_or(FromGattError::InvalidLength)?;
        let rest = data.get(1..).ok_or(FromGattError::InvalidLength)?;
        match state_byte {
            0x00 => Ok(Self::Idle),
            0x01 => {
                if rest.len() < 25 {
                    return Err(FromGattError::InvalidLength);
                }
                let codec_specific_configuration_length = rest[24] as usize;
                let config_end = 25usize
                    .checked_add(codec_specific_configuration_length)
                    .ok_or(FromGattError::InvalidLength)?;
                let codec_specific_configuration =
                    AVec::from(rest.get(25..config_end).ok_or(FromGattError::InvalidLength)?);
                Ok(Self::CodecConfigured {
                    framing: rest[0],
                    preferred_phy: phy_set_from_u8(rest[1])?,
                    preferred_retransmission_number: rest[2],
                    max_transport_latency: u16::from_le_bytes([rest[3], rest[4]]),
                    presentation_delay_min: [rest[5], rest[6], rest[7]],
                    presentation_delay_max: [rest[8], rest[9], rest[10]],
                    preferred_presentation_delay_min: [rest[11], rest[12], rest[13]],
                    preferred_presentation_delay_max: [rest[14], rest[15], rest[16]],
                    codec_id: CodecId::from_le_bytes([rest[17], rest[18], rest[19], rest[20], rest[21]]),
                    codec_specific_configuration,
                })
            }
            0x02 => {
                if rest.len() != 15 {
                    return Err(FromGattError::InvalidLength);
                }
                Ok(Self::QosConfigured {
                    cig_id: rest[0],
                    cis_id: rest[1],
                    sdu_interval: [rest[2], rest[3], rest[4]],
                    framing: rest[5],
                    phy: phy_set_from_u8(rest[6])?,
                    max_sdu: u16::from_le_bytes([rest[7], rest[8]]),
                    retransmission_number: rest[9],
                    max_transport_latency: u16::from_le_bytes([rest[10], rest[11]]),
                    presentation_delay: [rest[12], rest[13], rest[14]],
                })
            }
            0x03 | 0x04 | 0x05 => {
                if rest.len() < 3 {
                    return Err(FromGattError::InvalidLength);
                }
                let metadata_length = rest[2] as usize;
                let metadata_end = 3usize.checked_add(metadata_length).ok_or(FromGattError::InvalidLength)?;
                let metadata = AVec::from(rest.get(3..metadata_end).ok_or(FromGattError::InvalidLength)?);
                let cig_id = rest[0];
                let cis_id = rest[1];
                Ok(match state_byte {
                    0x03 => Self::Enabling { cig_id, cis_id, metadata },
                    0x04 => Self::Streaming { cig_id, cis_id, metadata },
                    _ => Self::Disabling { cig_id, cis_id, metadata },
                })
            }
            0x06 => Ok(Self::Releasing),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// Target_Latency: the client's preference between latency and reliability.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TargetLatency {
    Lower = 0x01,
    BalancedLatencyAndReliability = 0x02,
    HigherReliability = 0x03,
}

impl TryFrom<u8> for TargetLatency {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Lower),
            0x02 => Ok(Self::BalancedLatencyAndReliability),
            0x03 => Ok(Self::HigherReliability),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// One ASE_ID plus its Config Codec operand, as written in a Config Codec operation.
pub type ConfigCodecEntry = (u8, TargetLatency, PhySet, CodecId, AVec<u8>);
/// One ASE_ID plus its Config QoS operand, as written in a Config QoS operation. Fields:
/// (ase_id, cig_id, cis_id, sdu_interval, framing, phy, max_sdu, retransmission_number,
/// max_transport_latency, presentation_delay).
pub type ConfigQosEntry = (u8, u8, u8, [u8; 3], u8, PhySet, u16, u8, u16, [u8; 3]);
/// One ASE_ID plus Metadata, as written in an Enable or Update Metadata operation.
pub type AseMetadataEntry = (u8, AVec<u8>);

/// The value written to the ASE Control Point characteristic: an Opcode followed by
/// Number_of_ASEs and that many per-ASE operands, per Bluetooth ASCS 5. Stored pre-encoded
/// (like [`Ase`]/`PAC`) since the value is variable-length.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct AseControlPointOperation {
    encoded: AVec<u8>,
}

/// A decoded ASE Control Point operation. Obtain via [`AseControlPointOperation::operation`] -
/// decode once and pass this around, rather than re-decoding per accessor.
#[derive(Clone)]
pub enum Operation {
    ConfigCodec(AVec<ConfigCodecEntry>),
    ConfigQos(AVec<ConfigQosEntry>),
    Enable(AVec<AseMetadataEntry>),
    ReceiverStartReady(AVec<u8>),
    Disable(AVec<u8>),
    ReceiverStopReady(AVec<u8>),
    UpdateMetadata(AVec<AseMetadataEntry>),
    Release(AVec<u8>),
}

impl Operation {
    /// This operation's Opcode (Bluetooth ASCS 5, Table 5.1).
    pub fn opcode(&self) -> u8 {
        match self {
            Self::ConfigCodec(_) => 0x01,
            Self::ConfigQos(_) => 0x02,
            Self::Enable(_) => 0x03,
            Self::ReceiverStartReady(_) => 0x04,
            Self::Disable(_) => 0x05,
            Self::ReceiverStopReady(_) => 0x06,
            Self::UpdateMetadata(_) => 0x07,
            Self::Release(_) => 0x08,
        }
    }

    /// The ASE_IDs this operation applies to.
    pub fn ase_ids(&self) -> AVec<u8> {
        match self {
            Self::ConfigCodec(v) => v.iter().map(|e| e.0).collect(),
            Self::ConfigQos(v) => v.iter().map(|e| e.0).collect(),
            Self::Enable(v) | Self::UpdateMetadata(v) => v.iter().map(|e| e.0).collect(),
            Self::ReceiverStartReady(v) | Self::Disable(v) | Self::ReceiverStopReady(v) | Self::Release(v) => {
                v.clone()
            }
        }
    }

    fn ase_count(&self) -> usize {
        match self {
            Self::ConfigCodec(v) => v.len(),
            Self::ConfigQos(v) => v.len(),
            Self::Enable(v) | Self::UpdateMetadata(v) => v.len(),
            Self::ReceiverStartReady(v) | Self::Disable(v) | Self::ReceiverStopReady(v) | Self::Release(v) => v.len(),
        }
    }

    fn encode(&self, out: &mut AVec<u8>) {
        out.push(self.opcode());
        out.push(self.ase_count() as u8);
        match self {
            Self::ConfigCodec(entries) => {
                for (ase_id, target_latency, target_phy, codec_id, config) in entries {
                    out.push(*ase_id);
                    out.push(*target_latency as u8);
                    out.push(*target_phy as u8);
                    out.extend_from_slice(&codec_id.to_le_bytes());
                    out.push(config.len() as u8);
                    out.extend_from_slice(config);
                }
            }
            Self::ConfigQos(entries) => {
                for (ase_id, cig_id, cis_id, sdu_interval, framing, phy, max_sdu, rtn, max_latency, delay) in entries {
                    out.push(*ase_id);
                    out.push(*cig_id);
                    out.push(*cis_id);
                    out.extend_from_slice(sdu_interval);
                    out.push(*framing);
                    out.push(*phy as u8);
                    out.extend_from_slice(&max_sdu.to_le_bytes());
                    out.push(*rtn);
                    out.extend_from_slice(&max_latency.to_le_bytes());
                    out.extend_from_slice(delay);
                }
            }
            Self::Enable(entries) | Self::UpdateMetadata(entries) => {
                for (ase_id, metadata) in entries {
                    out.push(*ase_id);
                    out.push(metadata.len() as u8);
                    out.extend_from_slice(metadata);
                }
            }
            Self::ReceiverStartReady(ids) | Self::Disable(ids) | Self::ReceiverStopReady(ids) | Self::Release(ids) => {
                out.extend_from_slice(ids);
            }
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() < 2 {
            return Err(FromGattError::InvalidLength);
        }
        let opcode = data[0];
        let count = data[1] as usize;
        let mut pos = 2usize;

        match opcode {
            0x01 => {
                let mut entries = AVec::with_capacity(count);
                for _ in 0..count {
                    let entry = data.get(pos..pos + 8).ok_or(FromGattError::InvalidLength)?;
                    let ase_id = entry[0];
                    let target_latency = TargetLatency::try_from(entry[1])?;
                    let target_phy = phy_set_from_u8(entry[2])?;
                    let codec_id = CodecId::from_le_bytes(entry[3..8].try_into().unwrap());
                    let config_len = *data.get(pos + 8).ok_or(FromGattError::InvalidLength)? as usize;
                    pos += 9;
                    let config_end = pos.checked_add(config_len).ok_or(FromGattError::InvalidLength)?;
                    let config = AVec::from(data.get(pos..config_end).ok_or(FromGattError::InvalidLength)?);
                    pos = config_end;
                    entries.push((ase_id, target_latency, target_phy, codec_id, config));
                }
                Ok(Self::ConfigCodec(entries))
            }
            0x02 => {
                let mut entries = AVec::with_capacity(count);
                for _ in 0..count {
                    let entry = data.get(pos..pos + 16).ok_or(FromGattError::InvalidLength)?;
                    pos += 16;
                    entries.push((
                        entry[0],                                 // ase_id
                        entry[1],                                 // cig_id
                        entry[2],                                 // cis_id
                        [entry[3], entry[4], entry[5]],           // sdu_interval
                        entry[6],                                 // framing
                        phy_set_from_u8(entry[7])?,                // phy
                        u16::from_le_bytes([entry[8], entry[9]]), // max_sdu
                        entry[10],                                // retransmission_number
                        u16::from_le_bytes([entry[11], entry[12]]), // max_transport_latency
                        [entry[13], entry[14], entry[15]],        // presentation_delay
                    ));
                }
                Ok(Self::ConfigQos(entries))
            }
            0x03 | 0x07 => {
                let mut entries = AVec::with_capacity(count);
                for _ in 0..count {
                    let ase_id = *data.get(pos).ok_or(FromGattError::InvalidLength)?;
                    let metadata_len = *data.get(pos + 1).ok_or(FromGattError::InvalidLength)? as usize;
                    pos += 2;
                    let metadata_end = pos.checked_add(metadata_len).ok_or(FromGattError::InvalidLength)?;
                    let metadata = AVec::from(data.get(pos..metadata_end).ok_or(FromGattError::InvalidLength)?);
                    pos = metadata_end;
                    entries.push((ase_id, metadata));
                }
                if opcode == 0x03 {
                    Ok(Self::Enable(entries))
                } else {
                    Ok(Self::UpdateMetadata(entries))
                }
            }
            0x04 | 0x05 | 0x06 | 0x08 => {
                let ids = AVec::from(data.get(pos..pos + count).ok_or(FromGattError::InvalidLength)?);
                match opcode {
                    0x04 => Ok(Self::ReceiverStartReady(ids)),
                    0x05 => Ok(Self::Disable(ids)),
                    0x06 => Ok(Self::ReceiverStopReady(ids)),
                    _ => Ok(Self::Release(ids)),
                }
            }
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AseControlPointOperation {
    /// Builds a Config Codec operation.
    pub fn config_codec(entries: AVec<ConfigCodecEntry>) -> Self {
        Self::from_operation(&Operation::ConfigCodec(entries))
    }
    /// Builds a Config QoS operation.
    pub fn config_qos(entries: AVec<ConfigQosEntry>) -> Self {
        Self::from_operation(&Operation::ConfigQos(entries))
    }
    /// Builds an Enable operation.
    pub fn enable(entries: AVec<AseMetadataEntry>) -> Self {
        Self::from_operation(&Operation::Enable(entries))
    }
    /// Builds a Receiver Start Ready operation.
    pub fn receiver_start_ready(ase_ids: AVec<u8>) -> Self {
        Self::from_operation(&Operation::ReceiverStartReady(ase_ids))
    }
    /// Builds a Disable operation.
    pub fn disable(ase_ids: AVec<u8>) -> Self {
        Self::from_operation(&Operation::Disable(ase_ids))
    }
    /// Builds a Receiver Stop Ready operation.
    pub fn receiver_stop_ready(ase_ids: AVec<u8>) -> Self {
        Self::from_operation(&Operation::ReceiverStopReady(ase_ids))
    }
    /// Builds an Update Metadata operation.
    pub fn update_metadata(entries: AVec<AseMetadataEntry>) -> Self {
        Self::from_operation(&Operation::UpdateMetadata(entries))
    }
    /// Builds a Release operation.
    pub fn release(ase_ids: AVec<u8>) -> Self {
        Self::from_operation(&Operation::Release(ase_ids))
    }

    fn from_operation(op: &Operation) -> Self {
        let mut encoded = AVec::new();
        op.encode(&mut encoded);
        Self { encoded }
    }

    /// This operation's Opcode.
    pub fn opcode(&self) -> u8 {
        self.encoded.first().copied().unwrap_or(0)
    }

    /// Decodes this write into an [`Operation`]. Decode once and pass the result to everything
    /// that needs it ([`crate::cis::CisManager::observe_operation`],
    /// [`crate::bap::drive_ase_control_point`]) - each entry owns copies of its variable-length
    /// operands, so repeated calls repeat that work.
    pub fn operation(&self) -> Result<Operation, FromGattError> {
        Operation::decode(&self.encoded)
    }
}

impl Default for AseControlPointOperation {
    fn default() -> Self {
        Self::release(AVec::new())
    }
}

impl AsGatt for AseControlPointOperation {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for AseControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        // Validate eagerly so a malformed write is rejected up front.
        Operation::decode(data)?;
        Ok(Self { encoded: AVec::from(data) })
    }
}

/// The ASE Control Point notification sent back to the client after processing an operation:
/// Opcode, Number_of_ASEs, then per ASE an ASE_ID/Response_Code/Reason triple.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct AseControlPointNotification {
    encoded: AVec<u8>,
}

impl AseControlPointNotification {
    /// `results` is (ase_id, response_code, reason) - see Bluetooth ASCS 5 Table 5.2/5.3 for the
    /// assigned Response_Code/Reason values.
    pub fn new(opcode: u8, results: &[(u8, u8, u8)]) -> Self {
        let mut encoded = AVec::with_capacity(2 + results.len() * 3);
        encoded.push(opcode);
        encoded.push(results.len() as u8);
        for (ase_id, response_code, reason) in results {
            encoded.push(*ase_id);
            encoded.push(*response_code);
            encoded.push(*reason);
        }
        Self { encoded }
    }

    /// The Opcode this notification is responding to.
    pub fn opcode(&self) -> u8 {
        self.encoded.first().copied().unwrap_or(0)
    }

    /// The per-ASE (ase_id, response_code, reason) results.
    pub fn results(&self) -> impl Iterator<Item = (u8, u8, u8)> + '_ {
        self.encoded.get(2..).unwrap_or(&[]).chunks_exact(3).map(|c| (c[0], c[1], c[2]))
    }
}

impl AsGatt for AseControlPointNotification {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for AseControlPointNotification {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let count = *data.get(1).ok_or(FromGattError::InvalidLength)? as usize;
        if data.len() != 2 + count * 3 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self { encoded: AVec::from(data) })
    }
}
