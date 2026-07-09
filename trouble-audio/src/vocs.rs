//! ## Volume Offset Control Service
//!
//! The Volume Offset Control Service (VOCS) exposes a per-audio-output volume trim (e.g. balance
//! between two earbuds in a coordinated set). Like AICS, VOCS is normally exposed as an
//! *included* service of VCS (one instance per output); this crate exposes it standalone instead
//! - see the analogous note atop aics.rs for why.

use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{GattConnection, ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

pub use crate::generic_audio::AudioLocation;
use crate::{LeAudioServerService, MAX_SERVICES};

/// Application error code (VOCS): the Change_Counter in a Volume Offset Control Point write
/// didn't match the Volume Offset State characteristic's current Change_Counter.
pub const INVALID_CHANGE_COUNTER: AttErrorCode = AttErrorCode::new(0x80);

/// The Volume Offset State characteristic value (VOCS): Volume_Offset (2, signed LE) |
/// Change_Counter (1).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct VolumeOffsetState {
    pub volume_offset: i16,
    pub change_counter: u8,
}

impl AsGatt for VolumeOffsetState {
    const MIN_SIZE: usize = 3;
    const MAX_SIZE: usize = 3;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]` places `volume_offset` (2 bytes, LE on this crate's supported
        // targets) at offset 0 and `change_counter` immediately after at offset 2 with no gap (a
        // `u8` field needs no alignment) - exactly the 3-byte wire layout. `size_of::<Self>()`
        // may be padded to 4 to satisfy `i16`'s alignment, but slicing exactly 3 (not
        // `size_of`) reads only the two real fields, never any trailing padding byte.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 3) }
    }
}

impl FromGatt for VolumeOffsetState {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 3 {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            volume_offset: i16::from_le_bytes([data[0], data[1]]),
            change_counter: data[2],
        })
    }
}

impl FixedGattValue for VolumeOffsetState {
    const SIZE: usize = 3;
}

/// The value written to the Volume Offset Control Point characteristic: Opcode (1, always `0x01`
/// - Set Volume Offset is VOCS's only defined opcode) | Change_Counter (1) | Volume_Offset (2,
/// signed LE).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SetVolumeOffset {
    opcode: u8,
    pub change_counter: u8,
    pub volume_offset: i16,
}

const SET_VOLUME_OFFSET_OPCODE: u8 = 0x01;

impl SetVolumeOffset {
    pub fn new(change_counter: u8, volume_offset: i16) -> Self {
        Self { opcode: SET_VOLUME_OFFSET_OPCODE, change_counter, volume_offset }
    }
}

impl AsGatt for SetVolumeOffset {
    const MIN_SIZE: usize = 4;
    const MAX_SIZE: usize = 4;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`: `opcode` (1) | `change_counter` (1) at offsets 0/1 with no gap,
        // then `volume_offset` (i16, needing 2-byte alignment) at offset 2 - already aligned, so
        // no padding is inserted anywhere in this 4-byte layout (unlike `VolumeOffsetState`,
        // whose trailing `u8` forces the compiler to pad the *end* of the struct instead).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 4) }
    }
}

impl FromGatt for SetVolumeOffset {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 4 {
            return Err(FromGattError::InvalidLength);
        }
        if data[0] != SET_VOLUME_OFFSET_OPCODE {
            return Err(FromGattError::InvalidLength);
        }
        Ok(Self {
            opcode: data[0],
            change_counter: data[1],
            volume_offset: i16::from_le_bytes([data[2], data[3]]),
        })
    }
}

impl Default for SetVolumeOffset {
    /// Only used to seed the characteristic's backing store at construction time.
    fn default() -> Self {
        Self::new(0, 0)
    }
}

/// Applies a Set Volume Offset operation to `current`, per VOCS's Volume Offset Control Point
/// procedure requirements.
pub fn apply_volume_offset_operation(current: VolumeOffsetState, operation: SetVolumeOffset) -> Result<VolumeOffsetState, AttErrorCode> {
    if operation.change_counter != current.change_counter {
        return Err(INVALID_CHANGE_COUNTER);
    }
    Ok(VolumeOffsetState {
        volume_offset: operation.volume_offset,
        change_counter: current.change_counter.wrapping_add(1),
    })
}

/// A Gatt service client for reading/controlling one audio output's volume offset.
pub struct VocsClient {
    handle: ServiceHandle,
    pub volume_offset_state: Characteristic<VolumeOffsetState>,
    pub audio_location: Characteristic<AudioLocation>,
    pub volume_offset_control_point: Characteristic<SetVolumeOffset>,
    pub audio_output_description: Characteristic<heapless::String<32>>,
}

impl VocsClient {
    /// Discovers the VOCS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose VOCS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::VOLUME_OFFSET_CONTROL)).await.ok()?;
        let handle = services.first()?;

        macro_rules! required {
            ($uuid:expr) => {
                client.characteristic_by_uuid(handle, &Uuid::from($uuid)).await.ok()?
            };
        }

        Some(Self {
            handle: handle.clone(),
            volume_offset_state: required!(characteristic::VOLUME_OFFSET_STATE),
            audio_location: required!(characteristic::AUDIO_LOCATION),
            volume_offset_control_point: required!(characteristic::VOLUME_OFFSET_CONTROL_POINT),
            audio_output_description: required!(characteristic::AUDIO_OUTPUT_DESCRIPTION),
        })
    }
}

/// Backing storage buffers for [`VocsServer::new`].
pub struct VocsStore<'a> {
    pub volume_offset_state: &'a mut [u8],
    pub audio_location: &'a mut [u8],
    pub volume_offset_control_point: &'a mut [u8],
    pub audio_output_description: &'a mut [u8],
}

/// A Gatt service server exposing control over one audio output's volume offset.
pub struct VocsServer {
    handle: u16,
    volume_offset_state: Characteristic<VolumeOffsetState>,
    audio_location: Characteristic<AudioLocation>,
    volume_offset_control_point: Characteristic<SetVolumeOffset>,
    audio_output_description: Characteristic<heapless::String<32>>,
}

pub const VOCS_ATTRIBUTES: usize = 12;

impl VocsServer {
    /// Creates a new VOCS Gatt service.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        initial_state: VolumeOffsetState,
        audio_location: AudioLocation,
        description: heapless::String<32>,
        store: VocsStore<'a>,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::VOLUME_OFFSET_CONTROL));

        let volume_offset_state = service
            .add_characteristic(
                characteristic::VOLUME_OFFSET_STATE,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                initial_state,
                store.volume_offset_state,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let audio_location = service
            .add_characteristic(
                characteristic::AUDIO_LOCATION,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                audio_location,
                store.audio_location,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let volume_offset_control_point = service
            .add_characteristic(
                characteristic::VOLUME_OFFSET_CONTROL_POINT,
                &[CharacteristicProp::Write],
                SetVolumeOffset::default(),
                store.volume_offset_control_point,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let audio_output_description = service
            .add_characteristic(
                characteristic::AUDIO_OUTPUT_DESCRIPTION,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                description,
                store.audio_output_description,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self {
            handle: service.build(),
            volume_offset_state,
            audio_location,
            volume_offset_control_point,
            audio_output_description,
        }
    }

    pub fn volume_offset_state(&self) -> &Characteristic<VolumeOffsetState> {
        &self.volume_offset_state
    }
    pub fn audio_location(&self) -> &Characteristic<AudioLocation> {
        &self.audio_location
    }
    pub fn volume_offset_control_point(&self) -> &Characteristic<SetVolumeOffset> {
        &self.volume_offset_control_point
    }
    pub fn audio_output_description(&self) -> &Characteristic<heapless::String<32>> {
        &self.audio_output_description
    }
}

impl<P: PacketPool> LeAudioServerService<P> for VocsServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        let handles = [self.volume_offset_state.handle, self.audio_location.handle, self.audio_output_description.handle];
        if handles.contains(&event.handle()) {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.audio_output_description.handle {
            return Some(match event.value(&self.audio_output_description) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        // The Control Point's business rule needs the current `VolumeOffsetState`, so
        // `Server::handle` special-cases it before it would otherwise reach this generic path -
        // the same way it special-cases VCS's Volume Control Point.
        if event.handle() == self.volume_offset_control_point.handle {
            return Some(match event.value(&self.volume_offset_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Applies a Volume Offset Control Point write end to end: decodes the raw write value, applies
/// it against the current `VolumeOffsetState` (see [`apply_volume_offset_operation`]), and - on
/// success - stores and notifies the new state. Mirrors `vcs::drive_volume_control_point`.
pub async fn drive_volume_offset_control_point<M: RawMutex, P: PacketPool, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    vocs: &VocsServer,
    conn: &GattConnection<'_, '_, P>,
    operation: &SetVolumeOffset,
) -> Result<(), AttErrorCode> {
    let current = vocs.volume_offset_state.get(server).map_err(|_| AttErrorCode::UNLIKELY_ERROR)?;
    let new_state = apply_volume_offset_operation(current, *operation)?;
    let _ = vocs.volume_offset_state.notify(conn, &new_state, true).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    #[test]
    fn volume_offset_state_round_trips() {
        let s = VolumeOffsetState { volume_offset: -200, change_counter: 7 };
        assert_eq!(VolumeOffsetState::from_gatt(s.as_gatt()).unwrap(), s);
    }

    #[test]
    fn set_volume_offset_round_trips() {
        let op = SetVolumeOffset::new(3, -55);
        assert_eq!(SetVolumeOffset::from_gatt(op.as_gatt()).unwrap(), op);
    }

    #[test]
    fn set_volume_offset_rejects_wrong_opcode() {
        assert!(SetVolumeOffset::from_gatt(&[0x02, 0, 0, 0]).is_err());
    }

    #[test]
    fn apply_operation_rejects_stale_change_counter() {
        let current = VolumeOffsetState { volume_offset: 0, change_counter: 5 };
        let op = SetVolumeOffset::new(4, 100);
        assert_eq!(apply_volume_offset_operation(current, op), Err(INVALID_CHANGE_COUNTER));
    }

    #[test]
    fn apply_operation_sets_offset_and_advances_change_counter() {
        let current = VolumeOffsetState { volume_offset: 0, change_counter: 5 };
        let op = SetVolumeOffset::new(5, -128);
        let next = apply_volume_offset_operation(current, op).unwrap();
        assert_eq!(next, VolumeOffsetState { volume_offset: -128, change_counter: 6 });
    }

    #[test]
    fn vocs_server_exposes_characteristics_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static STATE: static_cell::StaticCell<[u8; 3]> = static_cell::StaticCell::new();
        static LOC: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static CP: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static DESC: static_cell::StaticCell<[u8; 32]> = static_cell::StaticCell::new();

        let vocs = VocsServer::new(
            &mut table,
            VolumeOffsetState { volume_offset: 10, change_counter: 0 },
            AudioLocation::FrontLeft,
            heapless::String::try_from("Left").unwrap(),
            VocsStore {
                volume_offset_state: STATE.init([0; 3]),
                audio_location: LOC.init([0; 4]),
                volume_offset_control_point: CP.init([0; 4]),
                audio_output_description: DESC.init([0; 32]),
            },
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(vocs.volume_offset_state().get(&server).unwrap(), VolumeOffsetState { volume_offset: 10, change_counter: 0 });
        assert_eq!(vocs.audio_location().get(&server).unwrap(), AudioLocation::FrontLeft);
    }
}
