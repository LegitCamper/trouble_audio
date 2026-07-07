//! ## Volume Control Service
//!
//! The Volume Control Service (VCS) exposes a single logical volume control, letting a client
//! read the current volume/mute state and adjust it via the Volume Control Point. Bluetooth VCS
//! v1.0 Section 3.

use bitflags::bitflags;
use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{GattConnection, ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{LeAudioServerService, MAX_SERVICES};

/// Application error code (VCS 3.3): the Change_Counter in a Volume Control Point write didn't
/// match the Volume State characteristic's current Change_Counter.
pub const INVALID_CHANGE_COUNTER: AttErrorCode = AttErrorCode::new(0x80);
/// Application error code (VCS 3.3): the Opcode in a Volume Control Point write isn't supported.
pub const OPCODE_NOT_SUPPORTED: AttErrorCode = AttErrorCode::new(0x81);

/// Volume State's Mute field (VCS 3.1, Table 3.1) - distinct from [`crate::mics::Mute`], which
/// additionally has a `Disabled` value that doesn't apply here.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mute {
    #[default]
    NotMuted = 0x00,
    Muted = 0x01,
}

impl TryFrom<u8> for Mute {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::NotMuted),
            0x01 => Ok(Self::Muted),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// The Volume State characteristic value (VCS 3.1): Volume_Setting (1) | Mute (1) |
/// Change_Counter (1).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct VolumeState {
    pub volume_setting: u8,
    pub mute: Mute,
    pub change_counter: u8,
}

impl AsGatt for VolumeState {
    const MIN_SIZE: usize = 3;
    const MAX_SIZE: usize = 3;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `Self` is three `u8`-sized fields back to back with no padding (`Mute` is
        // `#[repr(u8)]`), matching the wire layout exactly.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 3) }
    }
}

impl FromGatt for VolumeState {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 3 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            volume_setting: data[0],
            mute: Mute::try_from(data[1])?,
            change_counter: data[2],
        })
    }
}

impl FixedGattValue for VolumeState {
    const SIZE: usize = 3;
}

bitflags! {
    /// Volume Flags bitfield (VCS 3.3).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VolumeFlags: u8 {
        /// Set once the server's Volume_Setting has been changed from its factory default and
        /// that value is persisted across power cycles.
        const VolumeSettingPersisted = 0x01;
    }
}

impl AsGatt for VolumeFlags {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u8 here).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for VolumeFlags {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self::from_bits_truncate(data[0]))
    }
}

impl FixedGattValue for VolumeFlags {
    const SIZE: usize = 1;
}

/// Volume Control Point Opcodes (VCS 3.2, Table 3.2).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeControlPointOpcode {
    RelativeVolumeDown,
    RelativeVolumeUp,
    UnmuteRelativeVolumeDown,
    UnmuteRelativeVolumeUp,
    /// Carries the requested Volume_Setting.
    SetAbsoluteVolume(u8),
    Unmute,
    Mute,
}

impl VolumeControlPointOpcode {
    fn opcode(&self) -> u8 {
        match self {
            Self::RelativeVolumeDown => 0x00,
            Self::RelativeVolumeUp => 0x01,
            Self::UnmuteRelativeVolumeDown => 0x02,
            Self::UnmuteRelativeVolumeUp => 0x03,
            Self::SetAbsoluteVolume(_) => 0x04,
            Self::Unmute => 0x05,
            Self::Mute => 0x06,
        }
    }
}

/// The value written to the Volume Control Point characteristic: Opcode (1) | Change_Counter (1)
/// | optional Volume_Setting (1, only for [`VolumeControlPointOpcode::SetAbsoluteVolume`]). Wire
/// bytes are cached in `encoded` (like `PAC`/`Ase` in pacs.rs/ascs.rs) since the value is
/// variable-length and `AsGatt::as_gatt` must return a borrow, not an owned buffer.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeControlPointOperation {
    pub opcode: VolumeControlPointOpcode,
    pub change_counter: u8,
    encoded: [u8; 3],
    encoded_len: u8,
}

impl VolumeControlPointOperation {
    pub fn new(opcode: VolumeControlPointOpcode, change_counter: u8) -> Self {
        let mut encoded = [0u8; 3];
        encoded[0] = opcode.opcode();
        encoded[1] = change_counter;
        let mut encoded_len = 2;
        if let VolumeControlPointOpcode::SetAbsoluteVolume(volume) = opcode {
            encoded[2] = volume;
            encoded_len = 3;
        }
        Self { opcode, change_counter, encoded, encoded_len }
    }
}

impl FromGatt for VolumeControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() < 2 {
            return Err(FromGattError::InvalidLength);
        }
        let change_counter = data[1];
        let opcode = match data[0] {
            0x00 => VolumeControlPointOpcode::RelativeVolumeDown,
            0x01 => VolumeControlPointOpcode::RelativeVolumeUp,
            0x02 => VolumeControlPointOpcode::UnmuteRelativeVolumeDown,
            0x03 => VolumeControlPointOpcode::UnmuteRelativeVolumeUp,
            0x04 => {
                let volume = *data.get(2).ok_or(FromGattError::InvalidLength)?;
                VolumeControlPointOpcode::SetAbsoluteVolume(volume)
            }
            0x05 => VolumeControlPointOpcode::Unmute,
            0x06 => VolumeControlPointOpcode::Mute,
            _ => return Err(FromGattError::InvalidLength),
        };
        let expected_len = if opcode.opcode() == 0x04 { 3 } else { 2 };
        if data.len() != expected_len {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self::new(opcode, change_counter))
    }
}

impl AsGatt for VolumeControlPointOperation {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 3;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded[..self.encoded_len as usize]
    }
}

impl Default for VolumeControlPointOperation {
    /// An arbitrary valid operation - only used to seed the characteristic's backing store at
    /// construction time (the Volume Control Point is write-only, so this value is never read).
    fn default() -> Self {
        Self::new(VolumeControlPointOpcode::Mute, 0)
    }
}

/// Applies one Volume Control Point operation to `current`, per VCS 3.2. `step` is this
/// implementation's Volume_Step_Size (the amount a relative up/down operation changes
/// Volume_Setting by) - the spec leaves this value implementation-defined.
///
/// Returns [`INVALID_CHANGE_COUNTER`] if `operation.change_counter` doesn't match
/// `current.change_counter`.
pub fn apply_volume_operation(
    current: VolumeState,
    operation: VolumeControlPointOperation,
    step: u8,
) -> Result<VolumeState, AttErrorCode> {
    if operation.change_counter != current.change_counter {
        return Err(INVALID_CHANGE_COUNTER);
    }

    let next_change_counter = current.change_counter.wrapping_add(1);
    let next = match operation.opcode {
        VolumeControlPointOpcode::RelativeVolumeDown => VolumeState {
            volume_setting: current.volume_setting.saturating_sub(step),
            ..current
        },
        VolumeControlPointOpcode::RelativeVolumeUp => VolumeState {
            volume_setting: current.volume_setting.saturating_add(step),
            ..current
        },
        VolumeControlPointOpcode::UnmuteRelativeVolumeDown => VolumeState {
            volume_setting: current.volume_setting.saturating_sub(step),
            mute: Mute::NotMuted,
            ..current
        },
        VolumeControlPointOpcode::UnmuteRelativeVolumeUp => VolumeState {
            volume_setting: current.volume_setting.saturating_add(step),
            mute: Mute::NotMuted,
            ..current
        },
        VolumeControlPointOpcode::SetAbsoluteVolume(volume_setting) => VolumeState { volume_setting, ..current },
        VolumeControlPointOpcode::Unmute => VolumeState { mute: Mute::NotMuted, ..current },
        VolumeControlPointOpcode::Mute => VolumeState { mute: Mute::Muted, ..current },
    };
    Ok(VolumeState {
        change_counter: next_change_counter,
        ..next
    })
}

/// A Gatt service client for reading/controlling a device's volume.
pub struct VcsClient {
    handle: ServiceHandle,
    pub volume_state: Characteristic<VolumeState>,
    pub volume_control_point: Characteristic<VolumeControlPointOperation>,
    pub volume_flags: Characteristic<VolumeFlags>,
}

impl VcsClient {
    /// Discovers the VCS service and its characteristics on an already-connected `GattClient`.
    pub async fn new<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Self {
        let services = client.services_by_uuid(&Uuid::from(service::VOLUME_CONTROL)).await.unwrap();
        let handle = services.first().unwrap();

        let volume_state = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::VOLUME_STATE))
            .await
            .expect("Volume State is mandatory on VCS");
        let volume_control_point = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::VOLUME_CONTROL_POINT))
            .await
            .expect("Volume Control Point is mandatory on VCS");
        let volume_flags = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::VOLUME_FLAGS))
            .await
            .expect("Volume Flags is mandatory on VCS");

        Self {
            handle: handle.clone(),
            volume_state,
            volume_control_point,
            volume_flags,
        }
    }
}

/// A Gatt service server exposing volume control.
pub struct VcsServer {
    handle: u16,
    volume_state: Characteristic<VolumeState>,
    volume_control_point: Characteristic<VolumeControlPointOperation>,
    volume_flags: Characteristic<VolumeFlags>,
    /// This implementation's Volume_Step_Size - see [`apply_volume_operation`].
    step: u8,
}

pub const VCS_ATTRIBUTES: usize = 10;

impl VcsServer {
    /// Creates a new VCS Gatt service.
    #[allow(clippy::too_many_arguments)]
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        initial: VolumeState,
        flags: VolumeFlags,
        step: u8,
        volume_state_store: &'a mut [u8],
        volume_control_point_store: &'a mut [u8],
        volume_flags_store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::VOLUME_CONTROL));

        let volume_state = service
            .add_characteristic(
                characteristic::VOLUME_STATE,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                initial,
                volume_state_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let volume_control_point = service
            .add_characteristic(
                characteristic::VOLUME_CONTROL_POINT,
                &[CharacteristicProp::Write],
                VolumeControlPointOperation::default(),
                volume_control_point_store,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let volume_flags = service
            .add_characteristic(
                characteristic::VOLUME_FLAGS,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                flags,
                volume_flags_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self {
            handle: service.build(),
            volume_state,
            volume_control_point,
            volume_flags,
            step,
        }
    }

    pub fn volume_state(&self) -> &Characteristic<VolumeState> {
        &self.volume_state
    }
    pub fn volume_control_point(&self) -> &Characteristic<VolumeControlPointOperation> {
        &self.volume_control_point
    }
    pub fn volume_flags(&self) -> &Characteristic<VolumeFlags> {
        &self.volume_flags
    }
}

impl<P: PacketPool> LeAudioServerService<P> for VcsServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.volume_state.handle {
            return Some(Ok(()));
        }
        if event.handle() == self.volume_flags.handle {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.volume_state.handle || event.handle() == self.volume_flags.handle {
            return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
        }
        // Only validates that the write decodes to a shape-valid operation. Applying it against
        // the current `VolumeState` needs the `AttributeServer`, which isn't reachable from this
        // trait, so `Server::handle` special-cases the Volume Control Point handle (calling
        // [`drive_volume_control_point`]) before it would otherwise reach this generic path - the
        // same way it special-cases the ASE Control Point for ASCS.
        if event.handle() == self.volume_control_point.handle {
            return Some(match event.value(&self.volume_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Applies a Volume Control Point write end to end: decodes the raw write value, applies it
/// against the current `VolumeState` (see [`apply_volume_operation`]), and - on success - stores
/// and notifies the new `VolumeState`. Returns the `AttErrorCode` the ATT write itself should be
/// rejected with on failure, or `Ok(())` if it should be accepted.
///
/// Mirrors [`crate::bap::drive_ase_control_point`]'s split: the pure state transition
/// ([`apply_volume_operation`]) is unit-tested directly, while this async wrapper (which needs a
/// live `GattConnection` to notify) is exercised only through the full `Server::handle` path.
pub async fn drive_volume_control_point<M: RawMutex, P: PacketPool, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    vcs: &VcsServer,
    conn: &GattConnection<'_, '_, P>,
    operation: &VolumeControlPointOperation,
) -> Result<(), AttErrorCode> {
    let current = vcs.volume_state.get(server).map_err(|_| AttErrorCode::UNLIKELY_ERROR)?;
    let new_state = apply_volume_operation(current, *operation, vcs.step)?;
    let _ = vcs.volume_state.notify(conn, &new_state, true).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn state(volume_setting: u8, mute: Mute, change_counter: u8) -> VolumeState {
        VolumeState { volume_setting, mute, change_counter }
    }

    #[test]
    fn volume_state_round_trips() {
        let s = state(0x40, Mute::Muted, 0x07);
        assert_eq!(VolumeState::from_gatt(s.as_gatt()).unwrap(), s);
    }

    #[test]
    fn volume_flags_round_trips() {
        let flags = VolumeFlags::VolumeSettingPersisted;
        assert_eq!(VolumeFlags::from_gatt(flags.as_gatt()).unwrap(), flags);
        assert_eq!(VolumeFlags::from_gatt(&[0x00]).unwrap(), VolumeFlags::empty());
    }

    #[test]
    fn control_point_operation_round_trips_every_opcode() {
        let change_counter = 0x03;
        for opcode in [
            VolumeControlPointOpcode::RelativeVolumeDown,
            VolumeControlPointOpcode::RelativeVolumeUp,
            VolumeControlPointOpcode::UnmuteRelativeVolumeDown,
            VolumeControlPointOpcode::UnmuteRelativeVolumeUp,
            VolumeControlPointOpcode::SetAbsoluteVolume(0x64),
            VolumeControlPointOpcode::Unmute,
            VolumeControlPointOpcode::Mute,
        ] {
            let op = VolumeControlPointOperation::new(opcode, change_counter);
            assert_eq!(VolumeControlPointOperation::from_gatt(op.as_gatt()).unwrap(), op);
        }
    }

    #[test]
    fn control_point_rejects_wrong_length_for_set_absolute_volume() {
        // Opcode 0x04 (Set Absolute Volume) without its Volume_Setting operand.
        assert!(VolumeControlPointOperation::from_gatt(&[0x04, 0x00]).is_err());
    }

    #[test]
    fn control_point_rejects_unknown_opcode() {
        assert!(VolumeControlPointOperation::from_gatt(&[0x07, 0x00]).is_err());
    }

    #[test]
    fn apply_operation_rejects_stale_change_counter() {
        let current = state(50, Mute::NotMuted, 5);
        let op = VolumeControlPointOperation::new(VolumeControlPointOpcode::Mute, 4);
        assert_eq!(apply_volume_operation(current, op, 8), Err(INVALID_CHANGE_COUNTER));
    }

    #[test]
    fn relative_volume_down_decrements_by_step_and_advances_change_counter() {
        let current = state(50, Mute::NotMuted, 5);
        let op = VolumeControlPointOperation::new(VolumeControlPointOpcode::RelativeVolumeDown, 5);
        let next = apply_volume_operation(current, op, 8).unwrap();
        assert_eq!(next, state(42, Mute::NotMuted, 6));
    }

    #[test]
    fn relative_volume_operations_saturate_at_the_volume_setting_bounds() {
        let low = state(3, Mute::NotMuted, 0);
        let down = VolumeControlPointOperation::new(VolumeControlPointOpcode::RelativeVolumeDown, 0);
        assert_eq!(apply_volume_operation(low, down, 8).unwrap().volume_setting, 0);

        let high = state(250, Mute::NotMuted, 0);
        let up = VolumeControlPointOperation::new(VolumeControlPointOpcode::RelativeVolumeUp, 0);
        assert_eq!(apply_volume_operation(high, up, 8).unwrap().volume_setting, 255);
    }

    #[test]
    fn unmute_relative_volume_operations_clear_mute() {
        let current = state(50, Mute::Muted, 0);
        let op = VolumeControlPointOperation::new(VolumeControlPointOpcode::UnmuteRelativeVolumeUp, 0);
        let next = apply_volume_operation(current, op, 8).unwrap();
        assert_eq!(next.mute, Mute::NotMuted);
        assert_eq!(next.volume_setting, 58);
    }

    #[test]
    fn set_absolute_volume_does_not_affect_mute() {
        let current = state(10, Mute::Muted, 2);
        let op = VolumeControlPointOperation::new(VolumeControlPointOpcode::SetAbsoluteVolume(0x99), 2);
        let next = apply_volume_operation(current, op, 8).unwrap();
        assert_eq!(next, state(0x99, Mute::Muted, 3));
    }

    #[test]
    fn mute_and_unmute_leave_volume_setting_untouched() {
        let current = state(77, Mute::NotMuted, 9);
        let muted = apply_volume_operation(current, VolumeControlPointOperation::new(VolumeControlPointOpcode::Mute, 9), 8).unwrap();
        assert_eq!(muted, state(77, Mute::Muted, 10));
        let unmuted = apply_volume_operation(muted, VolumeControlPointOperation::new(VolumeControlPointOpcode::Unmute, 10), 8).unwrap();
        assert_eq!(unmuted, state(77, Mute::NotMuted, 11));
    }

    #[test]
    fn change_counter_wraps_around_at_255() {
        let current = state(0, Mute::NotMuted, 255);
        let op = VolumeControlPointOperation::new(VolumeControlPointOpcode::Mute, 255);
        assert_eq!(apply_volume_operation(current, op, 8).unwrap().change_counter, 0);
    }

    #[test]
    fn vcs_server_exposes_volume_state_and_flags_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static VS: static_cell::StaticCell<[u8; 3]> = static_cell::StaticCell::new();
        static CP: static_cell::StaticCell<[u8; 3]> = static_cell::StaticCell::new();
        static FLAGS: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        let vcs = VcsServer::new(
            &mut table,
            state(64, Mute::NotMuted, 0),
            VolumeFlags::empty(),
            8,
            VS.init([0; 3]),
            CP.init([0; 3]),
            FLAGS.init([0; 1]),
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(vcs.volume_state().get(&server).unwrap(), state(64, Mute::NotMuted, 0));
        vcs.volume_state().set(&server, &state(70, Mute::NotMuted, 1)).unwrap();
        assert_eq!(vcs.volume_state().get(&server).unwrap(), state(70, Mute::NotMuted, 1));
    }
}
