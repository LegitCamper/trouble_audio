//! ## Hearing Access Service
//!
//! The Hearing Access Service (HAS) lets a client discover and switch between a hearing aid's
//! stored presets (e.g. "Outdoor", "Restaurant", "TV"). Bluetooth HAS v1.0.
//!
//! Opcode numbering and application error codes are reconstructed from public documentation, not
//! cross-checked against the official Bluetooth SIG assigned-numbers PDF - verify before relying
//! on the exact values for certification. The Preset Control Point's response is sent via
//! `notify_raw` here (like this crate's other control points - ASE/Volume/Media Control Point),
//! though the real HAS spec specifies "Indicate" (confirmed delivery) for it.
//!
//! "Synchronized Locally" opcode variants (`SetActivePresetSynchronizedLocally` etc, meant for a
//! coordinated set member propagating a preset change to its peer without going back out over
//! the air) are accepted but behave identically to their non-synchronized counterparts, since
//! this crate models a single active connection with no peer to synchronize with (see the
//! analogous note atop ascs.rs).

use bitflags::bitflags;
use bt_hci::uuid::{characteristic, service};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::{String as HString, Vec as HVec};
use trouble_host::{
    gatt::{GattConnection, ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{LeAudioServerService, MAX_SERVICES};

/// Preset names are at most 40 octets (HAS 4.2).
pub const MAX_PRESET_NAME_LEN: usize = 40;

/// Application error codes (HAS). See the module doc comment's disclaimer.
pub const INVALID_OPCODE: AttErrorCode = AttErrorCode::new(0x80);
pub const WRITE_NAME_NOT_ALLOWED: AttErrorCode = AttErrorCode::new(0x81);
pub const PRESET_NOT_FOUND: AttErrorCode = AttErrorCode::new(0x82);
pub const PRESET_NOT_AVAILABLE: AttErrorCode = AttErrorCode::new(0x83);
pub const INVALID_PARAMETERS_LENGTH: AttErrorCode = AttErrorCode::new(0x84);

/// Hearing Aid Type (HAS, "Hearing Aid Features" table, bits 0-1).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HearingAidType {
    #[default]
    Monaural = 0x00,
    Binaural = 0x01,
    Banded = 0x02,
}

impl TryFrom<u8> for HearingAidType {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Monaural),
            0x01 => Ok(Self::Binaural),
            0x02 => Ok(Self::Banded),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// The Hearing Aid Features characteristic value (HAS): a 2-bit [`HearingAidType`] plus four
/// capability flags, packed into one byte.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HearingAidFeatures {
    pub hearing_aid_type: HearingAidType,
    pub preset_synchronization_supported: bool,
    pub independent_presets: bool,
    pub dynamic_presets: bool,
    pub writable_preset_names: bool,
}

impl AsGatt for HearingAidFeatures {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // `IDENTITY[b] == b` for every possible byte - a roundabout way to hand back a `'static`
        // one-byte slice for an arbitrary computed value without allocating (this type packs
        // several bitfields into one byte, so there's no single struct field to borrow from
        // directly, unlike this crate's other fixed-size GATT values).
        const IDENTITY: [u8; 256] = {
            let mut table = [0u8; 256];
            let mut i = 0;
            while i < 256 {
                table[i] = i as u8;
                i += 1;
            }
            table
        };
        let byte = (self.hearing_aid_type as u8)
            | (self.preset_synchronization_supported as u8) << 2
            | (self.independent_presets as u8) << 3
            | (self.dynamic_presets as u8) << 4
            | (self.writable_preset_names as u8) << 5;
        &IDENTITY[byte as usize..=byte as usize]
    }
}

impl FromGatt for HearingAidFeatures {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        let byte = data[0];
        Ok(Self {
            hearing_aid_type: HearingAidType::try_from(byte & 0b11)?,
            preset_synchronization_supported: byte & (1 << 2) != 0,
            independent_presets: byte & (1 << 3) != 0,
            dynamic_presets: byte & (1 << 4) != 0,
            writable_preset_names: byte & (1 << 5) != 0,
        })
    }
}

impl FixedGattValue for HearingAidFeatures {
    const SIZE: usize = 1;
}

bitflags! {
    /// Preset_Properties (HAS, "Preset Record" format).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PresetProperties: u8 {
        const Writable = 1 << 0;
        const Available = 1 << 1;
    }
}

/// One stored hearing aid preset (HAS "Preset Record" format): Index (1) | Properties (1) | Name
/// (variable, up to [`MAX_PRESET_NAME_LEN`]).
///
/// No `defmt::Format` derive (unlike most value types in this crate) - `PresetProperties` is a
/// `bitflags!`-generated struct, which doesn't implement it (see the analogous omission on
/// `vcs::VolumeFlags`), so a derive here would fail to compile under the `defmt` feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetRecord {
    pub index: u8,
    pub properties: PresetProperties,
    pub name: HString<MAX_PRESET_NAME_LEN>,
}

/// Preset Control Point Opcodes (HAS, "Preset Control Point Operations" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetControlPointOpcode {
    ReadPresetRecords { start_index: u8, num_presets: u8 },
    WritePresetName { index: u8, name: HString<MAX_PRESET_NAME_LEN> },
    SetActivePreset { index: u8 },
    SetNextPreset,
    SetPreviousPreset,
    SetActivePresetSynchronizedLocally { index: u8 },
    SetNextPresetSynchronizedLocally,
    SetPreviousPresetSynchronizedLocally,
}

impl PresetControlPointOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::ReadPresetRecords { .. } => 0x01,
            Self::WritePresetName { .. } => 0x02,
            Self::SetActivePreset { .. } => 0x03,
            Self::SetNextPreset => 0x04,
            Self::SetPreviousPreset => 0x05,
            Self::SetActivePresetSynchronizedLocally { .. } => 0x06,
            Self::SetNextPresetSynchronizedLocally => 0x07,
            Self::SetPreviousPresetSynchronizedLocally => 0x08,
        }
    }

    fn encode(&self, out: &mut HVec<u8, { 2 + MAX_PRESET_NAME_LEN }>) {
        let _ = out.push(self.raw_opcode());
        match self {
            Self::ReadPresetRecords { start_index, num_presets } => {
                let _ = out.push(*start_index);
                let _ = out.push(*num_presets);
            }
            Self::WritePresetName { index, name } => {
                let _ = out.push(*index);
                let _ = out.extend_from_slice(name.as_bytes());
            }
            Self::SetActivePreset { index } | Self::SetActivePresetSynchronizedLocally { index } => {
                let _ = out.push(*index);
            }
            Self::SetNextPreset
            | Self::SetPreviousPreset
            | Self::SetNextPresetSynchronizedLocally
            | Self::SetPreviousPresetSynchronizedLocally => {}
        }
    }

    fn decode(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = *data.first().ok_or(FromGattError::InvalidLength)?;
        let rest = &data[1..];
        Ok(match opcode {
            0x01 => {
                if rest.len() != 2 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::ReadPresetRecords { start_index: rest[0], num_presets: rest[1] }
            }
            0x02 => {
                if rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                let name = core::str::from_utf8(&rest[1..]).map_err(|_| FromGattError::InvalidCharacter)?;
                Self::WritePresetName {
                    index: rest[0],
                    name: HString::try_from(name).map_err(|_| FromGattError::InvalidLength)?,
                }
            }
            0x03 => {
                if rest.len() != 1 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::SetActivePreset { index: rest[0] }
            }
            0x04 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::SetNextPreset
            }
            0x05 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::SetPreviousPreset
            }
            0x06 => {
                if rest.len() != 1 {
                    return Err(FromGattError::InvalidLength);
                }
                Self::SetActivePresetSynchronizedLocally { index: rest[0] }
            }
            0x07 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::SetNextPresetSynchronizedLocally
            }
            0x08 => {
                if !rest.is_empty() {
                    return Err(FromGattError::InvalidLength);
                }
                Self::SetPreviousPresetSynchronizedLocally
            }
            _ => return Err(FromGattError::InvalidLength),
        })
    }
}

/// The value written to the Preset Control Point characteristic.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetControlPointOperation {
    pub opcode: PresetControlPointOpcode,
    encoded: HVec<u8, { 2 + MAX_PRESET_NAME_LEN }>,
}

impl PresetControlPointOperation {
    pub fn new(opcode: PresetControlPointOpcode) -> Self {
        let mut encoded = HVec::new();
        opcode.encode(&mut encoded);
        Self { opcode, encoded }
    }
}

impl AsGatt for PresetControlPointOperation {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 2 + MAX_PRESET_NAME_LEN;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for PresetControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let opcode = PresetControlPointOpcode::decode(data)?;
        Ok(Self::new(opcode))
    }
}

impl Default for PresetControlPointOperation {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(PresetControlPointOpcode::SetNextPreset)
    }
}

/// Finds the index (into `presets`) of the record with the given `preset_index`.
fn find(presets: &[PresetRecord], preset_index: u8) -> Option<usize> {
    presets.iter().position(|p| p.index == preset_index)
}

/// Applies a Set Active Preset operation: the requested preset must exist and be
/// [`PresetProperties::Available`].
pub fn set_active_preset(presets: &[PresetRecord], requested_index: u8) -> Result<u8, AttErrorCode> {
    let record = presets.get(find(presets, requested_index).ok_or(PRESET_NOT_FOUND)?).unwrap();
    if !record.properties.contains(PresetProperties::Available) {
        return Err(PRESET_NOT_AVAILABLE);
    }
    Ok(record.index)
}

/// Applies a Set Next/Previous Preset operation: cyclically finds the next/previous
/// [`PresetProperties::Available`] preset (by position in `presets`) after/before `current_index`.
/// Returns [`PRESET_NOT_FOUND`] if `presets` has no available preset at all.
pub fn step_preset(presets: &[PresetRecord], current_index: u8, forward: bool) -> Result<u8, AttErrorCode> {
    if presets.is_empty() {
        return Err(PRESET_NOT_FOUND);
    }
    let start = find(presets, current_index).unwrap_or(0);
    let len = presets.len();
    for step in 1..=len {
        let offset = if forward { step } else { len - step };
        let candidate = &presets[(start + offset) % len];
        if candidate.properties.contains(PresetProperties::Available) {
            return Ok(candidate.index);
        }
    }
    Err(PRESET_NOT_FOUND)
}

/// Applies a Write Preset Name operation: the target preset must exist and be
/// [`PresetProperties::Writable`].
pub fn write_preset_name(presets: &mut [PresetRecord], index: u8, name: HString<MAX_PRESET_NAME_LEN>) -> Result<(), AttErrorCode> {
    let position = find(presets, index).ok_or(PRESET_NOT_FOUND)?;
    if !presets[position].properties.contains(PresetProperties::Writable) {
        return Err(WRITE_NAME_NOT_ALLOWED);
    }
    presets[position].name = name;
    Ok(())
}

/// A Gatt service client for reading/controlling a hearing aid's presets.
pub struct HasClient {
    handle: ServiceHandle,
    pub hearing_aid_features: Characteristic<HearingAidFeatures>,
    pub preset_control_point: Characteristic<PresetControlPointOperation>,
    pub active_preset_index: Characteristic<u8>,
}

impl HasClient {
    /// Discovers the HAS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose HAS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::HEARING_ACCESS)).await.ok()?;
        let handle = services.first()?;

        macro_rules! required {
            ($uuid:expr) => {
                client.characteristic_by_uuid(handle, &Uuid::from($uuid)).await.ok()?
            };
        }

        Some(Self {
            handle: handle.clone(),
            hearing_aid_features: required!(characteristic::HEARING_AID_FEATURES),
            preset_control_point: required!(characteristic::HEARING_AID_PRESET_CONTROL_POINT),
            active_preset_index: required!(characteristic::ACTIVE_PRESET_INDEX),
        })
    }
}

/// A Gatt service server exposing a hearing aid's preset control, with up to `MAX_PRESETS` stored
/// presets.
pub struct HasServer<const MAX_PRESETS: usize> {
    handle: u16,
    hearing_aid_features: Characteristic<HearingAidFeatures>,
    preset_control_point: Characteristic<PresetControlPointOperation>,
    active_preset_index: Characteristic<u8>,
    presets: RefCell<HVec<PresetRecord, MAX_PRESETS>>,
}

/// Owned backing storage for HAS's characteristics.
pub struct HasStorage {
    pub features: [u8; 1],
    /// Opcode + index + a maximal preset name (Write Preset Name is the largest operation).
    pub preset_control_point: [u8; 2 + MAX_PRESET_NAME_LEN],
    pub active_preset_index: [u8; 1],
}

impl Default for HasStorage {
    fn default() -> Self {
        Self {
            features: [0; 1],
            preset_control_point: [0; 2 + MAX_PRESET_NAME_LEN],
            active_preset_index: [0; 1],
        }
    }
}

pub const HAS_ATTRIBUTES: usize = 10;

impl<const MAX_PRESETS: usize> HasServer<MAX_PRESETS> {
    /// Creates a new HAS Gatt service with the given initial preset list.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        features: HearingAidFeatures,
        presets: HVec<PresetRecord, MAX_PRESETS>,
        active_preset_index: u8,
        features_store: &'a mut [u8],
        control_point_store: &'a mut [u8],
        active_preset_index_store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::HEARING_ACCESS));

        let hearing_aid_features = service
            .add_characteristic(characteristic::HEARING_AID_FEATURES, &[CharacteristicProp::Read], features, features_store)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let preset_control_point = service
            .add_characteristic(
                characteristic::HEARING_AID_PRESET_CONTROL_POINT,
                &[CharacteristicProp::Write, CharacteristicProp::Notify],
                PresetControlPointOperation::default(),
                control_point_store,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let active_preset_index = service
            .add_characteristic(
                characteristic::ACTIVE_PRESET_INDEX,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                active_preset_index,
                active_preset_index_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self {
            handle: service.build(),
            hearing_aid_features,
            preset_control_point,
            active_preset_index,
            presets: RefCell::new(presets),
        }
    }

    pub fn hearing_aid_features(&self) -> &Characteristic<HearingAidFeatures> {
        &self.hearing_aid_features
    }
    pub fn preset_control_point(&self) -> &Characteristic<PresetControlPointOperation> {
        &self.preset_control_point
    }
    pub fn active_preset_index(&self) -> &Characteristic<u8> {
        &self.active_preset_index
    }
    /// A snapshot of the currently stored presets.
    pub fn presets(&self) -> HVec<PresetRecord, MAX_PRESETS> {
        self.presets.borrow().clone()
    }
}

impl<const MAX_PRESETS: usize, P: PacketPool> LeAudioServerService<P> for HasServer<MAX_PRESETS> {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        let handles = [self.hearing_aid_features.handle, self.active_preset_index.handle];
        if handles.contains(&event.handle()) {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        // The Preset Control Point's business rules need the stored preset list, so
        // `Server::handle` special-cases it before it would otherwise reach this generic path -
        // the same way it special-cases VCS's Volume Control Point.
        if event.handle() == self.preset_control_point.handle {
            return Some(match event.value(&self.preset_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Applies a Preset Control Point write end to end: mutates `has`'s stored preset list / active
/// preset index (see `set_active_preset`/`step_preset`/`write_preset_name`), stores + notifies
/// whichever characteristics changed, and reports the outcome via the returned `AttErrorCode`
/// (`Ok(())` if the write should be accepted). `ReadPresetRecords` has no side effect - it's
/// answered directly by the caller inspecting [`HasServer::presets`] since this crate simplifies
/// the real spec's (possibly multi-notification) Read Preset Response into a single logical
/// query, matching this crate's general preference for a workable subset over exactly replaying
/// every wire notification a real Bluetooth SIG PTS test would expect.
pub async fn drive_preset_control_point<M: RawMutex, P: PacketPool, const MAX_PRESETS: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    has: &HasServer<MAX_PRESETS>,
    conn: &GattConnection<'_, '_, P>,
    operation: &PresetControlPointOperation,
) -> Result<(), AttErrorCode> {
    match &operation.opcode {
        PresetControlPointOpcode::ReadPresetRecords { .. } => Ok(()),
        PresetControlPointOpcode::SetActivePreset { index } | PresetControlPointOpcode::SetActivePresetSynchronizedLocally { index } => {
            let new_index = set_active_preset(&has.presets.borrow(), *index)?;
            let _ = has.active_preset_index.notify(conn, &new_index, true).await;
            Ok(())
        }
        PresetControlPointOpcode::SetNextPreset | PresetControlPointOpcode::SetNextPresetSynchronizedLocally => {
            let current = has.active_preset_index.get(server).unwrap_or(0);
            let new_index = step_preset(&has.presets.borrow(), current, true)?;
            let _ = has.active_preset_index.notify(conn, &new_index, true).await;
            Ok(())
        }
        PresetControlPointOpcode::SetPreviousPreset | PresetControlPointOpcode::SetPreviousPresetSynchronizedLocally => {
            let current = has.active_preset_index.get(server).unwrap_or(0);
            let new_index = step_preset(&has.presets.borrow(), current, false)?;
            let _ = has.active_preset_index.notify(conn, &new_index, true).await;
            Ok(())
        }
        PresetControlPointOpcode::WritePresetName { index, name } => {
            write_preset_name(&mut has.presets.borrow_mut(), *index, name.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn preset(index: u8, properties: PresetProperties, name: &str) -> PresetRecord {
        PresetRecord { index, properties, name: HString::try_from(name).unwrap() }
    }

    fn sample_presets() -> HVec<PresetRecord, 4> {
        let mut v = HVec::new();
        let _ = v.push(preset(1, PresetProperties::Available, "Outdoor"));
        let _ = v.push(preset(2, PresetProperties::empty(), "Unavailable"));
        let _ = v.push(preset(3, PresetProperties::Available | PresetProperties::Writable, "Restaurant"));
        v
    }

    #[test]
    fn hearing_aid_features_round_trips() {
        let features = HearingAidFeatures {
            hearing_aid_type: HearingAidType::Binaural,
            preset_synchronization_supported: true,
            independent_presets: false,
            dynamic_presets: true,
            writable_preset_names: false,
        };
        assert_eq!(HearingAidFeatures::from_gatt(features.as_gatt()).unwrap(), features);
    }

    #[test]
    fn control_point_operation_round_trips_every_opcode_shape() {
        for opcode in [
            PresetControlPointOpcode::ReadPresetRecords { start_index: 1, num_presets: 5 },
            PresetControlPointOpcode::WritePresetName { index: 2, name: HString::try_from("Loud").unwrap() },
            PresetControlPointOpcode::SetActivePreset { index: 3 },
            PresetControlPointOpcode::SetNextPreset,
            PresetControlPointOpcode::SetPreviousPreset,
            PresetControlPointOpcode::SetActivePresetSynchronizedLocally { index: 1 },
            PresetControlPointOpcode::SetNextPresetSynchronizedLocally,
            PresetControlPointOpcode::SetPreviousPresetSynchronizedLocally,
        ] {
            let op = PresetControlPointOperation::new(opcode.clone());
            assert_eq!(PresetControlPointOperation::from_gatt(op.as_gatt()).unwrap().opcode, opcode);
        }
    }

    #[test]
    fn set_active_preset_succeeds_for_an_available_preset() {
        assert_eq!(set_active_preset(&sample_presets(), 3), Ok(3));
    }

    #[test]
    fn set_active_preset_rejects_an_unavailable_preset() {
        assert_eq!(set_active_preset(&sample_presets(), 2), Err(PRESET_NOT_AVAILABLE));
    }

    #[test]
    fn set_active_preset_rejects_a_missing_index() {
        assert_eq!(set_active_preset(&sample_presets(), 99), Err(PRESET_NOT_FOUND));
    }

    #[test]
    fn step_preset_forward_skips_unavailable_presets() {
        // From preset 1 (Outdoor, Available), next should skip preset 2 (Unavailable) and land on
        // preset 3 (Restaurant, Available).
        assert_eq!(step_preset(&sample_presets(), 1, true), Ok(3));
    }

    #[test]
    fn step_preset_wraps_around() {
        // From preset 3 (the last Available one), next wraps back around to preset 1.
        assert_eq!(step_preset(&sample_presets(), 3, true), Ok(1));
    }

    #[test]
    fn step_preset_backward_also_skips_unavailable_presets() {
        assert_eq!(step_preset(&sample_presets(), 1, false), Ok(3));
    }

    #[test]
    fn write_preset_name_rejects_a_non_writable_preset() {
        let mut presets = sample_presets();
        let result = write_preset_name(&mut presets, 1, HString::try_from("New Name").unwrap());
        assert_eq!(result, Err(WRITE_NAME_NOT_ALLOWED));
    }

    #[test]
    fn write_preset_name_succeeds_for_a_writable_preset() {
        let mut presets = sample_presets();
        write_preset_name(&mut presets, 3, HString::try_from("Cafe").unwrap()).unwrap();
        assert_eq!(presets[2].name, HString::<MAX_PRESET_NAME_LEN>::try_from("Cafe").unwrap());
    }

    #[test]
    fn has_server_exposes_features_and_active_preset_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static FEATURES: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static CP: static_cell::StaticCell<[u8; 42]> = static_cell::StaticCell::new();
        static ACTIVE: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();

        let has: HasServer<4> = HasServer::new(
            &mut table,
            HearingAidFeatures::default(),
            sample_presets(),
            1,
            FEATURES.init([0; 1]),
            CP.init([0; 42]),
            ACTIVE.init([0; 1]),
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(has.active_preset_index().get(&server).unwrap(), 1);
        assert_eq!(has.presets().len(), 3);
    }
}
