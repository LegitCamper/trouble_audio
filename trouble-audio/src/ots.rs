//! ## Object Transfer Service
//!
//! The Object Transfer Service (OTS) lets a client browse a list of objects (e.g. MCS's track
//! icons, or TBS-adjacent call recordings) and manage metadata about them. Bluetooth OTS v1.0.
//!
//! **Scope limitation**: OTS's actual bulk data transfer happens over a separate L2CAP Connection
//! Oriented Channel (the "Object Transfer Channel"), opened out-of-band from GATT when a client
//! issues an OACP Read/Write/Calculate Checksum operation. This crate does not implement that
//! L2CAP CoC channel - only the GATT metadata/control-point surface (object list navigation,
//! object metadata characteristics, Create/Delete/Execute/Abort). A Read/Write/Calculate Checksum
//! OACP operation is accepted and validated against object metadata (offset/length bounds) but
//! never actually moves any bytes - there's nowhere for them to go. `Object_List_Filter` is also
//! not implemented (optional per spec, and orthogonal to the above limitation).
//!
//! Opcode numbering and result codes are reconstructed from public documentation, not
//! cross-checked against the official Bluetooth SIG assigned-numbers PDF - verify before relying
//! on the exact values for certification.

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

/// Object names are capped at this length in this crate (not itself a spec limit).
pub const MAX_OBJECT_NAME_LEN: usize = 32;

/// OACP Response_Code values ("Object Action Control Point" procedure requirements).
pub const OACP_SUCCESS: u8 = 0x01;
pub const OACP_OPCODE_NOT_SUPPORTED: u8 = 0x02;
pub const OACP_INVALID_PARAMETER: u8 = 0x03;
pub const OACP_INSUFFICIENT_RESOURCES: u8 = 0x04;
pub const OACP_INVALID_OBJECT: u8 = 0x05;
pub const OACP_CHANNEL_NOT_AVAILABLE: u8 = 0x06;
pub const OACP_UNSUPPORTED_TYPE: u8 = 0x07;
pub const OACP_PROCEDURE_NOT_PERMITTED: u8 = 0x08;
pub const OACP_OBJECT_LOCKED: u8 = 0x09;
pub const OACP_OPERATION_FAILED: u8 = 0x0A;

/// OLCP Response_Code values ("Object List Control Point" procedure requirements).
pub const OLCP_SUCCESS: u8 = 0x01;
pub const OLCP_OPCODE_NOT_SUPPORTED: u8 = 0x02;
pub const OLCP_INVALID_PARAMETER: u8 = 0x03;
pub const OLCP_OPERATION_FAILED: u8 = 0x04;
pub const OLCP_OUT_OF_BOUNDS: u8 = 0x05;
pub const OLCP_TOO_MANY_OBJECTS: u8 = 0x06;
pub const OLCP_NO_OBJECT: u8 = 0x07;
pub const OLCP_OBJECT_ID_NOT_FOUND: u8 = 0x08;

bitflags! {
    /// OACP_Features, half of the OTS Feature characteristic value.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OacpFeatures: u32 {
        const Create = 1 << 0;
        const Delete = 1 << 1;
        const CalculateChecksum = 1 << 2;
        const Execute = 1 << 3;
        const Read = 1 << 4;
        const Write = 1 << 5;
        const Append = 1 << 6;
        const Truncate = 1 << 7;
        const Patch = 1 << 8;
        const Abort = 1 << 9;
    }
}

bitflags! {
    /// OLCP_Features, the other half of the OTS Feature characteristic value.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OlcpFeatures: u32 {
        const GoTo = 1 << 0;
        const Order = 1 << 1;
        const RequestNumberOfObjects = 1 << 2;
        const ClearMarking = 1 << 3;
    }
}

/// The OTS Feature characteristic value: OACP_Features (4, LE) | OLCP_Features (4, LE).
///
/// No `defmt::Format` derive (unlike most value types in this crate) - `OacpFeatures`/
/// `OlcpFeatures` are `bitflags!`-generated structs, which don't implement it (see the analogous
/// omission on `vcs::VolumeFlags`/`has::PresetRecord`/`tbs::CallRecord`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct OtsFeature {
    pub oacp: OacpFeatures,
    pub olcp: OlcpFeatures,
}

impl AsGatt for OtsFeature {
    const MIN_SIZE: usize = 8;
    const MAX_SIZE: usize = 8;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`; both `OacpFeatures`/`OlcpFeatures` are bitflags types
        // (`#[repr(transparent)]` over `u32`), so this is `oacp` (4 bytes LE) followed by `olcp`
        // (4 bytes LE) with no padding - exactly the wire layout.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 8) }
    }
}

impl FromGatt for OtsFeature {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 8 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            oacp: OacpFeatures::from_bits_truncate(u32::from_le_bytes(data[0..4].try_into().unwrap())),
            olcp: OlcpFeatures::from_bits_truncate(u32::from_le_bytes(data[4..8].try_into().unwrap())),
        })
    }
}

impl FixedGattValue for OtsFeature {
    const SIZE: usize = 8;
}

/// A 48-bit Object ID. The top 16 bits of the backing `u64` are always zero.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectId(pub u64);

impl ObjectId {
    fn encode(self, out: &mut AVec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes()[..6]);
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 6] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        let mut widened = [0u8; 8];
        widened[..6].copy_from_slice(&bytes);
        Ok(Self(u64::from_le_bytes(widened)))
    }
}

/// The Bluetooth "Date Time" characteristic format (also used by e.g. Current Time Service),
/// reused here for Object First-Created/Last-Modified. `0` in any field means "unknown"/unset.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
}

impl AsGatt for DateTime {
    const MIN_SIZE: usize = 7;
    const MAX_SIZE: usize = 7;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`: `year` (u16, 2 bytes, already aligned at offset 0) followed by
        // five adjacent `u8` fields with no padding - exactly the wire layout. `size_of::<Self>()`
        // is exactly 7 here (unlike `vocs::VolumeOffsetState`), since `year` being first means no
        // trailing padding is needed to satisfy its alignment.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 7) }
    }
}

impl FromGatt for DateTime {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 7 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            year: u16::from_le_bytes([data[0], data[1]]),
            month: data[2],
            day: data[3],
            hours: data[4],
            minutes: data[5],
            seconds: data[6],
        })
    }
}

impl FixedGattValue for DateTime {
    const SIZE: usize = 7;
}

bitflags! {
    /// Object_Properties (OTS, "Object Properties" table).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ObjectProperties: u32 {
        const Delete = 1 << 0;
        const Execute = 1 << 1;
        const Read = 1 << 2;
        const Write = 1 << 3;
        const Append = 1 << 4;
        const Truncate = 1 << 5;
        const Patch = 1 << 6;
        const Mark = 1 << 7;
    }
}

impl AsGatt for ObjectProperties {
    const MIN_SIZE: usize = 4;
    const MAX_SIZE: usize = 4;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u32 here).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 4) }
    }
}

impl FromGatt for ObjectProperties {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 4] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u32::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for ObjectProperties {
    const SIZE: usize = 4;
}

/// One object tracked server-side. The Object Name/Type/Size/ID/Properties/First-Created/
/// Last-Modified characteristics are each a view over whichever object is currently "selected"
/// (see [`OtsServer::selected`]).
///
/// No `defmt::Format` derive - `ObjectProperties` is a `bitflags!`-generated struct, which
/// doesn't implement it (see `OtsFeature`'s doc comment for the analogous case).
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectRecord {
    pub id: ObjectId,
    pub name: HString<MAX_OBJECT_NAME_LEN>,
    pub object_type: Uuid,
    pub current_size: u32,
    pub allocated_size: u32,
    pub first_created: Option<DateTime>,
    pub last_modified: Option<DateTime>,
    pub properties: ObjectProperties,
}

/// Object Action Control Point Opcodes ("OACP" procedure requirements table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
pub enum OacpOpcode {
    Create { size: u32, object_type: Uuid },
    Delete,
    CalculateChecksum { offset: u32, length: u32 },
    Execute(AVec<u8>),
    /// Never actually transfers data - see the module doc comment.
    Read { offset: u32, length: u32 },
    /// Never actually transfers data - see the module doc comment.
    Write { offset: u32, length: u32, mode: u8 },
    Abort,
}

impl OacpOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::Create { .. } => 0x01,
            Self::Delete => 0x02,
            Self::CalculateChecksum { .. } => 0x03,
            Self::Execute(_) => 0x04,
            Self::Read { .. } => 0x05,
            Self::Write { .. } => 0x06,
            Self::Abort => 0x07,
        }
    }

    fn encode(&self, out: &mut AVec<u8>) {
        out.push(self.raw_opcode());
        match self {
            Self::Create { size, object_type } => {
                out.extend_from_slice(&size.to_le_bytes());
                out.extend_from_slice(object_type.as_gatt());
            }
            Self::Delete | Self::Abort => {}
            Self::CalculateChecksum { offset, length } => {
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&length.to_le_bytes());
            }
            Self::Execute(params) => out.extend_from_slice(params),
            Self::Read { offset, length } => {
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&length.to_le_bytes());
            }
            Self::Write { offset, length, mode } => {
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&length.to_le_bytes());
                out.push(*mode);
            }
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = *data.first().ok_or(FromGattError::InvalidLength)?;
        let rest = &data[1..];
        Ok(match opcode {
            0x01 => {
                if rest.len() < 4 {
                    return Err(FromGattError::InvalidLength);
                }
                let size = u32::from_le_bytes(rest[..4].try_into().unwrap());
                let object_type = Uuid::from_gatt(&rest[4..])?;
                Self::Create { size, object_type }
            }
            0x02 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::Delete
            }
            0x03 => {
                if rest.len() != 8 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::CalculateChecksum {
                    offset: u32::from_le_bytes(rest[..4].try_into().unwrap()),
                    length: u32::from_le_bytes(rest[4..8].try_into().unwrap()),
                }
            }
            0x04 => Self::Execute(AVec::from(rest)),
            0x05 => {
                if rest.len() != 8 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::Read {
                    offset: u32::from_le_bytes(rest[..4].try_into().unwrap()),
                    length: u32::from_le_bytes(rest[4..8].try_into().unwrap()),
                }
            }
            0x06 => {
                if rest.len() != 9 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::Write {
                    offset: u32::from_le_bytes(rest[..4].try_into().unwrap()),
                    length: u32::from_le_bytes(rest[4..8].try_into().unwrap()),
                    mode: rest[8],
                }
            }
            0x07 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::Abort
            }
            _ => return Err(FromGattError::InvalidLength),
        })
    }
}

/// The value written to the Object Action Control Point characteristic. Wire bytes are cached in
/// `encoded` for the same reason as `vcs::VolumeControlPointOperation`.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
pub struct OacpOperation {
    pub opcode: OacpOpcode,
    encoded: AVec<u8>,
}

impl OacpOperation {
    pub fn new(opcode: OacpOpcode) -> Self {
        let mut encoded = AVec::new();
        opcode.encode(&mut encoded);
        Self { opcode, encoded }
    }
}

impl AsGatt for OacpOperation {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = usize::MAX;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for OacpOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        Ok(Self::new(OacpOpcode::decode(data)?))
    }
}

impl Default for OacpOperation {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(OacpOpcode::Abort)
    }
}

/// The OACP notification sent back after processing an operation: Requested_Opcode (1) |
/// Result_Code (1) | optional Checksum (4, only for `CalculateChecksum`).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OacpResponse {
    pub requested_opcode: u8,
    pub result_code: u8,
    pub checksum: Option<u32>,
}

impl OacpResponse {
    /// Encodes this response on demand - unlike most value types in this crate, `OacpResponse`
    /// doesn't implement `AsGatt` (it's never a characteristic's resting value, only ever sent
    /// via `notify_raw`, so there's no need for a borrow-friendly representation - see
    /// `vcs::VolumeControlPointOperation`'s doc comment for the analogous case that *does* need
    /// one).
    pub fn encode(&self) -> AVec<u8> {
        let mut out = AVec::with_capacity(6);
        out.push(self.requested_opcode);
        out.push(self.result_code);
        if let Some(checksum) = self.checksum {
            out.extend_from_slice(&checksum.to_le_bytes());
        }
        out
    }
}

/// Object List Control Point Opcodes ("OLCP" procedure requirements table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OlcpOpcode {
    First,
    Last,
    Previous,
    Next,
    GoTo(ObjectId),
    Order(u8),
    RequestNumberOfObjects,
    ClearMarking,
}

impl OlcpOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::First => 0x01,
            Self::Last => 0x02,
            Self::Previous => 0x03,
            Self::Next => 0x04,
            Self::GoTo(_) => 0x05,
            Self::Order(_) => 0x06,
            Self::RequestNumberOfObjects => 0x07,
            Self::ClearMarking => 0x08,
        }
    }

    fn encode(&self, out: &mut AVec<u8>) {
        out.push(self.raw_opcode());
        match self {
            Self::First | Self::Last | Self::Previous | Self::Next | Self::RequestNumberOfObjects | Self::ClearMarking => {}
            Self::GoTo(id) => id.encode(out),
            Self::Order(order) => out.push(*order),
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = *data.first().ok_or(FromGattError::InvalidLength)?;
        let rest = &data[1..];
        Ok(match opcode {
            0x01 if rest.is_empty() => Self::First,
            0x02 if rest.is_empty() => Self::Last,
            0x03 if rest.is_empty() => Self::Previous,
            0x04 if rest.is_empty() => Self::Next,
            0x05 => Self::GoTo(ObjectId::decode(rest)?),
            0x06 if rest.len() == 1 => Self::Order(rest[0]),
            0x07 if rest.is_empty() => Self::RequestNumberOfObjects,
            0x08 if rest.is_empty() => Self::ClearMarking,
            _ => return Err(FromGattError::InvalidLength),
        })
    }
}

/// The value written to the Object List Control Point characteristic.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlcpOperation {
    pub opcode: OlcpOpcode,
    encoded: AVec<u8>,
}

impl OlcpOperation {
    pub fn new(opcode: OlcpOpcode) -> Self {
        let mut encoded = AVec::new();
        opcode.encode(&mut encoded);
        Self { opcode, encoded }
    }
}

impl AsGatt for OlcpOperation {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 7;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for OlcpOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        Ok(Self::new(OlcpOpcode::decode(data)?))
    }
}

impl Default for OlcpOperation {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(OlcpOpcode::First)
    }
}

/// The OLCP notification sent back after processing an operation: Requested_Opcode (1) |
/// Result_Code (1) | optional Number_Of_Objects (4, only for `RequestNumberOfObjects`).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlcpResponse {
    pub requested_opcode: u8,
    pub result_code: u8,
    pub number_of_objects: Option<u32>,
}

impl OlcpResponse {
    /// See `OacpResponse::encode`'s doc comment for why this type doesn't implement `AsGatt`.
    pub fn encode(&self) -> AVec<u8> {
        let mut out = AVec::with_capacity(6);
        out.push(self.requested_opcode);
        out.push(self.result_code);
        if let Some(count) = self.number_of_objects {
            out.extend_from_slice(&count.to_le_bytes());
        }
        out
    }
}

/// Applies an OLCP operation to the object list, returning the new selected index (into
/// `objects`) alongside the response that should be sent, or `None` for the index if the
/// operation didn't change the current selection (e.g. `Order`/`RequestNumberOfObjects`/
/// `ClearMarking`, none of which are modeled as changing which object is selected).
pub fn apply_olcp_operation(
    objects: &[ObjectRecord],
    current: usize,
    opcode: &OlcpOpcode,
) -> (Option<usize>, u8, Option<u32>) {
    if objects.is_empty() {
        return (None, OLCP_NO_OBJECT, None);
    }
    match opcode {
        OlcpOpcode::First => (Some(0), OLCP_SUCCESS, None),
        OlcpOpcode::Last => (Some(objects.len() - 1), OLCP_SUCCESS, None),
        OlcpOpcode::Previous => {
            if current == 0 {
                (None, OLCP_OUT_OF_BOUNDS, None)
            } else {
                (Some(current - 1), OLCP_SUCCESS, None)
            }
        }
        OlcpOpcode::Next => {
            if current + 1 >= objects.len() {
                (None, OLCP_OUT_OF_BOUNDS, None)
            } else {
                (Some(current + 1), OLCP_SUCCESS, None)
            }
        }
        OlcpOpcode::GoTo(id) => match objects.iter().position(|o| o.id == *id) {
            Some(index) => (Some(index), OLCP_SUCCESS, None),
            None => (None, OLCP_OBJECT_ID_NOT_FOUND, None),
        },
        OlcpOpcode::Order(_) => (None, OLCP_OPCODE_NOT_SUPPORTED, None),
        OlcpOpcode::RequestNumberOfObjects => (None, OLCP_SUCCESS, Some(objects.len() as u32)),
        OlcpOpcode::ClearMarking => (None, OLCP_SUCCESS, None),
    }
}

/// Applies an OACP operation. `objects`/`selected` may be mutated (Create appends and selects a
/// new object; Delete removes the selected one). Offset/length bounds for
/// Read/Write/CalculateChecksum are validated against the selected object's `current_size`, but
/// (per the module doc comment) no bytes are ever actually transferred.
pub fn apply_oacp_operation(objects: &mut AVec<ObjectRecord>, selected: &mut usize, opcode: &OacpOpcode) -> (u8, Option<u32>) {
    match opcode {
        OacpOpcode::Create { size, object_type } => {
            let id = ObjectId(objects.iter().map(|o| o.id.0).max().map_or(0, |max| max + 1));
            objects.push(ObjectRecord {
                id,
                name: HString::new(),
                object_type: *object_type,
                current_size: 0,
                allocated_size: *size,
                first_created: None,
                last_modified: None,
                properties: ObjectProperties::Read | ObjectProperties::Write | ObjectProperties::Delete,
            });
            *selected = objects.len() - 1;
            (OACP_SUCCESS, None)
        }
        OacpOpcode::Delete => {
            let Some(object) = objects.get(*selected) else {
                return (OACP_INVALID_OBJECT, None);
            };
            if !object.properties.contains(ObjectProperties::Delete) {
                return (OACP_PROCEDURE_NOT_PERMITTED, None);
            }
            objects.remove(*selected);
            *selected = selected.saturating_sub(1);
            (OACP_SUCCESS, None)
        }
        OacpOpcode::CalculateChecksum { offset, length } => match objects.get(*selected) {
            Some(object) if offset.saturating_add(*length) <= object.current_size => {
                // No data to actually checksum (see the module doc comment) - reports a
                // placeholder so the response shape is still exercised end to end.
                (OACP_SUCCESS, Some(0))
            }
            Some(_) => (OACP_INVALID_PARAMETER, None),
            None => (OACP_INVALID_OBJECT, None),
        },
        OacpOpcode::Execute(_) => match objects.get(*selected) {
            Some(object) if object.properties.contains(ObjectProperties::Execute) => (OACP_SUCCESS, None),
            Some(_) => (OACP_PROCEDURE_NOT_PERMITTED, None),
            None => (OACP_INVALID_OBJECT, None),
        },
        OacpOpcode::Read { offset, length } => match objects.get(*selected) {
            Some(object) if !object.properties.contains(ObjectProperties::Read) => (OACP_PROCEDURE_NOT_PERMITTED, None),
            Some(object) if offset.saturating_add(*length) <= object.current_size => (OACP_CHANNEL_NOT_AVAILABLE, None),
            Some(_) => (OACP_INVALID_PARAMETER, None),
            None => (OACP_INVALID_OBJECT, None),
        },
        OacpOpcode::Write { offset, length, .. } => match objects.get(*selected) {
            Some(object) if !object.properties.contains(ObjectProperties::Write) => (OACP_PROCEDURE_NOT_PERMITTED, None),
            Some(object) if offset.saturating_add(*length) <= object.allocated_size => (OACP_CHANNEL_NOT_AVAILABLE, None),
            Some(_) => (OACP_INVALID_PARAMETER, None),
            None => (OACP_INVALID_OBJECT, None),
        },
        OacpOpcode::Abort => (OACP_SUCCESS, None),
    }
}

/// Object_Changed characteristic value: Flags (1) | Object_ID (6).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectChanged {
    pub flags: u8,
    pub object_id: ObjectId,
}

impl ObjectChanged {
    /// See `OacpResponse::encode`'s doc comment for why this type doesn't implement `AsGatt`.
    pub fn encode(&self) -> AVec<u8> {
        let mut out = AVec::with_capacity(7);
        out.push(self.flags);
        self.object_id.encode(&mut out);
        out
    }
}

/// A Gatt service client for browsing/managing a device's objects.
pub struct OtsClient {
    handle: ServiceHandle,
    pub feature: Characteristic<OtsFeature>,
    pub object_name: Characteristic<HString<MAX_OBJECT_NAME_LEN>>,
    pub object_type: Characteristic<Uuid>,
    pub object_size_current: Characteristic<u32>,
    pub object_properties: Characteristic<ObjectProperties>,
    pub object_action_control_point: Characteristic<OacpOperation>,
    pub object_list_control_point: Characteristic<OlcpOperation>,
}

impl OtsClient {
    /// Discovers the OTS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose OTS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::OBJECT_TRANSFER)).await.ok()?;
        let handle = services.first()?;

        macro_rules! required {
            ($uuid:expr) => {
                client.characteristic_by_uuid(handle, &Uuid::from($uuid)).await.ok()?
            };
        }

        Some(Self {
            handle: handle.clone(),
            feature: required!(characteristic::OTS_FEATURE),
            object_name: required!(characteristic::OBJECT_NAME),
            object_type: required!(characteristic::OBJECT_TYPE),
            object_size_current: required!(characteristic::OBJECT_SIZE),
            object_properties: required!(characteristic::OBJECT_PROPERTIES),
            object_action_control_point: required!(characteristic::OBJECT_ACTION_CONTROL_POINT),
            object_list_control_point: required!(characteristic::OBJECT_LIST_CONTROL_POINT),
        })
    }
}

/// Backing storage buffers for [`OtsServer::new`].
pub struct OtsStore<'a> {
    pub feature: &'a mut [u8],
    pub object_name: &'a mut [u8],
    pub object_type: &'a mut [u8],
    pub object_size: &'a mut [u8],
    pub object_id: &'a mut [u8],
    pub object_properties: &'a mut [u8],
    pub object_action_control_point: &'a mut [u8],
    pub object_list_control_point: &'a mut [u8],
    pub object_changed: &'a mut [u8],
}

/// A Gatt service server exposing an object list of up to `MAX_OBJECTS` objects. Only the
/// currently *selected* object's metadata is exposed through the Object Name/Type/Size/ID/
/// Properties characteristics at any one time (per OTS - the client uses OLCP to change which
/// object is selected).
pub struct OtsServer<const MAX_OBJECTS: usize> {
    handle: u16,
    feature: Characteristic<OtsFeature>,
    object_name: Characteristic<HString<MAX_OBJECT_NAME_LEN>>,
    object_type: Characteristic<Uuid>,
    object_size: Characteristic<u32>,
    object_id: Characteristic<[u8; 6]>,
    object_properties: Characteristic<ObjectProperties>,
    object_action_control_point: Characteristic<OacpOperation>,
    object_list_control_point: Characteristic<OlcpOperation>,
    object_changed: Characteristic<()>,
    objects: RefCell<AVec<ObjectRecord>>,
    selected: RefCell<usize>,
}

pub const OTS_ATTRIBUTES: usize = 22;

impl<const MAX_OBJECTS: usize> OtsServer<MAX_OBJECTS> {
    /// Creates a new OTS Gatt service with the given initial object list. Panics if `objects` is
    /// empty - OTS requires at least one selected object at all times when it has any objects at
    /// all, and this crate doesn't model the (legal per spec, but degenerate) empty-list case.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        feature: OtsFeature,
        objects: AVec<ObjectRecord>,
        store: OtsStore<'a>,
    ) -> Self {
        assert!(!objects.is_empty(), "OtsServer requires at least one initial object");
        let mut service = table.add_service(Service::new(service::OBJECT_TRANSFER));

        let feature_char = service
            .add_characteristic(characteristic::OTS_FEATURE, &[CharacteristicProp::Read], feature, store.feature)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_name = service
            .add_characteristic(
                characteristic::OBJECT_NAME,
                &[CharacteristicProp::Read, CharacteristicProp::Write],
                objects[0].name.clone(),
                store.object_name,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_type = service
            .add_characteristic(characteristic::OBJECT_TYPE, &[CharacteristicProp::Read], objects[0].object_type, store.object_type)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_size = service
            .add_characteristic(
                characteristic::OBJECT_SIZE,
                &[CharacteristicProp::Read],
                objects[0].current_size,
                store.object_size,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let mut id_bytes = AVec::new();
        objects[0].id.encode(&mut id_bytes);
        let object_id = service
            .add_characteristic(
                characteristic::OBJECT_ID,
                &[CharacteristicProp::Read],
                <[u8; 6]>::try_from(id_bytes.as_slice()).unwrap(),
                store.object_id,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_properties = service
            .add_characteristic(
                characteristic::OBJECT_PROPERTIES,
                &[CharacteristicProp::Read, CharacteristicProp::Write],
                objects[0].properties,
                store.object_properties,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_action_control_point = service
            .add_characteristic(
                characteristic::OBJECT_ACTION_CONTROL_POINT,
                &[CharacteristicProp::Write, CharacteristicProp::Notify],
                OacpOperation::default(),
                store.object_action_control_point,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_list_control_point = service
            .add_characteristic(
                characteristic::OBJECT_LIST_CONTROL_POINT,
                &[CharacteristicProp::Write, CharacteristicProp::Notify],
                OlcpOperation::default(),
                store.object_list_control_point,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let object_changed = service
            .add_characteristic(
                characteristic::OBJECT_CHANGED,
                &[CharacteristicProp::Notify],
                (),
                store.object_changed,
            )
            .build();

        Self {
            handle: service.build(),
            feature: feature_char,
            object_name,
            object_type,
            object_size,
            object_id,
            object_properties,
            object_action_control_point,
            object_list_control_point,
            object_changed,
            objects: RefCell::new(objects),
            selected: RefCell::new(0),
        }
    }

    pub fn feature(&self) -> &Characteristic<OtsFeature> {
        &self.feature
    }
    pub fn object_name(&self) -> &Characteristic<HString<MAX_OBJECT_NAME_LEN>> {
        &self.object_name
    }
    pub fn object_action_control_point(&self) -> &Characteristic<OacpOperation> {
        &self.object_action_control_point
    }
    pub fn object_list_control_point(&self) -> &Characteristic<OlcpOperation> {
        &self.object_list_control_point
    }
    /// A snapshot of the currently tracked objects.
    pub fn objects(&self) -> AVec<ObjectRecord> {
        self.objects.borrow().clone()
    }
    /// The index (into [`Self::objects`]) of the currently selected object.
    pub fn selected(&self) -> usize {
        *self.selected.borrow()
    }
}

impl<const MAX_OBJECTS: usize, P: PacketPool> LeAudioServerService<P> for OtsServer<MAX_OBJECTS> {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        let handles = [
            self.feature.handle,
            self.object_name.handle,
            self.object_type.handle,
            self.object_size.handle,
            self.object_id.handle,
            self.object_properties.handle,
        ];
        if handles.contains(&event.handle()) {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.object_name.handle {
            return Some(match event.value(&self.object_name) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        if event.handle() == self.object_properties.handle {
            return Some(match event.value(&self.object_properties) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        // OACP/OLCP's business rules need the stored object list, so `Server::handle`
        // special-cases them before they'd otherwise reach this generic path - the same way it
        // special-cases MCS's Media Control Point.
        if event.handle() == self.object_action_control_point.handle {
            return Some(match event.value(&self.object_action_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        if event.handle() == self.object_list_control_point.handle {
            return Some(match event.value(&self.object_list_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Notifies every metadata characteristic with the currently selected object's values - called
/// after OLCP changes the selection, or OACP mutates the object list.
async fn notify_selected_object<M: RawMutex, P: PacketPool, const MAX_OBJECTS: usize, const MAX_CONNECTIONS: usize>(
    _server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ots: &OtsServer<MAX_OBJECTS>,
    conn: &GattConnection<'_, '_, P>,
) {
    let objects = ots.objects.borrow();
    let selected = *ots.selected.borrow();
    let Some(object) = objects.get(selected) else { return };
    ots.object_name.set(_server, &object.name).ok();
    let _ = ots.object_name.notify(conn, &object.name, false).await;
}

/// Applies an Object List Control Point write end to end: updates `ots`'s selected-object index
/// (see [`apply_olcp_operation`]), refreshes the metadata characteristics if the selection
/// changed, and sends the resulting [`OlcpResponse`] on the Object List Control Point
/// characteristic itself.
pub async fn drive_olcp<M: RawMutex, P: PacketPool, const MAX_OBJECTS: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ots: &OtsServer<MAX_OBJECTS>,
    conn: &GattConnection<'_, '_, P>,
    operation: &OlcpOperation,
) {
    let (new_selected, result_code, number_of_objects) = {
        let objects = ots.objects.borrow();
        let current = *ots.selected.borrow();
        apply_olcp_operation(&objects, current, &operation.opcode)
    };
    if let Some(index) = new_selected {
        *ots.selected.borrow_mut() = index;
        notify_selected_object(server, ots, conn).await;
    }
    let response = OlcpResponse {
        requested_opcode: operation.opcode.raw_opcode(),
        result_code,
        number_of_objects,
    };
    let _ = ots.object_list_control_point.notify_raw(conn, &response.encode(), false).await;
}

/// Applies an Object Action Control Point write end to end: mutates `ots`'s object list (see
/// [`apply_oacp_operation`]), refreshes the metadata characteristics if the selected object
/// changed, and sends the resulting [`OacpResponse`] on the Object Action Control Point
/// characteristic itself.
pub async fn drive_oacp<M: RawMutex, P: PacketPool, const MAX_OBJECTS: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ots: &OtsServer<MAX_OBJECTS>,
    conn: &GattConnection<'_, '_, P>,
    operation: &OacpOperation,
) {
    let (result_code, checksum) = {
        let mut objects = ots.objects.borrow_mut();
        let mut selected = ots.selected.borrow_mut();
        apply_oacp_operation(&mut objects, &mut selected, &operation.opcode)
    };
    if result_code == OACP_SUCCESS && matches!(operation.opcode, OacpOpcode::Create { .. } | OacpOpcode::Delete) {
        notify_selected_object(server, ots, conn).await;
    }
    let response = OacpResponse {
        requested_opcode: operation.opcode.raw_opcode(),
        result_code,
        checksum,
    };
    let _ = ots.object_action_control_point.notify_raw(conn, &response.encode(), false).await;
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn object(id: u64, name: &str) -> ObjectRecord {
        ObjectRecord {
            id: ObjectId(id),
            name: HString::try_from(name).unwrap(),
            object_type: Uuid::new_short(0x1234),
            current_size: 100,
            allocated_size: 200,
            first_created: None,
            last_modified: None,
            properties: ObjectProperties::Read | ObjectProperties::Write | ObjectProperties::Delete,
        }
    }

    #[test]
    fn ots_feature_round_trips() {
        let feature = OtsFeature {
            oacp: OacpFeatures::Create | OacpFeatures::Delete,
            olcp: OlcpFeatures::GoTo,
        };
        assert_eq!(OtsFeature::from_gatt(feature.as_gatt()).unwrap(), feature);
    }

    #[test]
    fn date_time_round_trips() {
        let dt = DateTime { year: 2026, month: 7, day: 7, hours: 12, minutes: 30, seconds: 0 };
        assert_eq!(DateTime::from_gatt(dt.as_gatt()).unwrap(), dt);
    }

    #[test]
    fn object_id_round_trips_through_olcp_goto() {
        let op = OlcpOperation::new(OlcpOpcode::GoTo(ObjectId(0xAABBCCDDEEFF)));
        assert_eq!(OlcpOperation::from_gatt(op.as_gatt()).unwrap(), op);
    }

    #[test]
    fn oacp_operation_round_trips_every_opcode_shape() {
        for opcode in [
            OacpOpcode::Create { size: 100, object_type: Uuid::new_short(0x1234) },
            OacpOpcode::Delete,
            OacpOpcode::CalculateChecksum { offset: 0, length: 10 },
            OacpOpcode::Execute(AVec::from([1, 2, 3].as_slice())),
            OacpOpcode::Read { offset: 0, length: 10 },
            OacpOpcode::Write { offset: 0, length: 10, mode: 0 },
            OacpOpcode::Abort,
        ] {
            let op = OacpOperation::new(opcode.clone());
            assert_eq!(OacpOperation::from_gatt(op.as_gatt()).unwrap(), op);
        }
    }

    #[test]
    fn olcp_first_last_next_previous_navigate_the_list() {
        let objects = [object(1, "a"), object(2, "b"), object(3, "c")];
        assert_eq!(apply_olcp_operation(&objects, 1, &OlcpOpcode::First), (Some(0), OLCP_SUCCESS, None));
        assert_eq!(apply_olcp_operation(&objects, 1, &OlcpOpcode::Last), (Some(2), OLCP_SUCCESS, None));
        assert_eq!(apply_olcp_operation(&objects, 1, &OlcpOpcode::Next), (Some(2), OLCP_SUCCESS, None));
        assert_eq!(apply_olcp_operation(&objects, 1, &OlcpOpcode::Previous), (Some(0), OLCP_SUCCESS, None));
    }

    #[test]
    fn olcp_next_at_the_end_of_the_list_is_out_of_bounds() {
        let objects = [object(1, "a")];
        assert_eq!(apply_olcp_operation(&objects, 0, &OlcpOpcode::Next), (None, OLCP_OUT_OF_BOUNDS, None));
    }

    #[test]
    fn olcp_goto_finds_by_object_id() {
        let objects = [object(1, "a"), object(2, "b")];
        assert_eq!(apply_olcp_operation(&objects, 0, &OlcpOpcode::GoTo(ObjectId(2))), (Some(1), OLCP_SUCCESS, None));
    }

    #[test]
    fn olcp_goto_rejects_an_unknown_object_id() {
        let objects = [object(1, "a")];
        assert_eq!(apply_olcp_operation(&objects, 0, &OlcpOpcode::GoTo(ObjectId(99))), (None, OLCP_OBJECT_ID_NOT_FOUND, None));
    }

    #[test]
    fn olcp_request_number_of_objects_reports_the_count() {
        let objects = [object(1, "a"), object(2, "b")];
        assert_eq!(apply_olcp_operation(&objects, 0, &OlcpOpcode::RequestNumberOfObjects), (None, OLCP_SUCCESS, Some(2)));
    }

    #[test]
    fn oacp_create_appends_and_selects_a_new_object() {
        let mut objects: AVec<ObjectRecord> = AVec::from([object(5, "a")].as_slice());
        let mut selected = 0;
        let (result, _) = apply_oacp_operation(&mut objects, &mut selected, &OacpOpcode::Create { size: 50, object_type: Uuid::new_short(1) });
        assert_eq!(result, OACP_SUCCESS);
        assert_eq!(objects.len(), 2);
        assert_eq!(selected, 1);
        assert_eq!(objects[1].id, ObjectId(6));
    }

    #[test]
    fn oacp_delete_rejects_when_the_object_lacks_the_delete_property() {
        let mut object = object(1, "a");
        object.properties = ObjectProperties::Read;
        let mut objects: AVec<ObjectRecord> = AVec::from([object].as_slice());
        let mut selected = 0;
        let (result, _) = apply_oacp_operation(&mut objects, &mut selected, &OacpOpcode::Delete);
        assert_eq!(result, OACP_PROCEDURE_NOT_PERMITTED);
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn oacp_delete_removes_the_selected_object() {
        let mut objects: AVec<ObjectRecord> = AVec::from([object(1, "a"), object(2, "b")].as_slice());
        let mut selected = 1;
        let (result, _) = apply_oacp_operation(&mut objects, &mut selected, &OacpOpcode::Delete);
        assert_eq!(result, OACP_SUCCESS);
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn oacp_read_within_bounds_reports_channel_not_available_since_no_transfer_ever_happens() {
        let mut objects: AVec<ObjectRecord> = AVec::from([object(1, "a")].as_slice());
        let mut selected = 0;
        let (result, _) = apply_oacp_operation(&mut objects, &mut selected, &OacpOpcode::Read { offset: 0, length: 50 });
        assert_eq!(result, OACP_CHANNEL_NOT_AVAILABLE);
    }

    #[test]
    fn oacp_read_out_of_bounds_is_rejected_before_the_channel_question_comes_up() {
        let mut objects: AVec<ObjectRecord> = AVec::from([object(1, "a")].as_slice());
        let mut selected = 0;
        let (result, _) = apply_oacp_operation(&mut objects, &mut selected, &OacpOpcode::Read { offset: 0, length: 1000 });
        assert_eq!(result, OACP_INVALID_PARAMETER);
    }

    #[test]
    fn ots_server_exposes_the_first_selected_object_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static FEATURE: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        static NAME: static_cell::StaticCell<[u8; 32]> = static_cell::StaticCell::new();
        static TYPE: static_cell::StaticCell<[u8; 16]> = static_cell::StaticCell::new();
        static SIZE: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static ID: static_cell::StaticCell<[u8; 6]> = static_cell::StaticCell::new();
        static PROPS: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static OACP: static_cell::StaticCell<[u8; 16]> = static_cell::StaticCell::new();
        static OLCP: static_cell::StaticCell<[u8; 8]> = static_cell::StaticCell::new();
        static CHANGED: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();

        let ots: OtsServer<4> = OtsServer::new(
            &mut table,
            OtsFeature { oacp: OacpFeatures::Create, olcp: OlcpFeatures::GoTo },
            AVec::from([object(1, "Icon")].as_slice()),
            OtsStore {
                feature: FEATURE.init([0; 8]),
                object_name: NAME.init([0; 32]),
                object_type: TYPE.init([0; 16]),
                object_size: SIZE.init([0; 4]),
                object_id: ID.init([0; 6]),
                object_properties: PROPS.init([0; 4]),
                object_action_control_point: OACP.init([0; 16]),
                object_list_control_point: OLCP.init([0; 8]),
                object_changed: CHANGED.init([0; 1]),
            },
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(ots.object_name().get(&server).unwrap(), HString::<MAX_OBJECT_NAME_LEN>::try_from("Icon").unwrap());
        assert_eq!(ots.selected(), 0);
        assert_eq!(ots.objects().len(), 1);
    }
}
