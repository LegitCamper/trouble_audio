//! ## Telephone Bearer Service
//!
//! The Telephone Bearer Service (TBS) exposes call control - the "phone call" analog of `mcs`'s
//! media control: accept/terminate/hold/retrieve/originate/join calls, and observe each call's
//! state. Bluetooth TBS v1.0.
//!
//! This implements the **Generic** TBS (service UUID `GENERIC_TELEPHONE_BEARER`, singular per
//! device, standing in for "whichever bearer currently has calls") rather than per-bearer TBS
//! instances, matching this crate's single-active-connection, single-player model (see the
//! analogous note atop ascs.rs/mcs.rs).
//!
//! Opcode numbering and result/error codes are reconstructed from public documentation, not
//! cross-checked against the official Bluetooth SIG assigned-numbers PDF - verify before relying
//! on the exact values for certification. The Call Control Point's response is sent via
//! `notify_raw` here (like this crate's other control points), though the real TBS spec specifies
//! "Notify" for it too (unlike HAS's Preset Control Point, which specifies Indicate) - so this one
//! matches the spec's characteristic property exactly, just simplified to a single-response model
//! (one Call Control Point Notification per write) rather than the potential
//! notify-once-per-affected-call some operations like Join could produce.

use alloc::vec::Vec as AVec;

use bitflags::bitflags;
use bt_hci::uuid::{characteristic, service};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::String as HString;
use trouble_host::{
    gatt::{GattConnection, ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{LeAudioServerService, MAX_SERVICES};

/// Call URIs/names/friendly names are capped at this length in this crate (not itself a spec
/// limit - TBS leaves these unbounded, but a `no_std` crate needs a fixed bound somewhere).
pub const MAX_URI_LEN: usize = 32;

/// Call Control Point Notification result codes ("Call Control Point Notification" table). See
/// the module doc comment's disclaimer.
pub const RESULT_SUCCESS: u8 = 0x00;
pub const RESULT_OPCODE_NOT_SUPPORTED: u8 = 0x01;
pub const RESULT_OPERATION_NOT_POSSIBLE: u8 = 0x02;
pub const RESULT_INVALID_CALL_INDEX: u8 = 0x03;
pub const RESULT_STATE_MISMATCH: u8 = 0x04;
pub const RESULT_LACK_OF_RESOURCES: u8 = 0x05;
pub const RESULT_INVALID_OUTGOING_URI: u8 = 0x06;

/// Bearer_Technology (TBS, "Bearer Technology" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BearerTechnology {
    #[default]
    Unknown = 0x00,
    ThreeG = 0x01,
    FourG = 0x02,
    Lte = 0x03,
    WiFi = 0x04,
    FiveG = 0x05,
    Gsm = 0x06,
    Cdma = 0x07,
    TwoG = 0x08,
    Wcdma = 0x09,
    Ip = 0x0A,
}

impl TryFrom<u8> for BearerTechnology {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Unknown),
            0x01 => Ok(Self::ThreeG),
            0x02 => Ok(Self::FourG),
            0x03 => Ok(Self::Lte),
            0x04 => Ok(Self::WiFi),
            0x05 => Ok(Self::FiveG),
            0x06 => Ok(Self::Gsm),
            0x07 => Ok(Self::Cdma),
            0x08 => Ok(Self::TwoG),
            0x09 => Ok(Self::Wcdma),
            0x0A => Ok(Self::Ip),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for BearerTechnology {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for BearerTechnology {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for BearerTechnology {
    const SIZE: usize = 1;
}

bitflags! {
    /// Status_Flags (TBS, "Status Flags" table).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusFlags: u16 {
        const InbandRingtoneEnabled = 1 << 0;
        const ServerIsSilent = 1 << 1;
    }
}

impl AsGatt for StatusFlags {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 2;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u16 here).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 2) }
    }
}

impl FromGatt for StatusFlags {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 2] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u16::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for StatusFlags {
    const SIZE: usize = 2;
}

/// Call_State (TBS, "Call State" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallState {
    Incoming = 0x00,
    Dialing = 0x01,
    Alerting = 0x02,
    Active = 0x03,
    LocallyHeld = 0x04,
    RemotelyHeld = 0x05,
    LocallyAndRemotelyHeld = 0x06,
}

impl TryFrom<u8> for CallState {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Incoming),
            0x01 => Ok(Self::Dialing),
            0x02 => Ok(Self::Alerting),
            0x03 => Ok(Self::Active),
            0x04 => Ok(Self::LocallyHeld),
            0x05 => Ok(Self::RemotelyHeld),
            0x06 => Ok(Self::LocallyAndRemotelyHeld),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

bitflags! {
    /// Call_Flags (TBS, "Call State" table).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CallFlags: u8 {
        /// Set for an outgoing call, clear for an incoming one.
        const Outgoing = 1 << 0;
        const InfoWithheldByServer = 1 << 1;
        const InfoWithheldByNetwork = 1 << 2;
    }
}

/// One active call, as tracked server-side. The Call State/Bearer List Current Calls
/// characteristics are each derived views over this (see [`CallStateValue`]/
/// [`BearerListCurrentCallsValue`]).
///
/// No `defmt::Format` derive (unlike most value types in this crate) - `CallFlags` is a
/// `bitflags!`-generated struct, which doesn't implement it (see the analogous omission on
/// `vcs::VolumeFlags`/`has::PresetRecord`), so a derive here would fail to compile under the
/// `defmt` feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub call_index: u8,
    pub state: CallState,
    pub flags: CallFlags,
    pub uri: HString<MAX_URI_LEN>,
}

/// Value wrapper for the Call State characteristic: a back-to-back list of Call_Index (1) |
/// Call_State (1) | Call_Flags (1) triples, one per active call. Pre-encoded like `PAC`/`Ase`.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct CallStateValue {
    encoded: AVec<u8>,
}

impl CallStateValue {
    pub fn new(calls: &[CallRecord]) -> Self {
        let mut encoded = AVec::with_capacity(calls.len() * 3);
        for call in calls {
            encoded.push(call.call_index);
            encoded.push(call.state as u8);
            encoded.push(call.flags.bits());
        }
        Self { encoded }
    }
}

impl AsGatt for CallStateValue {
    const MIN_SIZE: usize = 0;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for CallStateValue {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() % 3 != 0 {
            return Err(FromGattError::InvalidLength);
        }
        for triple in data.chunks_exact(3) {
            CallState::try_from(triple[1])?;
        }
        Ok(Self { encoded: AVec::from(data) })
    }
}

/// Value wrapper for the Bearer List Current Calls characteristic: a back-to-back list of
/// Length (1) | Call_Index (1) | Call_State (1) | Call_Flags (1) | Call_URI (`Length - 3`) items.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone)]
pub struct BearerListCurrentCallsValue {
    encoded: AVec<u8>,
}

impl BearerListCurrentCallsValue {
    pub fn new(calls: &[CallRecord]) -> Self {
        let mut encoded = AVec::new();
        for call in calls {
            let uri = call.uri.as_bytes();
            encoded.push((3 + uri.len()) as u8);
            encoded.push(call.call_index);
            encoded.push(call.state as u8);
            encoded.push(call.flags.bits());
            encoded.extend_from_slice(uri);
        }
        Self { encoded }
    }
}

impl AsGatt for BearerListCurrentCallsValue {
    const MIN_SIZE: usize = 0;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for BearerListCurrentCallsValue {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let mut pos = 0;
        while pos < data.len() {
            let len = *data.get(pos).ok_or(FromGattError::InvalidLength)? as usize;
            if len < 3 {
                return Err(FromGattError::InvalidLength);
            }
            let end = pos.checked_add(1 + len).ok_or(FromGattError::InvalidLength)?;
            let item = data.get(pos + 1..end).ok_or(FromGattError::InvalidLength)?;
            CallState::try_from(item[1])?;
            core::str::from_utf8(&item[3..]).map_err(|_| FromGattError::InvalidCharacter)?;
            pos = end;
        }
        Ok(Self { encoded: AVec::from(data) })
    }
}

/// Call Control Point Opcodes ("Call Control Point" operations table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallControlPointOpcode {
    Accept(u8),
    Terminate(u8),
    LocalHold(u8),
    LocalRetrieve(u8),
    Originate(HString<MAX_URI_LEN>),
    Join(HeaplessCallIndices),
}

/// A small fixed-capacity list of Call_Index values, as carried by the Join opcode.
pub type HeaplessCallIndices = heapless::Vec<u8, 8>;

impl CallControlPointOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::Accept(_) => 0x00,
            Self::Terminate(_) => 0x01,
            Self::LocalHold(_) => 0x02,
            Self::LocalRetrieve(_) => 0x03,
            Self::Originate(_) => 0x04,
            Self::Join(_) => 0x05,
        }
    }

    fn encode(&self, out: &mut AVec<u8>) {
        out.push(self.raw_opcode());
        match self {
            Self::Accept(i) | Self::Terminate(i) | Self::LocalHold(i) | Self::LocalRetrieve(i) => out.push(*i),
            Self::Originate(uri) => out.extend_from_slice(uri.as_bytes()),
            Self::Join(indices) => out.extend_from_slice(indices),
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = *data.first().ok_or(FromGattError::InvalidLength)?;
        let rest = &data[1..];
        Ok(match opcode {
            0x00 | 0x01 | 0x02 | 0x03 => {
                if rest.len() != 1 {
                    return Err(FromGattError::InvalidLength);
                }
                match opcode {
                    0x00 => Self::Accept(rest[0]),
                    0x01 => Self::Terminate(rest[0]),
                    0x02 => Self::LocalHold(rest[0]),
                    _ => Self::LocalRetrieve(rest[0]),
                }
            }
            0x04 => {
                let uri = core::str::from_utf8(rest).map_err(|_| FromGattError::InvalidCharacter)?;
                Self::Originate(HString::try_from(uri).map_err(|_| FromGattError::InvalidLength)?)
            }
            0x05 => {
                if rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::Join(HeaplessCallIndices::from_slice(rest).map_err(|_| FromGattError::InvalidLength)?)
            }
            _ => return Err(FromGattError::InvalidLength),
        })
    }
}

/// The value written to the Call Control Point characteristic. Wire bytes are cached in `encoded`
/// for the same reason as `vcs::VolumeControlPointOperation`.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallControlPointOperation {
    pub opcode: CallControlPointOpcode,
    encoded: AVec<u8>,
}

impl CallControlPointOperation {
    pub fn new(opcode: CallControlPointOpcode) -> Self {
        let mut encoded = AVec::new();
        opcode.encode(&mut encoded);
        Self { opcode, encoded }
    }
}

impl AsGatt for CallControlPointOperation {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for CallControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = CallControlPointOpcode::decode(data)?;
        Ok(Self::new(opcode))
    }
}

impl Default for CallControlPointOperation {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(CallControlPointOpcode::Terminate(0))
    }
}

/// The Call Control Point notification sent back after processing an operation: Opcode (1) |
/// Call_Index (1) | Result_Code (1).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct CallControlPointNotification {
    pub requested_opcode: u8,
    pub call_index: u8,
    pub result_code: u8,
}

impl AsGatt for CallControlPointNotification {
    const MIN_SIZE: usize = 3;
    const MAX_SIZE: usize = 3;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`, three adjacent one-byte fields with no padding.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 3) }
    }
}

fn find(calls: &[CallRecord], call_index: u8) -> Option<usize> {
    calls.iter().position(|c| c.call_index == call_index)
}

/// Applies an Accept operation: the call must exist and be [`CallState::Incoming`].
pub fn accept_call(calls: &mut [CallRecord], call_index: u8) -> Result<(), u8> {
    let position = find(calls, call_index).ok_or(RESULT_INVALID_CALL_INDEX)?;
    if calls[position].state != CallState::Incoming {
        return Err(RESULT_STATE_MISMATCH);
    }
    calls[position].state = CallState::Active;
    Ok(())
}

/// Applies a Local Hold operation: `Active` -> `LocallyHeld`, `RemotelyHeld` ->
/// `LocallyAndRemotelyHeld`.
pub fn local_hold(calls: &mut [CallRecord], call_index: u8) -> Result<(), u8> {
    let position = find(calls, call_index).ok_or(RESULT_INVALID_CALL_INDEX)?;
    calls[position].state = match calls[position].state {
        CallState::Active => CallState::LocallyHeld,
        CallState::RemotelyHeld => CallState::LocallyAndRemotelyHeld,
        _ => return Err(RESULT_STATE_MISMATCH),
    };
    Ok(())
}

/// Applies a Local Retrieve operation: `LocallyHeld` -> `Active`, `LocallyAndRemotelyHeld` ->
/// `RemotelyHeld`.
pub fn local_retrieve(calls: &mut [CallRecord], call_index: u8) -> Result<(), u8> {
    let position = find(calls, call_index).ok_or(RESULT_INVALID_CALL_INDEX)?;
    calls[position].state = match calls[position].state {
        CallState::LocallyHeld => CallState::Active,
        CallState::LocallyAndRemotelyHeld => CallState::RemotelyHeld,
        _ => return Err(RESULT_STATE_MISMATCH),
    };
    Ok(())
}

/// Applies a Terminate operation: removes the call entirely.
pub fn terminate_call<const N: usize>(calls: &mut heapless::Vec<CallRecord, N>, call_index: u8) -> Result<(), u8> {
    let position = find(calls, call_index).ok_or(RESULT_INVALID_CALL_INDEX)?;
    calls.remove(position);
    Ok(())
}

/// Applies an Originate operation: adds a new outgoing call in the `Dialing` state, assigning it
/// a fresh Call_Index (one past the highest currently in use).
pub fn originate_call<const N: usize>(calls: &mut heapless::Vec<CallRecord, N>, uri: HString<MAX_URI_LEN>) -> Result<u8, u8> {
    if uri.is_empty() {
        return Err(RESULT_INVALID_OUTGOING_URI);
    }
    let call_index = calls.iter().map(|c| c.call_index).max().map_or(0, |i| i.wrapping_add(1));
    calls
        .push(CallRecord {
            call_index,
            state: CallState::Dialing,
            flags: CallFlags::Outgoing,
            uri,
        })
        .map_err(|_| RESULT_LACK_OF_RESOURCES)?;
    Ok(call_index)
}

/// Applies a Join operation: every given call index must exist; each becomes `Active`.
pub fn join_calls(calls: &mut [CallRecord], indices: &[u8]) -> Result<(), u8> {
    if indices.len() < 2 {
        return Err(RESULT_OPERATION_NOT_POSSIBLE);
    }
    for &index in indices {
        if find(calls, index).is_none() {
            return Err(RESULT_INVALID_CALL_INDEX);
        }
    }
    for &index in indices {
        let position = find(calls, index).unwrap();
        calls[position].state = CallState::Active;
    }
    Ok(())
}

/// A Gatt service client for reading/controlling a device's telephone bearer.
pub struct TbsClient {
    handle: ServiceHandle,
    pub bearer_provider_name: Characteristic<HString<32>>,
    pub bearer_technology: Characteristic<BearerTechnology>,
    pub signal_strength: Characteristic<u8>,
    pub call_state: Characteristic<CallStateValue>,
    pub bearer_list_current_calls: Characteristic<BearerListCurrentCallsValue>,
    pub content_control_id: Characteristic<u8>,
    pub status_flags: Characteristic<StatusFlags>,
    pub call_control_point: Characteristic<CallControlPointOperation>,
}

impl TbsClient {
    /// Discovers the (Generic) TBS service and its characteristics on an already-connected
    /// `GattClient`.
    pub async fn new<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Self {
        let services = client
            .services_by_uuid(&Uuid::from(service::GENERIC_TELEPHONE_BEARER))
            .await
            .unwrap();
        let handle = services.first().unwrap();

        macro_rules! required {
            ($uuid:expr) => {
                client.characteristic_by_uuid(handle, &Uuid::from($uuid)).await.expect("mandatory TBS characteristic missing")
            };
        }

        Self {
            handle: handle.clone(),
            bearer_provider_name: required!(characteristic::BEARER_PROVIDER_NAME),
            bearer_technology: required!(characteristic::BEARER_TECHNOLOGY),
            signal_strength: required!(characteristic::BEARER_SIGNAL_STRENGTH),
            call_state: required!(characteristic::CALL_STATE),
            bearer_list_current_calls: required!(characteristic::BEARER_LIST_CURRENT_CALLS),
            content_control_id: required!(characteristic::CONTENT_CONTROL_ID),
            status_flags: required!(characteristic::STATUS_FLAGS),
            call_control_point: required!(characteristic::CALL_CONTROL_POINT),
        }
    }
}

/// Initial values for [`TbsServer::new`].
pub struct TbsInit {
    pub bearer_provider_name: HString<32>,
    pub bearer_technology: BearerTechnology,
    pub content_control_id: u8,
    pub status_flags: StatusFlags,
}

/// Backing storage buffers for [`TbsServer::new`].
pub struct TbsStore<'a> {
    pub bearer_provider_name: &'a mut [u8],
    pub bearer_technology: &'a mut [u8],
    pub signal_strength: &'a mut [u8],
    pub call_state: &'a mut [u8],
    pub bearer_list_current_calls: &'a mut [u8],
    pub content_control_id: &'a mut [u8],
    pub status_flags: &'a mut [u8],
    pub call_control_point: &'a mut [u8],
}

/// A Gatt service server exposing this device's telephone bearer, with up to `MAX_CALLS`
/// concurrent calls.
pub struct TbsServer<const MAX_CALLS: usize> {
    handle: u16,
    bearer_provider_name: Characteristic<HString<32>>,
    bearer_technology: Characteristic<BearerTechnology>,
    signal_strength: Characteristic<u8>,
    call_state: Characteristic<CallStateValue>,
    bearer_list_current_calls: Characteristic<BearerListCurrentCallsValue>,
    content_control_id: Characteristic<u8>,
    status_flags: Characteristic<StatusFlags>,
    call_control_point: Characteristic<CallControlPointOperation>,
    calls: RefCell<heapless::Vec<CallRecord, MAX_CALLS>>,
}

pub const TBS_ATTRIBUTES: usize = 26;

impl<const MAX_CALLS: usize> TbsServer<MAX_CALLS> {
    /// Creates a new (Generic) TBS Gatt service, initially with no active calls.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        init: TbsInit,
        store: TbsStore<'a>,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::GENERIC_TELEPHONE_BEARER));

        let bearer_provider_name = service
            .add_characteristic(
                characteristic::BEARER_PROVIDER_NAME,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                init.bearer_provider_name,
                store.bearer_provider_name,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let bearer_technology = service
            .add_characteristic(
                characteristic::BEARER_TECHNOLOGY,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                init.bearer_technology,
                store.bearer_technology,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let signal_strength = service
            .add_characteristic(
                characteristic::BEARER_SIGNAL_STRENGTH,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                0xFFu8, // 255 = unknown
                store.signal_strength,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let call_state = service
            .add_characteristic(
                characteristic::CALL_STATE,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                CallStateValue::new(&[]),
                store.call_state,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let bearer_list_current_calls = service
            .add_characteristic(
                characteristic::BEARER_LIST_CURRENT_CALLS,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                BearerListCurrentCallsValue::new(&[]),
                store.bearer_list_current_calls,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let content_control_id = service
            .add_characteristic(
                characteristic::CONTENT_CONTROL_ID,
                &[CharacteristicProp::Read],
                init.content_control_id,
                store.content_control_id,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let status_flags = service
            .add_characteristic(
                characteristic::STATUS_FLAGS,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                init.status_flags,
                store.status_flags,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let call_control_point = service
            .add_characteristic(
                characteristic::CALL_CONTROL_POINT,
                &[CharacteristicProp::Write, CharacteristicProp::Notify],
                CallControlPointOperation::default(),
                store.call_control_point,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self {
            handle: service.build(),
            bearer_provider_name,
            bearer_technology,
            signal_strength,
            call_state,
            bearer_list_current_calls,
            content_control_id,
            status_flags,
            call_control_point,
            calls: RefCell::new(heapless::Vec::new()),
        }
    }

    pub fn bearer_provider_name(&self) -> &Characteristic<HString<32>> {
        &self.bearer_provider_name
    }
    pub fn bearer_technology(&self) -> &Characteristic<BearerTechnology> {
        &self.bearer_technology
    }
    pub fn signal_strength(&self) -> &Characteristic<u8> {
        &self.signal_strength
    }
    pub fn call_state(&self) -> &Characteristic<CallStateValue> {
        &self.call_state
    }
    pub fn bearer_list_current_calls(&self) -> &Characteristic<BearerListCurrentCallsValue> {
        &self.bearer_list_current_calls
    }
    pub fn content_control_id(&self) -> &Characteristic<u8> {
        &self.content_control_id
    }
    pub fn status_flags(&self) -> &Characteristic<StatusFlags> {
        &self.status_flags
    }
    pub fn call_control_point(&self) -> &Characteristic<CallControlPointOperation> {
        &self.call_control_point
    }
    /// A snapshot of the currently tracked calls.
    pub fn calls(&self) -> heapless::Vec<CallRecord, MAX_CALLS> {
        self.calls.borrow().clone()
    }
}

impl<const MAX_CALLS: usize, P: PacketPool> LeAudioServerService<P> for TbsServer<MAX_CALLS> {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        let handles = [
            self.bearer_provider_name.handle,
            self.bearer_technology.handle,
            self.signal_strength.handle,
            self.call_state.handle,
            self.bearer_list_current_calls.handle,
            self.content_control_id.handle,
            self.status_flags.handle,
        ];
        if handles.contains(&event.handle()) {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        // The Call Control Point's business rules need the stored call list, so `Server::handle`
        // special-cases it before it would otherwise reach this generic path - the same way it
        // special-cases MCS's Media Control Point.
        if event.handle() == self.call_control_point.handle {
            return Some(match event.value(&self.call_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Applies a Call Control Point write end to end: mutates `tbs`'s stored call list (see
/// `accept_call`/`local_hold`/`local_retrieve`/`terminate_call`/`originate_call`/`join_calls`),
/// stores + notifies Call State/Bearer List Current Calls, and sends the resulting
/// [`CallControlPointNotification`] on the Call Control Point characteristic itself (mirroring
/// `mcs::drive_media_control_point`'s accept-then-notify handling).
pub async fn drive_call_control_point<M: RawMutex, P: PacketPool, const MAX_CALLS: usize, const MAX_CONNECTIONS: usize>(
    // Unlike `vcs`/`mcs`'s drive functions, TBS's call list lives in `TbsServer::calls` (a
    // `RefCell`), not in a GATT-backed characteristic, so the `AttributeServer` itself is never
    // read here - only kept as a parameter to match this crate's other `drive_*` signatures and
    // to pin `M`/`MAX_CONNECTIONS` the same way callers already expect.
    _server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    tbs: &TbsServer<MAX_CALLS>,
    conn: &GattConnection<'_, '_, P>,
    operation: &CallControlPointOperation,
) {
    let (call_index, result_code) = {
        let mut calls = tbs.calls.borrow_mut();
        match &operation.opcode {
            CallControlPointOpcode::Accept(i) => (*i, accept_call(&mut calls, *i).err().unwrap_or(RESULT_SUCCESS)),
            CallControlPointOpcode::Terminate(i) => (*i, terminate_call(&mut calls, *i).err().unwrap_or(RESULT_SUCCESS)),
            CallControlPointOpcode::LocalHold(i) => (*i, local_hold(&mut calls, *i).err().unwrap_or(RESULT_SUCCESS)),
            CallControlPointOpcode::LocalRetrieve(i) => (*i, local_retrieve(&mut calls, *i).err().unwrap_or(RESULT_SUCCESS)),
            CallControlPointOpcode::Originate(uri) => match originate_call(&mut calls, uri.clone()) {
                Ok(index) => (index, RESULT_SUCCESS),
                Err(code) => (0, code),
            },
            CallControlPointOpcode::Join(indices) => {
                let first = indices.first().copied().unwrap_or(0);
                (first, join_calls(&mut calls, indices).err().unwrap_or(RESULT_SUCCESS))
            }
        }
    };

    if result_code == RESULT_SUCCESS {
        let calls = tbs.calls.borrow();
        let _ = tbs.call_state.notify(conn, &CallStateValue::new(&calls), true).await;
        let _ = tbs.bearer_list_current_calls.notify(conn, &BearerListCurrentCallsValue::new(&calls), true).await;
    }

    let notification = CallControlPointNotification {
        requested_opcode: operation.opcode.raw_opcode(),
        call_index,
        result_code,
    };
    let _ = tbs.call_control_point.notify_raw(conn, notification.as_gatt(), false).await;
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn call(index: u8, state: CallState, flags: CallFlags, uri: &str) -> CallRecord {
        CallRecord { call_index: index, state, flags, uri: HString::try_from(uri).unwrap() }
    }

    #[test]
    fn call_control_point_operation_round_trips_every_opcode_shape() {
        let mut indices = HeaplessCallIndices::new();
        let _ = indices.push(1);
        let _ = indices.push(2);
        for opcode in [
            CallControlPointOpcode::Accept(1),
            CallControlPointOpcode::Terminate(2),
            CallControlPointOpcode::LocalHold(3),
            CallControlPointOpcode::LocalRetrieve(4),
            CallControlPointOpcode::Originate(HString::try_from("tel:+123456").unwrap()),
            CallControlPointOpcode::Join(indices),
        ] {
            let op = CallControlPointOperation::new(opcode.clone());
            assert_eq!(CallControlPointOperation::from_gatt(op.as_gatt()).unwrap().opcode, opcode);
        }
    }

    #[test]
    fn call_state_value_round_trips() {
        let calls = [call(1, CallState::Active, CallFlags::Outgoing, "tel:+1"), call(2, CallState::Incoming, CallFlags::empty(), "tel:+2")];
        let value = CallStateValue::new(&calls);
        assert_eq!(value.as_gatt().len(), 6);
        assert!(CallStateValue::from_gatt(value.as_gatt()).is_ok());
    }

    #[test]
    fn bearer_list_current_calls_round_trips() {
        let calls = [call(1, CallState::Active, CallFlags::Outgoing, "tel:+123")];
        let value = BearerListCurrentCallsValue::new(&calls);
        assert!(BearerListCurrentCallsValue::from_gatt(value.as_gatt()).is_ok());
    }

    #[test]
    fn accept_call_transitions_incoming_to_active() {
        let mut calls = [call(1, CallState::Incoming, CallFlags::empty(), "tel:+1")];
        accept_call(&mut calls, 1).unwrap();
        assert_eq!(calls[0].state, CallState::Active);
    }

    #[test]
    fn accept_call_rejects_a_call_not_in_incoming_state() {
        let mut calls = [call(1, CallState::Active, CallFlags::empty(), "tel:+1")];
        assert_eq!(accept_call(&mut calls, 1), Err(RESULT_STATE_MISMATCH));
    }

    #[test]
    fn accept_call_rejects_an_unknown_call_index() {
        let mut calls: [CallRecord; 0] = [];
        assert_eq!(accept_call(&mut calls, 9), Err(RESULT_INVALID_CALL_INDEX));
    }

    #[test]
    fn local_hold_and_retrieve_round_trip_through_active() {
        let mut calls = [call(1, CallState::Active, CallFlags::empty(), "tel:+1")];
        local_hold(&mut calls, 1).unwrap();
        assert_eq!(calls[0].state, CallState::LocallyHeld);
        local_retrieve(&mut calls, 1).unwrap();
        assert_eq!(calls[0].state, CallState::Active);
    }

    #[test]
    fn local_hold_on_remotely_held_call_becomes_locally_and_remotely_held() {
        let mut calls = [call(1, CallState::RemotelyHeld, CallFlags::empty(), "tel:+1")];
        local_hold(&mut calls, 1).unwrap();
        assert_eq!(calls[0].state, CallState::LocallyAndRemotelyHeld);
    }

    #[test]
    fn terminate_call_removes_it() {
        let mut calls: heapless::Vec<CallRecord, 4> = heapless::Vec::new();
        calls.push(call(1, CallState::Active, CallFlags::empty(), "tel:+1")).unwrap();
        terminate_call(&mut calls, 1).unwrap();
        assert!(calls.is_empty());
    }

    #[test]
    fn originate_call_adds_a_dialing_call_with_a_fresh_index() {
        let mut calls: heapless::Vec<CallRecord, 4> = heapless::Vec::new();
        calls.push(call(3, CallState::Active, CallFlags::empty(), "tel:+1")).unwrap();
        let new_index = originate_call(&mut calls, HString::try_from("tel:+999").unwrap()).unwrap();
        assert_eq!(new_index, 4);
        assert_eq!(calls[1].state, CallState::Dialing);
    }

    #[test]
    fn originate_call_rejects_an_empty_uri() {
        let mut calls: heapless::Vec<CallRecord, 4> = heapless::Vec::new();
        assert_eq!(originate_call(&mut calls, HString::new()), Err(RESULT_INVALID_OUTGOING_URI));
    }

    #[test]
    fn join_calls_makes_every_listed_call_active() {
        let mut calls = [
            call(1, CallState::LocallyHeld, CallFlags::empty(), "tel:+1"),
            call(2, CallState::RemotelyHeld, CallFlags::empty(), "tel:+2"),
        ];
        join_calls(&mut calls, &[1, 2]).unwrap();
        assert!(calls.iter().all(|c| c.state == CallState::Active));
    }

    #[test]
    fn join_calls_rejects_an_unknown_call_index() {
        let mut calls = [call(1, CallState::Active, CallFlags::empty(), "tel:+1")];
        assert_eq!(join_calls(&mut calls, &[1, 9]), Err(RESULT_INVALID_CALL_INDEX));
    }

    #[test]
    fn tbs_server_exposes_characteristics_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static NAME: static_cell::StaticCell<[u8; 32]> = static_cell::StaticCell::new();
        static TECH: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static SIGNAL: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static CALL_STATE: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
        static LIST_CALLS: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
        static CCID: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static FLAGS: static_cell::StaticCell<[u8; 2]> = static_cell::StaticCell::new();
        static CP: static_cell::StaticCell<[u8; 40]> = static_cell::StaticCell::new();

        let tbs: TbsServer<4> = TbsServer::new(
            &mut table,
            TbsInit {
                bearer_provider_name: HString::try_from("Trouble Phone").unwrap(),
                bearer_technology: BearerTechnology::Lte,
                content_control_id: 2,
                status_flags: StatusFlags::empty(),
            },
            TbsStore {
                bearer_provider_name: NAME.init([0; 32]),
                bearer_technology: TECH.init([0; 1]),
                signal_strength: SIGNAL.init([0; 1]),
                call_state: CALL_STATE.init([0; 64]),
                bearer_list_current_calls: LIST_CALLS.init([0; 64]),
                content_control_id: CCID.init([0; 1]),
                status_flags: FLAGS.init([0; 2]),
                call_control_point: CP.init([0; 40]),
            },
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(tbs.bearer_technology().get(&server).unwrap(), BearerTechnology::Lte);
        assert_eq!(tbs.content_control_id().get(&server).unwrap(), 2);
        assert!(tbs.calls().is_empty());
    }
}
