//! ## Broadcast Audio Scan Service
//!
//! The Broadcast Audio Scan Service (BASS) lets a Broadcast Assistant (a client that's already
//! scanning for broadcast audio sources) tell this device (a Scan Delegator - e.g. a pair of
//! earbuds that can't scan and receive at the same time) about a broadcast source to synchronize
//! to, without the delegator needing to scan for it itself. Bluetooth BASS v1.0.
//!
//! Opcode numbering and application error codes are reconstructed from public documentation, not
//! cross-checked against the official Bluetooth SIG assigned-numbers PDF - verify before relying
//! on the exact values for certification.
//!
//! Simplification: a real, unused Broadcast Receive State source slot is encoded by the spec as a
//! single all-zero-length octet; this crate always encodes the full fixed/variable structure
//! (with [`BroadcastReceiveState::UNUSED_SOURCE_ID`] marking an empty slot), which is simpler to
//! reason about but not what a real central will see on the wire for an empty slot.

use alloc::vec::Vec as AVec;

use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec as HVec;
use trouble_host::{
    gatt::{GattConnection, ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::generic_audio::{decode_list, encode_list, Metadata};
use crate::{LeAudioServerService, MAX_SERVICES};

/// Application error codes (BASS). See the module doc comment's disclaimer.
pub const OPCODE_NOT_SUPPORTED: AttErrorCode = AttErrorCode::new(0x80);
pub const INVALID_SOURCE_ID: AttErrorCode = AttErrorCode::new(0x81);

/// Advertiser_Address_Type / Source_Address_Type.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AddressType {
    #[default]
    Public = 0x00,
    Random = 0x01,
}

impl TryFrom<u8> for AddressType {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Public),
            0x01 => Ok(Self::Random),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// PA_Sync (Add/Modify Source operand): the Broadcast Assistant's request for how this device
/// should synchronize to the source's Periodic Advertising train.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PaSyncParam {
    DoNotSync = 0x00,
    SyncPastAvailable = 0x01,
    SyncNoPast = 0x02,
}

impl TryFrom<u8> for PaSyncParam {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::DoNotSync),
            0x01 => Ok(Self::SyncPastAvailable),
            0x02 => Ok(Self::SyncNoPast),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// PA_Sync_State (Broadcast Receive State field): this device's actual current PA sync status.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PaSyncState {
    #[default]
    NotSynchronized = 0x00,
    SyncInfoRequest = 0x01,
    Synchronized = 0x02,
    Failed = 0x03,
    NoPast = 0x04,
}

impl TryFrom<u8> for PaSyncState {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::NotSynchronized),
            0x01 => Ok(Self::SyncInfoRequest),
            0x02 => Ok(Self::Synchronized),
            0x03 => Ok(Self::Failed),
            0x04 => Ok(Self::NoPast),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// BIG_Encryption (Broadcast Receive State field).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BigEncryption {
    #[default]
    NotEncrypted = 0x00,
    BroadcastCodeRequired = 0x01,
    Decrypting = 0x02,
    BadCode = 0x03,
}

impl TryFrom<u8> for BigEncryption {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::NotEncrypted),
            0x01 => Ok(Self::BroadcastCodeRequired),
            0x02 => Ok(Self::Decrypting),
            0x03 => Ok(Self::BadCode),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// One subgroup's BIS sync state/metadata, as carried by Add/Modify Source operands and Broadcast
/// Receive State records: BIS_Sync (4, bitmask - bit `n` means "synced to BIS index `n + 1`") |
/// Metadata_Length (1) | Metadata (variable, LTV-encoded - see [`crate::generic_audio`]).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Subgroup {
    pub bis_sync: u32,
    pub metadata: AVec<Metadata>,
}

impl Subgroup {
    fn encode(&self, out: &mut AVec<u8>) {
        out.extend_from_slice(&self.bis_sync.to_le_bytes());
        let metadata = encode_list(&self.metadata);
        out.push(metadata.len() as u8);
        out.extend_from_slice(&metadata);
    }

    fn decode(data: &[u8]) -> Result<(Self, usize), FromGattError> {
        if data.len() < 5 {
            return Err(FromGattError::InvalidLength);
        }
        let bis_sync = u32::from_le_bytes(data[..4].try_into().unwrap());
        let metadata_len = data[4] as usize;
        let end = 5usize.checked_add(metadata_len).ok_or(FromGattError::InvalidLength)?;
        let metadata = decode_list(data.get(5..end).ok_or(FromGattError::InvalidLength)?)?;
        Ok((Self { bis_sync, metadata }, end))
    }
}

fn encode_subgroups(out: &mut AVec<u8>, subgroups: &[Subgroup]) {
    out.push(subgroups.len() as u8);
    for subgroup in subgroups {
        subgroup.encode(out);
    }
}

fn decode_subgroups(data: &[u8]) -> Result<(AVec<Subgroup>, usize), FromGattError> {
    let count = *data.first().ok_or(FromGattError::InvalidLength)? as usize;
    let mut pos = 1;
    let mut subgroups = AVec::with_capacity(count);
    for _ in 0..count {
        let (subgroup, consumed) = Subgroup::decode(data.get(pos..).ok_or(FromGattError::InvalidLength)?)?;
        subgroups.push(subgroup);
        pos += consumed;
    }
    Ok((subgroups, pos))
}

/// Broadcast Audio Scan Control Point Opcodes.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
pub enum BroadcastAudioScanControlPointOpcode {
    /// The Broadcast Assistant informs this device that it has started scanning on its behalf.
    RemoteScanStarted,
    /// The Broadcast Assistant informs this device that it has stopped scanning on its behalf.
    RemoteScanStopped,
    AddSource {
        advertiser_address_type: AddressType,
        advertiser_address: [u8; 6],
        advertising_sid: u8,
        /// 24-bit.
        broadcast_id: u32,
        pa_sync: PaSyncParam,
        pa_interval: u16,
        subgroups: AVec<Subgroup>,
    },
    ModifySource {
        source_id: u8,
        pa_sync: PaSyncParam,
        pa_interval: u16,
        subgroups: AVec<Subgroup>,
    },
    RemoveSource {
        source_id: u8,
    },
}

impl BroadcastAudioScanControlPointOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::RemoteScanStarted => 0x01,
            Self::RemoteScanStopped => 0x02,
            Self::AddSource { .. } => 0x03,
            Self::ModifySource { .. } => 0x04,
            Self::RemoveSource { .. } => 0x05,
        }
    }

    fn encode(&self, out: &mut AVec<u8>) {
        out.push(self.raw_opcode());
        match self {
            Self::RemoteScanStarted | Self::RemoteScanStopped => {}
            Self::AddSource {
                advertiser_address_type,
                advertiser_address,
                advertising_sid,
                broadcast_id,
                pa_sync,
                pa_interval,
                subgroups,
            } => {
                out.push(*advertiser_address_type as u8);
                out.extend_from_slice(advertiser_address);
                out.push(*advertising_sid);
                out.extend_from_slice(&broadcast_id.to_le_bytes()[..3]);
                out.push(*pa_sync as u8);
                out.extend_from_slice(&pa_interval.to_le_bytes());
                encode_subgroups(out, subgroups);
            }
            Self::ModifySource { source_id, pa_sync, pa_interval, subgroups } => {
                out.push(*source_id);
                out.push(*pa_sync as u8);
                out.extend_from_slice(&pa_interval.to_le_bytes());
                encode_subgroups(out, subgroups);
            }
            Self::RemoveSource { source_id } => out.push(*source_id),
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = *data.first().ok_or(FromGattError::InvalidLength)?;
        let rest = &data[1..];
        Ok(match opcode {
            0x01 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::RemoteScanStarted
            }
            0x02 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::RemoteScanStopped
            }
            0x03 => {
                if rest.len() < 11 {
                    return Err(FromGattError::InvalidLength);
                }
                let advertiser_address_type = AddressType::try_from(rest[0])?;
                let advertiser_address: [u8; 6] = rest[1..7].try_into().unwrap();
                let advertising_sid = rest[7];
                let broadcast_id = u32::from_le_bytes([rest[8], rest[9], rest[10], 0]);
                let pa_sync = PaSyncParam::try_from(*rest.get(11).ok_or(FromGattError::InvalidLength)?)?;
                let pa_interval = u16::from_le_bytes([
                    *rest.get(12).ok_or(FromGattError::InvalidLength)?,
                    *rest.get(13).ok_or(FromGattError::InvalidLength)?,
                ]);
                let (subgroups, _) = decode_subgroups(rest.get(14..).ok_or(FromGattError::InvalidLength)?)?;
                Self::AddSource {
                    advertiser_address_type,
                    advertiser_address,
                    advertising_sid,
                    broadcast_id,
                    pa_sync,
                    pa_interval,
                    subgroups,
                }
            }
            0x04 => {
                if rest.len() < 4 {
                    return Err(FromGattError::InvalidLength);
                }
                let source_id = rest[0];
                let pa_sync = PaSyncParam::try_from(rest[1])?;
                let pa_interval = u16::from_le_bytes([rest[2], rest[3]]);
                let (subgroups, _) = decode_subgroups(rest.get(4..).ok_or(FromGattError::InvalidLength)?)?;
                Self::ModifySource { source_id, pa_sync, pa_interval, subgroups }
            }
            0x05 => {
                if rest.len() != 1 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::RemoveSource { source_id: rest[0] }
            }
            _ => return Err(FromGattError::InvalidLength),
        })
    }
}

/// The value written to the Broadcast Audio Scan Control Point characteristic. Wire bytes are
/// cached in `encoded` for the same reason as `vcs::VolumeControlPointOperation`.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
pub struct BroadcastAudioScanControlPointOperation {
    pub opcode: BroadcastAudioScanControlPointOpcode,
    encoded: AVec<u8>,
}

impl BroadcastAudioScanControlPointOperation {
    pub fn new(opcode: BroadcastAudioScanControlPointOpcode) -> Self {
        let mut encoded = AVec::new();
        opcode.encode(&mut encoded);
        Self { opcode, encoded }
    }
}

impl AsGatt for BroadcastAudioScanControlPointOperation {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for BroadcastAudioScanControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = BroadcastAudioScanControlPointOpcode::decode(data)?;
        Ok(Self::new(opcode))
    }
}

impl Default for BroadcastAudioScanControlPointOperation {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(BroadcastAudioScanControlPointOpcode::RemoteScanStopped)
    }
}

/// One Broadcast Receive State source slot. `source_id ==` [`Self::UNUSED_SOURCE_ID`] marks an
/// empty slot - see the module doc comment's note on how this differs from the real spec's wire
/// encoding for that case.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
pub struct BroadcastReceiveState {
    pub source_id: u8,
    pub source_address_type: AddressType,
    pub source_address: [u8; 6],
    pub source_adv_sid: u8,
    /// 24-bit.
    pub broadcast_id: u32,
    pub pa_sync_state: PaSyncState,
    pub big_encryption: BigEncryption,
    /// Only meaningful (and, on the real wire, only present) when `big_encryption` is
    /// [`BigEncryption::BadCode`].
    pub bad_code: Option<[u8; 16]>,
    pub subgroups: AVec<Subgroup>,
}

impl BroadcastReceiveState {
    pub const UNUSED_SOURCE_ID: u8 = 0xFF;

    pub fn empty() -> Self {
        Self {
            source_id: Self::UNUSED_SOURCE_ID,
            source_address_type: AddressType::Public,
            source_address: [0; 6],
            source_adv_sid: 0,
            broadcast_id: 0,
            pa_sync_state: PaSyncState::NotSynchronized,
            big_encryption: BigEncryption::NotEncrypted,
            bad_code: None,
            subgroups: AVec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.source_id == Self::UNUSED_SOURCE_ID
    }
}

/// Value wrapper implementing `AsGatt`/`FromGatt` for [`BroadcastReceiveState`] - pre-encoded like
/// `PAC`/`Ase` in pacs.rs/ascs.rs, since the value is variable-length.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct BroadcastReceiveStateValue {
    encoded: AVec<u8>,
}

impl BroadcastReceiveStateValue {
    pub fn new(state: &BroadcastReceiveState) -> Self {
        let mut encoded = AVec::new();
        encoded.push(state.source_id);
        encoded.push(state.source_address_type as u8);
        encoded.extend_from_slice(&state.source_address);
        encoded.push(state.source_adv_sid);
        encoded.extend_from_slice(&state.broadcast_id.to_le_bytes()[..3]);
        encoded.push(state.pa_sync_state as u8);
        encoded.push(state.big_encryption as u8);
        if let Some(bad_code) = state.bad_code {
            encoded.extend_from_slice(&bad_code);
        }
        encode_subgroups(&mut encoded, &state.subgroups);
        Self { encoded }
    }

    pub fn decode(&self) -> Result<BroadcastReceiveState, FromGattError> {
        let data = &self.encoded;
        if data.len() < 13 {
            return Err(FromGattError::InvalidLength);
        }
        let source_id = data[0];
        let source_address_type = AddressType::try_from(data[1])?;
        let source_address: [u8; 6] = data[2..8].try_into().unwrap();
        let source_adv_sid = data[8];
        let broadcast_id = u32::from_le_bytes([data[9], data[10], data[11], 0]);
        let pa_sync_state = PaSyncState::try_from(data[12])?;
        let big_encryption = BigEncryption::try_from(*data.get(13).ok_or(FromGattError::InvalidLength)?)?;
        let mut pos = 14;
        let bad_code = if big_encryption == BigEncryption::BadCode {
            let code: [u8; 16] = data.get(pos..pos + 16).ok_or(FromGattError::InvalidLength)?.try_into().unwrap();
            pos += 16;
            Some(code)
        } else {
            None
        };
        let (subgroups, _) = decode_subgroups(data.get(pos..).ok_or(FromGattError::InvalidLength)?)?;
        Ok(BroadcastReceiveState {
            source_id,
            source_address_type,
            source_address,
            source_adv_sid,
            broadcast_id,
            pa_sync_state,
            big_encryption,
            bad_code,
            subgroups,
        })
    }
}

impl AsGatt for BroadcastReceiveStateValue {
    const MIN_SIZE: usize = 13;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for BroadcastReceiveStateValue {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let value = Self { encoded: AVec::from(data) };
        value.decode()?; // Validate eagerly, as PAC/Ase do.
        Ok(value)
    }
}

/// Finds the first source slot with a matching `source_id`.
fn find(slots: &[BroadcastReceiveState], source_id: u8) -> Option<usize> {
    slots.iter().position(|s| s.source_id == source_id)
}

/// Applies an Add Source operation: allocates the first unused slot for the new source, assigning
/// it a fresh Source_ID (one past the highest currently in use, matching this crate's
/// single-connection simplification elsewhere of favoring a simple, always-forward-progressing
/// rule over exactly replaying real Broadcast Assistant behavior). Returns
/// [`AttErrorCode::INSUFFICIENT_RESOURCES`] if every slot is in use.
pub fn apply_add_source(
    slots: &mut [BroadcastReceiveState],
    advertiser_address_type: AddressType,
    advertiser_address: [u8; 6],
    advertising_sid: u8,
    broadcast_id: u32,
    subgroups: AVec<Subgroup>,
) -> Result<u8, AttErrorCode> {
    let next_source_id = slots.iter().filter(|s| !s.is_empty()).map(|s| s.source_id).max().map_or(0, |id| id.wrapping_add(1));
    let slot = slots.iter_mut().find(|s| s.is_empty()).ok_or(AttErrorCode::INSUFFICIENT_RESOURCES)?;
    *slot = BroadcastReceiveState {
        source_id: next_source_id,
        source_address_type: advertiser_address_type,
        source_address: advertiser_address,
        source_adv_sid: advertising_sid,
        broadcast_id,
        pa_sync_state: PaSyncState::NotSynchronized,
        big_encryption: BigEncryption::NotEncrypted,
        bad_code: None,
        subgroups,
    };
    Ok(next_source_id)
}

/// Applies a Modify Source operation: updates the subgroups of the slot with the given
/// `source_id`.
pub fn apply_modify_source(slots: &mut [BroadcastReceiveState], source_id: u8, subgroups: AVec<Subgroup>) -> Result<(), AttErrorCode> {
    let position = find(slots, source_id).ok_or(INVALID_SOURCE_ID)?;
    slots[position].subgroups = subgroups;
    Ok(())
}

/// Applies a Remove Source operation: frees the slot with the given `source_id`.
pub fn apply_remove_source(slots: &mut [BroadcastReceiveState], source_id: u8) -> Result<(), AttErrorCode> {
    let position = find(slots, source_id).ok_or(INVALID_SOURCE_ID)?;
    slots[position] = BroadcastReceiveState::empty();
    Ok(())
}

/// A Gatt service client for discovering/controlling a Scan Delegator's broadcast sources.
pub struct BassClient {
    handle: ServiceHandle,
    pub control_point: Characteristic<BroadcastAudioScanControlPointOperation>,
    pub receive_states: HVec<Characteristic<BroadcastReceiveStateValue>, 8>,
}

impl BassClient {
    /// Discovers the BASS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose BASS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::BROADCAST_AUDIO_SCAN)).await.ok()?;
        let handle = services.first()?;

        let control_point = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::BROADCAST_AUDIO_SCAN_CONTROL_POINT))
            .await
            .ok()?;

        let mut receive_states = HVec::new();
        while let Ok(characteristic) = client
            .characteristic_by_uuid::<BroadcastReceiveStateValue>(handle, &Uuid::from(characteristic::BROADCAST_RECEIVE_STATE))
            .await
        {
            if receive_states.push(characteristic).is_err() {
                break;
            }
        }

        Some(Self { handle: handle.clone(), control_point, receive_states })
    }
}

/// A Gatt service server exposing Scan Delegator control over up to `MAX_SOURCES` broadcast
/// sources.
pub struct BassServer<const MAX_SOURCES: usize> {
    handle: u16,
    control_point: Characteristic<BroadcastAudioScanControlPointOperation>,
    receive_states: HVec<Characteristic<BroadcastReceiveStateValue>, MAX_SOURCES>,
}

pub const BASS_ATTRIBUTES: usize = 24;

impl<const MAX_SOURCES: usize> BassServer<MAX_SOURCES> {
    /// Creates a new BASS Gatt service with `MAX_SOURCES` empty receive-state slots.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        control_point_store: &'a mut [u8],
        receive_state_stores: &'a mut [&'a mut [u8]],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::BROADCAST_AUDIO_SCAN));

        let control_point = service
            .add_characteristic(
                characteristic::BROADCAST_AUDIO_SCAN_CONTROL_POINT,
                &[CharacteristicProp::Write, CharacteristicProp::WriteWithoutResponse],
                BroadcastAudioScanControlPointOperation::default(),
                control_point_store,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let mut receive_states = HVec::new();
        for store in receive_state_stores.iter_mut() {
            let characteristic = service
                .add_characteristic(
                    characteristic::BROADCAST_RECEIVE_STATE,
                    &[CharacteristicProp::Read, CharacteristicProp::Notify],
                    BroadcastReceiveStateValue::new(&BroadcastReceiveState::empty()),
                    store,
                )
                .read_permission(PermissionLevel::EncryptionRequired)
                .build();
            receive_states.push(characteristic).map_err(|_| "too many receive state slots").unwrap();
        }

        Self { handle: service.build(), control_point, receive_states }
    }

    pub fn control_point(&self) -> &Characteristic<BroadcastAudioScanControlPointOperation> {
        &self.control_point
    }
    pub fn receive_states(&self) -> &[Characteristic<BroadcastReceiveStateValue>] {
        &self.receive_states
    }
}

impl<const MAX_SOURCES: usize, P: PacketPool> LeAudioServerService<P> for BassServer<MAX_SOURCES> {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if self.receive_states.iter().any(|c| c.handle == event.handle()) {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        // The Control Point's business rules need the stored receive-state slots, so
        // `Server::handle` special-cases it before it would otherwise reach this generic path -
        // the same way it special-cases VCS's Volume Control Point.
        if event.handle() == self.control_point.handle {
            return Some(match event.value(&self.control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Applies a Broadcast Audio Scan Control Point write end to end: mutates `bass`'s stored
/// receive-state slots (see `apply_add_source`/`apply_modify_source`/`apply_remove_source`), and
/// stores + notifies whichever slot changed. `RemoteScanStarted`/`RemoteScanStopped` have no
/// server-side state to change in this simplified model. Returns the `AttErrorCode` the ATT write
/// itself should be rejected with on failure, or `Ok(())` if it should be accepted.
pub async fn drive_control_point<M: RawMutex, P: PacketPool, const MAX_SOURCES: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    bass: &BassServer<MAX_SOURCES>,
    conn: &GattConnection<'_, '_, P>,
    operation: &BroadcastAudioScanControlPointOperation,
) -> Result<(), AttErrorCode> {
    let mut slots: AVec<BroadcastReceiveState> = AVec::with_capacity(bass.receive_states.len());
    for characteristic in &bass.receive_states {
        let value = characteristic.get(server).map_err(|_| AttErrorCode::UNLIKELY_ERROR)?;
        slots.push(value.decode().map_err(|_| AttErrorCode::UNLIKELY_ERROR)?);
    }

    match &operation.opcode {
        BroadcastAudioScanControlPointOpcode::RemoteScanStarted | BroadcastAudioScanControlPointOpcode::RemoteScanStopped => Ok(()),
        BroadcastAudioScanControlPointOpcode::AddSource {
            advertiser_address_type,
            advertiser_address,
            advertising_sid,
            broadcast_id,
            subgroups,
            ..
        } => {
            let position = slots.iter().position(BroadcastReceiveState::is_empty).ok_or(AttErrorCode::INSUFFICIENT_RESOURCES)?;
            apply_add_source(&mut slots, *advertiser_address_type, *advertiser_address, *advertising_sid, *broadcast_id, subgroups.clone())?;
            let value = BroadcastReceiveStateValue::new(&slots[position]);
            let _ = bass.receive_states[position].notify(conn, &value, true).await;
            Ok(())
        }
        BroadcastAudioScanControlPointOpcode::ModifySource { source_id, subgroups, .. } => {
            apply_modify_source(&mut slots, *source_id, subgroups.clone())?;
            let position = find(&slots, *source_id).unwrap();
            let value = BroadcastReceiveStateValue::new(&slots[position]);
            let _ = bass.receive_states[position].notify(conn, &value, true).await;
            Ok(())
        }
        BroadcastAudioScanControlPointOpcode::RemoveSource { source_id } => {
            let position = find(&slots, *source_id).ok_or(INVALID_SOURCE_ID)?;
            apply_remove_source(&mut slots, *source_id)?;
            let value = BroadcastReceiveStateValue::new(&slots[position]);
            let _ = bass.receive_states[position].notify(conn, &value, true).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn sample_subgroups() -> AVec<Subgroup> {
        let mut v = AVec::new();
        v.push(Subgroup { bis_sync: 0b11, metadata: AVec::new() });
        v
    }

    #[test]
    fn control_point_operation_round_trips_every_opcode_shape() {
        let ops = [
            BroadcastAudioScanControlPointOpcode::RemoteScanStarted,
            BroadcastAudioScanControlPointOpcode::RemoteScanStopped,
            BroadcastAudioScanControlPointOpcode::AddSource {
                advertiser_address_type: AddressType::Random,
                advertiser_address: [1, 2, 3, 4, 5, 6],
                advertising_sid: 1,
                broadcast_id: 0xABCDEF,
                pa_sync: PaSyncParam::SyncPastAvailable,
                pa_interval: 100,
                subgroups: sample_subgroups(),
            },
            BroadcastAudioScanControlPointOpcode::ModifySource {
                source_id: 0,
                pa_sync: PaSyncParam::SyncNoPast,
                pa_interval: 200,
                subgroups: sample_subgroups(),
            },
            BroadcastAudioScanControlPointOpcode::RemoveSource { source_id: 2 },
        ];
        for opcode in ops {
            let op = BroadcastAudioScanControlPointOperation::new(opcode.clone());
            assert_eq!(BroadcastAudioScanControlPointOperation::from_gatt(op.as_gatt()).unwrap().opcode, opcode);
        }
    }

    #[test]
    fn broadcast_receive_state_round_trips_without_bad_code() {
        let mut state = BroadcastReceiveState::empty();
        state.source_id = 0;
        state.subgroups = sample_subgroups();
        let value = BroadcastReceiveStateValue::new(&state);
        assert_eq!(value.decode().unwrap(), state);
    }

    #[test]
    fn broadcast_receive_state_round_trips_with_bad_code() {
        let mut state = BroadcastReceiveState::empty();
        state.source_id = 1;
        state.big_encryption = BigEncryption::BadCode;
        state.bad_code = Some([0xAA; 16]);
        let value = BroadcastReceiveStateValue::new(&state);
        assert_eq!(value.decode().unwrap(), state);
    }

    #[test]
    fn add_source_allocates_first_empty_slot_and_assigns_next_source_id() {
        let mut slots = [BroadcastReceiveState::empty(), BroadcastReceiveState::empty()];
        let id = apply_add_source(&mut slots, AddressType::Public, [0; 6], 0, 42, AVec::new()).unwrap();
        assert_eq!(id, 0);
        assert!(!slots[0].is_empty());
        assert!(slots[1].is_empty());
    }

    #[test]
    fn add_source_rejects_when_all_slots_are_full() {
        let mut slots = [BroadcastReceiveState::empty()];
        apply_add_source(&mut slots, AddressType::Public, [0; 6], 0, 42, AVec::new()).unwrap();
        let result = apply_add_source(&mut slots, AddressType::Public, [0; 6], 0, 43, AVec::new());
        assert_eq!(result, Err(AttErrorCode::INSUFFICIENT_RESOURCES));
    }

    #[test]
    fn modify_source_rejects_an_unknown_source_id() {
        let mut slots = [BroadcastReceiveState::empty()];
        assert_eq!(apply_modify_source(&mut slots, 5, AVec::new()), Err(INVALID_SOURCE_ID));
    }

    #[test]
    fn remove_source_frees_the_slot() {
        let mut slots = [BroadcastReceiveState::empty()];
        let id = apply_add_source(&mut slots, AddressType::Public, [0; 6], 0, 42, AVec::new()).unwrap();
        apply_remove_source(&mut slots, id).unwrap();
        assert!(slots[0].is_empty());
    }

    #[test]
    fn bass_server_exposes_receive_state_slots_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static CP: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
        static SLOT0: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
        static SLOTS: static_cell::StaticCell<[&'static mut [u8]; 1]> = static_cell::StaticCell::new();

        let slot0: &'static mut [u8] = SLOT0.init([0; 64]);
        let slots = SLOTS.init([slot0]);
        let bass: BassServer<1> = BassServer::new(&mut table, CP.init([0; 64]), slots);
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(bass.receive_states().len(), 1);
        let value = bass.receive_states()[0].get(&server).unwrap();
        assert!(value.decode().unwrap().is_empty());
    }
}
