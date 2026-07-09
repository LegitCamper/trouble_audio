//! ## Audio Input Control Service
//!
//! The Audio Input Control Service (AICS) exposes gain/mute/mode control over one audio input
//! (e.g. a Bluetooth stream, a microphone, an analog line-in). AICS is normally exposed as an
//! *included* service of VCS or MICS (one instance per input the volume/microphone control
//! applies to); this crate exposes it as a standalone top-level service instead - simpler to wire
//! up, and standalone AICS is equally spec-valid - rather than threading GATT Include
//! declarations through `VcsServer`/`MicsServer`.
//!
//! Application error codes are reconstructed from public documentation, not cross-checked against
//! the official Bluetooth SIG assigned-numbers PDF - verify before relying on the exact values
//! for certification.

use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

pub use crate::mics::Mute;
use crate::{LeAudioServerService, MAX_SERVICES};

/// Application error code (AICS): the Change_Counter in a Audio Input Control Point write didn't
/// match the Audio Input State characteristic's current Change_Counter.
pub const INVALID_CHANGE_COUNTER: AttErrorCode = AttErrorCode::new(0x80);
/// Application error code (AICS): the Set Manual/Automatic Gain Mode opcode was used while
/// `GainMode` is fixed (`ManualOnly`/`AutomaticOnly`, i.e. not switchable).
pub const OPCODE_NOT_SUPPORTED: AttErrorCode = AttErrorCode::new(0x81);
/// Application error code (AICS): a Mute/Unmute operation was attempted while `Mute` is
/// `Disabled`. Mirrors [`crate::mics::MUTE_DISABLED`].
pub const MUTE_DISABLED: AttErrorCode = AttErrorCode::new(0x82);
/// Application error code (AICS): a Set Gain Setting operation's value was outside
/// `[GainSettingsAttribute::minimum, GainSettingsAttribute::maximum]`.
pub const VALUE_OUT_OF_RANGE: AttErrorCode = AttErrorCode::new(0x83);

/// Gain_Mode (AICS, "Audio Input State" table): whether gain is under manual or automatic
/// control, and whether that's switchable.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GainMode {
    /// Fixed manual gain - Set (Manual/Automatic) Gain Mode opcodes are not applicable.
    ManualOnly = 0x00,
    /// Fixed automatic gain - Set (Manual/Automatic) Gain Mode opcodes are not applicable.
    AutomaticOnly = 0x01,
    /// Currently under manual control; switchable to automatic.
    #[default]
    Manual = 0x02,
    /// Currently under automatic control; switchable to manual.
    Automatic = 0x03,
}

impl GainMode {
    fn is_switchable(self) -> bool {
        matches!(self, Self::Manual | Self::Automatic)
    }
}

impl TryFrom<u8> for GainMode {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::ManualOnly),
            0x01 => Ok(Self::AutomaticOnly),
            0x02 => Ok(Self::Manual),
            0x03 => Ok(Self::Automatic),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// The Audio Input State characteristic value (AICS): Gain_Setting (1, signed) | Mute (1) |
/// Gain_Mode (1) | Change_Counter (1).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct AudioInputState {
    pub gain_setting: i8,
    pub mute: Mute,
    pub gain_mode: GainMode,
    pub change_counter: u8,
}

impl Default for AudioInputState {
    fn default() -> Self {
        Self {
            gain_setting: 0,
            mute: Mute::NotMuted,
            gain_mode: GainMode::default(),
            change_counter: 0,
        }
    }
}

impl AsGatt for AudioInputState {
    const MIN_SIZE: usize = 4;
    const MAX_SIZE: usize = 4;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`, four one-byte fields (`i8`/`Mute`/`GainMode` are all one byte,
        // the latter two `#[repr(u8)]`) back to back with no padding.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 4) }
    }
}

impl FromGatt for AudioInputState {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 4 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            gain_setting: data[0] as i8,
            mute: Mute::try_from(data[1])?,
            gain_mode: GainMode::try_from(data[2])?,
            change_counter: data[3],
        })
    }
}

impl FixedGattValue for AudioInputState {
    const SIZE: usize = 4;
}

/// The Gain Settings Attribute characteristic value (AICS): Gain_Setting_Units (1, resolution in
/// steps of 0.1 dB) | Gain_Setting_Minimum (1, signed) | Gain_Setting_Maximum (1, signed).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GainSettingsAttribute {
    pub units: u8,
    pub minimum: i8,
    pub maximum: i8,
}

impl AsGatt for GainSettingsAttribute {
    const MIN_SIZE: usize = 3;
    const MAX_SIZE: usize = 3;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`, three adjacent one-byte fields with no padding.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 3) }
    }
}

impl FromGatt for GainSettingsAttribute {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 3 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            units: data[0],
            minimum: data[1] as i8,
            maximum: data[2] as i8,
        })
    }
}

impl FixedGattValue for GainSettingsAttribute {
    const SIZE: usize = 3;
}

/// Audio_Input_Type (AICS, "Audio Input Type" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioInputType {
    #[default]
    Unspecified = 0x00,
    Bluetooth = 0x01,
    Microphone = 0x02,
    Analog = 0x03,
    Digital = 0x04,
    Radio = 0x05,
    Streaming = 0x06,
    Ambient = 0x07,
}

impl TryFrom<u8> for AudioInputType {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Unspecified),
            0x01 => Ok(Self::Bluetooth),
            0x02 => Ok(Self::Microphone),
            0x03 => Ok(Self::Analog),
            0x04 => Ok(Self::Digital),
            0x05 => Ok(Self::Radio),
            0x06 => Ok(Self::Streaming),
            0x07 => Ok(Self::Ambient),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for AudioInputType {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for AudioInputType {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for AudioInputType {
    const SIZE: usize = 1;
}

/// Audio_Input_Status (AICS, "Audio Input Status" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioInputStatus {
    #[default]
    Inactive = 0x00,
    Active = 0x01,
}

impl TryFrom<u8> for AudioInputStatus {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Inactive),
            0x01 => Ok(Self::Active),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for AudioInputStatus {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for AudioInputStatus {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for AudioInputStatus {
    const SIZE: usize = 1;
}

/// Audio Input Control Point Opcodes (AICS, "Audio Input Control Point" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioInputControlPointOpcode {
    SetGainSetting(i8),
    Unmute,
    Mute,
    SetManualGainMode,
    SetAutomaticGainMode,
}

impl AudioInputControlPointOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::SetGainSetting(_) => 0x01,
            Self::Unmute => 0x02,
            Self::Mute => 0x03,
            Self::SetManualGainMode => 0x04,
            Self::SetAutomaticGainMode => 0x05,
        }
    }
}

/// The value written to the Audio Input Control Point characteristic: Opcode (1) |
/// Change_Counter (1) | optional Gain_Setting (1, signed - only for `SetGainSetting`).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInputControlPointOperation {
    pub opcode: AudioInputControlPointOpcode,
    pub change_counter: u8,
    encoded: [u8; 3],
    encoded_len: u8,
}

impl AudioInputControlPointOperation {
    pub fn new(opcode: AudioInputControlPointOpcode, change_counter: u8) -> Self {
        let mut encoded = [0u8; 3];
        encoded[0] = opcode.raw_opcode();
        encoded[1] = change_counter;
        let mut encoded_len = 2;
        if let AudioInputControlPointOpcode::SetGainSetting(gain) = opcode {
            encoded[2] = gain as u8;
            encoded_len = 3;
        }
        Self { opcode, change_counter, encoded, encoded_len }
    }
}

impl AsGatt for AudioInputControlPointOperation {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 3;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded[..self.encoded_len as usize]
    }
}

impl FromGatt for AudioInputControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() < 2 {
            return Err(FromGattError::InvalidLength);
        }
        let change_counter = data[1];
        let opcode = match data[0] {
            0x01 => {
                let gain = *data.get(2).ok_or(FromGattError::InvalidLength)? as i8;
                AudioInputControlPointOpcode::SetGainSetting(gain)
            }
            0x02 => AudioInputControlPointOpcode::Unmute,
            0x03 => AudioInputControlPointOpcode::Mute,
            0x04 => AudioInputControlPointOpcode::SetManualGainMode,
            0x05 => AudioInputControlPointOpcode::SetAutomaticGainMode,
            _ => return Err(FromGattError::InvalidLength),
        };
        let expected_len = if opcode.raw_opcode() == 0x01 { 3 } else { 2 };
        if data.len() != expected_len {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self::new(opcode, change_counter))
    }
}

impl Default for AudioInputControlPointOperation {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(AudioInputControlPointOpcode::Unmute, 0)
    }
}

/// Applies one Audio Input Control Point operation to `current`/`gain_limits`, per AICS's Audio
/// Input Control Point procedure requirements.
pub fn apply_input_operation(
    current: AudioInputState,
    gain_limits: GainSettingsAttribute,
    operation: AudioInputControlPointOperation,
) -> Result<AudioInputState, AttErrorCode> {
    if operation.change_counter != current.change_counter {
        return Err(INVALID_CHANGE_COUNTER);
    }
    let next_change_counter = current.change_counter.wrapping_add(1);

    let next = match operation.opcode {
        AudioInputControlPointOpcode::SetGainSetting(gain) => {
            if gain < gain_limits.minimum || gain > gain_limits.maximum {
                return Err(VALUE_OUT_OF_RANGE);
            }
            AudioInputState { gain_setting: gain, ..current }
        }
        AudioInputControlPointOpcode::Unmute => {
            if current.mute == Mute::Disabled {
                return Err(MUTE_DISABLED);
            }
            AudioInputState { mute: Mute::NotMuted, ..current }
        }
        AudioInputControlPointOpcode::Mute => {
            if current.mute == Mute::Disabled {
                return Err(MUTE_DISABLED);
            }
            AudioInputState { mute: Mute::Muted, ..current }
        }
        AudioInputControlPointOpcode::SetManualGainMode => {
            if !current.gain_mode.is_switchable() {
                return Err(OPCODE_NOT_SUPPORTED);
            }
            AudioInputState { gain_mode: GainMode::Manual, ..current }
        }
        AudioInputControlPointOpcode::SetAutomaticGainMode => {
            if !current.gain_mode.is_switchable() {
                return Err(OPCODE_NOT_SUPPORTED);
            }
            AudioInputState { gain_mode: GainMode::Automatic, ..current }
        }
    };
    Ok(AudioInputState { change_counter: next_change_counter, ..next })
}

/// A Gatt service client for reading/controlling one audio input.
pub struct AicsClient {
    handle: ServiceHandle,
    pub audio_input_state: Characteristic<AudioInputState>,
    pub gain_settings_attribute: Characteristic<GainSettingsAttribute>,
    pub audio_input_type: Characteristic<AudioInputType>,
    pub audio_input_status: Characteristic<AudioInputStatus>,
    pub audio_input_control_point: Characteristic<AudioInputControlPointOperation>,
    pub audio_input_description: Characteristic<heapless::String<32>>,
}

impl AicsClient {
    /// Discovers the AICS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose AICS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::AUDIO_INPUT_CONTROL)).await.ok()?;
        let handle = services.first()?;

        macro_rules! required {
            ($uuid:expr) => {
                client.characteristic_by_uuid(handle, &Uuid::from($uuid)).await.ok()?
            };
        }

        Some(Self {
            handle: handle.clone(),
            audio_input_state: required!(characteristic::AUDIO_INPUT_STATE),
            gain_settings_attribute: required!(characteristic::GAIN_SETTINGS_ATTRIBUTE),
            audio_input_type: required!(characteristic::AUDIO_INPUT_TYPE),
            audio_input_status: required!(characteristic::AUDIO_INPUT_STATUS),
            audio_input_control_point: required!(characteristic::AUDIO_INPUT_CONTROL_POINT),
            audio_input_description: required!(characteristic::AUDIO_INPUT_DESCRIPTION),
        })
    }
}

/// A Gatt service server exposing control over one audio input.
pub struct AicsServer {
    handle: u16,
    audio_input_state: Characteristic<AudioInputState>,
    gain_settings_attribute: GainSettingsAttribute,
    audio_input_type: Characteristic<AudioInputType>,
    audio_input_status: Characteristic<AudioInputStatus>,
    audio_input_control_point: Characteristic<AudioInputControlPointOperation>,
    audio_input_description: Characteristic<heapless::String<32>>,
}

pub const AICS_ATTRIBUTES: usize = 16;

/// Backing storage buffers for [`AicsServer::new`].
pub struct AicsStore<'a> {
    pub audio_input_state: &'a mut [u8],
    pub audio_input_type: &'a mut [u8],
    pub audio_input_status: &'a mut [u8],
    pub audio_input_control_point: &'a mut [u8],
    pub audio_input_description: &'a mut [u8],
}

impl AicsServer {
    /// Creates a new AICS Gatt service.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        initial_state: AudioInputState,
        gain_settings_attribute: GainSettingsAttribute,
        input_type: AudioInputType,
        initial_status: AudioInputStatus,
        description: heapless::String<32>,
        gain_settings_attribute_store: &'a mut [u8],
        input_type_store: &'a mut [u8],
        store: AicsStore<'a>,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::AUDIO_INPUT_CONTROL));

        let audio_input_state = service
            .add_characteristic(
                characteristic::AUDIO_INPUT_STATE,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                initial_state,
                store.audio_input_state,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let _gain_settings_attribute_char = service
            .add_characteristic(
                characteristic::GAIN_SETTINGS_ATTRIBUTE,
                &[CharacteristicProp::Read],
                gain_settings_attribute,
                gain_settings_attribute_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let audio_input_type = service
            .add_characteristic(characteristic::AUDIO_INPUT_TYPE, &[CharacteristicProp::Read], input_type, input_type_store)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let audio_input_status = service
            .add_characteristic(
                characteristic::AUDIO_INPUT_STATUS,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                initial_status,
                store.audio_input_status,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let audio_input_control_point = service
            .add_characteristic(
                characteristic::AUDIO_INPUT_CONTROL_POINT,
                &[CharacteristicProp::Write],
                AudioInputControlPointOperation::default(),
                store.audio_input_control_point,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let audio_input_description = service
            .add_characteristic(
                characteristic::AUDIO_INPUT_DESCRIPTION,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                description,
                store.audio_input_description,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self {
            handle: service.build(),
            audio_input_state,
            gain_settings_attribute,
            audio_input_type,
            audio_input_status,
            audio_input_control_point,
            audio_input_description,
        }
    }

    pub fn audio_input_state(&self) -> &Characteristic<AudioInputState> {
        &self.audio_input_state
    }
    pub fn gain_settings_attribute(&self) -> GainSettingsAttribute {
        self.gain_settings_attribute
    }
    pub fn audio_input_type(&self) -> &Characteristic<AudioInputType> {
        &self.audio_input_type
    }
    pub fn audio_input_status(&self) -> &Characteristic<AudioInputStatus> {
        &self.audio_input_status
    }
    pub fn audio_input_control_point(&self) -> &Characteristic<AudioInputControlPointOperation> {
        &self.audio_input_control_point
    }
    pub fn audio_input_description(&self) -> &Characteristic<heapless::String<32>> {
        &self.audio_input_description
    }
}

impl<P: PacketPool> LeAudioServerService<P> for AicsServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        let handles = [
            self.audio_input_state.handle,
            self.audio_input_type.handle,
            self.audio_input_status.handle,
            self.audio_input_description.handle,
        ];
        if handles.contains(&event.handle()) {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.audio_input_description.handle {
            return Some(match event.value(&self.audio_input_description) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        // The Control Point's business rule needs the current `AudioInputState`, so
        // `Server::handle` special-cases it before it would otherwise reach this generic path -
        // the same way it special-cases VCS's Volume Control Point.
        if event.handle() == self.audio_input_control_point.handle {
            return Some(match event.value(&self.audio_input_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Applies an Audio Input Control Point write end to end: decodes the raw write value, applies it
/// against the current `AudioInputState` (see [`apply_input_operation`]), and - on success -
/// stores and notifies the new state. Returns the `AttErrorCode` the ATT write itself should be
/// rejected with on failure, or `Ok(())` if it should be accepted. Mirrors
/// `vcs::drive_volume_control_point`.
pub async fn drive_input_control_point<M: RawMutex, P: PacketPool, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    aics: &AicsServer,
    conn: &trouble_host::gatt::GattConnection<'_, '_, P>,
    operation: &AudioInputControlPointOperation,
) -> Result<(), AttErrorCode> {
    let current = aics.audio_input_state.get(server).map_err(|_| AttErrorCode::UNLIKELY_ERROR)?;
    let new_state = apply_input_operation(current, aics.gain_settings_attribute, *operation)?;
    let _ = aics.audio_input_state.notify(conn, &new_state, true).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn state(gain: i8, mute: Mute, mode: GainMode, cc: u8) -> AudioInputState {
        AudioInputState { gain_setting: gain, mute, gain_mode: mode, change_counter: cc }
    }

    const LIMITS: GainSettingsAttribute = GainSettingsAttribute { units: 1, minimum: -20, maximum: 20 };

    #[test]
    fn audio_input_state_round_trips() {
        let s = state(-5, Mute::Muted, GainMode::Automatic, 3);
        assert_eq!(AudioInputState::from_gatt(s.as_gatt()).unwrap(), s);
    }

    #[test]
    fn gain_settings_attribute_round_trips() {
        assert_eq!(GainSettingsAttribute::from_gatt(LIMITS.as_gatt()).unwrap(), LIMITS);
    }

    #[test]
    fn control_point_operation_round_trips_every_opcode() {
        for opcode in [
            AudioInputControlPointOpcode::SetGainSetting(-10),
            AudioInputControlPointOpcode::Unmute,
            AudioInputControlPointOpcode::Mute,
            AudioInputControlPointOpcode::SetManualGainMode,
            AudioInputControlPointOpcode::SetAutomaticGainMode,
        ] {
            let op = AudioInputControlPointOperation::new(opcode, 9);
            assert_eq!(AudioInputControlPointOperation::from_gatt(op.as_gatt()).unwrap(), op);
        }
    }

    #[test]
    fn apply_operation_rejects_stale_change_counter() {
        let current = state(0, Mute::NotMuted, GainMode::Manual, 5);
        let op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::Mute, 4);
        assert_eq!(apply_input_operation(current, LIMITS, op), Err(INVALID_CHANGE_COUNTER));
    }

    #[test]
    fn set_gain_setting_rejects_out_of_range_values() {
        let current = state(0, Mute::NotMuted, GainMode::Manual, 0);
        let op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::SetGainSetting(21), 0);
        assert_eq!(apply_input_operation(current, LIMITS, op), Err(VALUE_OUT_OF_RANGE));
    }

    #[test]
    fn set_gain_setting_accepts_boundary_values() {
        let current = state(0, Mute::NotMuted, GainMode::Manual, 0);
        let op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::SetGainSetting(20), 0);
        assert_eq!(apply_input_operation(current, LIMITS, op).unwrap().gain_setting, 20);
    }

    #[test]
    fn mute_and_unmute_are_rejected_while_disabled() {
        let current = state(0, Mute::Disabled, GainMode::Manual, 0);
        let mute_op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::Mute, 0);
        assert_eq!(apply_input_operation(current, LIMITS, mute_op), Err(MUTE_DISABLED));
        let unmute_op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::Unmute, 0);
        assert_eq!(apply_input_operation(current, LIMITS, unmute_op), Err(MUTE_DISABLED));
    }

    #[test]
    fn set_gain_mode_opcodes_are_rejected_when_mode_is_fixed() {
        let manual_only = state(0, Mute::NotMuted, GainMode::ManualOnly, 0);
        let op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::SetAutomaticGainMode, 0);
        assert_eq!(apply_input_operation(manual_only, LIMITS, op), Err(OPCODE_NOT_SUPPORTED));
    }

    #[test]
    fn set_gain_mode_opcodes_succeed_when_mode_is_switchable() {
        let current = state(0, Mute::NotMuted, GainMode::Manual, 2);
        let op = AudioInputControlPointOperation::new(AudioInputControlPointOpcode::SetAutomaticGainMode, 2);
        let next = apply_input_operation(current, LIMITS, op).unwrap();
        assert_eq!(next.gain_mode, GainMode::Automatic);
        assert_eq!(next.change_counter, 3);
    }

    #[test]
    fn aics_server_exposes_characteristics_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static STATE: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static GAIN_LIMITS: static_cell::StaticCell<[u8; 3]> = static_cell::StaticCell::new();
        static TYPE: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static STATUS: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static CP: static_cell::StaticCell<[u8; 3]> = static_cell::StaticCell::new();
        static DESC: static_cell::StaticCell<[u8; 32]> = static_cell::StaticCell::new();

        let aics = AicsServer::new(
            &mut table,
            state(0, Mute::NotMuted, GainMode::Manual, 0),
            LIMITS,
            AudioInputType::Microphone,
            AudioInputStatus::Active,
            heapless::String::try_from("Mic").unwrap(),
            GAIN_LIMITS.init([0; 3]),
            TYPE.init([0; 1]),
            AicsStore {
                audio_input_state: STATE.init([0; 4]),
                audio_input_type: &mut [],
                audio_input_status: STATUS.init([0; 1]),
                audio_input_control_point: CP.init([0; 3]),
                audio_input_description: DESC.init([0; 32]),
            },
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(aics.audio_input_state().get(&server).unwrap(), state(0, Mute::NotMuted, GainMode::Manual, 0));
        assert_eq!(aics.audio_input_type().get(&server).unwrap(), AudioInputType::Microphone);
        assert_eq!(aics.audio_input_status().get(&server).unwrap(), AudioInputStatus::Active);
        assert_eq!(aics.gain_settings_attribute(), LIMITS);
    }
}
