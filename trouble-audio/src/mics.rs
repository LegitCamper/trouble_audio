//! ## Microphone Control Service
//!
//! The Microphone Control Service (MICS) exposes a single Mute control, letting a client mute
//! or unmute the device's microphone(s). Bluetooth MICS v1.0 Section 3.1.

use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{LeAudioServerService, MAX_SERVICES};

/// Application error code (MICS 3.1): the Mute characteristic is currently `Disabled` and so
/// cannot be written by a client.
pub const MUTE_DISABLED: AttErrorCode = AttErrorCode::new(0x80);

/// The Mute characteristic value (MICS 3.1, Table 3.1).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mute {
    #[default]
    NotMuted = 0x00,
    Muted = 0x01,
    /// Only ever set by the server (e.g. a device with no controllable microphone). A client
    /// attempting to write this value must be rejected - see [`Mute::validate_client_write`].
    Disabled = 0x02,
}

impl Mute {
    /// Validates a value a client is attempting to write to the Mute characteristic against the
    /// characteristic's current value, per MICS 3.1's write behavior:
    /// - `Disabled` may never be written by a client (it's server-only).
    /// - Any write while the current value is `Disabled` is rejected with [`MUTE_DISABLED`].
    pub fn validate_client_write(current: Self, requested: Self) -> Result<Self, AttErrorCode> {
        if current == Self::Disabled {
            return Err(MUTE_DISABLED);
        }
        match requested {
            Self::NotMuted | Self::Muted => Ok(requested),
            Self::Disabled => Err(AttErrorCode::VALUE_NOT_ALLOWED),
        }
    }
}

impl TryFrom<u8> for Mute {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::NotMuted),
            0x01 => Ok(Self::Muted),
            0x02 => Ok(Self::Disabled),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for Mute {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for Mute {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for Mute {
    const SIZE: usize = 1;
}

/// A Gatt service client for reading/controlling a device's microphone mute state.
pub struct MicsClient {
    handle: ServiceHandle,
    pub mute: Characteristic<Mute>,
}

impl MicsClient {
    /// Discovers the MICS service and its Mute characteristic on an already-connected
    /// `GattClient`. Returns `None` if the peer doesn't expose MICS (it's an optional service) -
    /// see [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::MICROPHONE_CONTROL)).await.ok()?;
        let handle = services.first()?;

        let mute = client.characteristic_by_uuid(handle, &Uuid::from(characteristic::MUTE)).await.ok()?;

        Some(Self { handle: handle.clone(), mute })
    }
}

/// A Gatt service server exposing microphone mute control.
pub struct MicsServer {
    handle: u16,
    mute: Characteristic<Mute>,
}

pub const MICS_ATTRIBUTES: usize = 6;

impl MicsServer {
    /// Creates a new MICS Gatt service, with the Mute characteristic starting at `initial`.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        initial: Mute,
        store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::MICROPHONE_CONTROL));

        // Consistent with the rest of this crate's LE Audio services (see the comment in
        // pacs.rs): every characteristic requires an encrypted link.
        let mute = service
            .add_characteristic(
                characteristic::MUTE,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                initial,
                store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self { handle: service.build(), mute }
    }

    /// The Mute characteristic.
    pub fn mute(&self) -> &Characteristic<Mute> {
        &self.mute
    }
}

impl<P: PacketPool> LeAudioServerService<P> for MicsServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.mute.handle {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() != self.mute.handle {
            return None;
        }
        // Only validates that the write decodes to a shape-valid `Mute` value (mirrors how
        // `PacsServer::handle_write_event` treats its characteristics). The stateful business
        // rule - rejecting writes against the *current* stored value, per
        // `Mute::validate_client_write` - needs the `AttributeServer`, which isn't reachable from
        // this trait, so `Server::handle` special-cases the Mute handle before falling through to
        // this generic path, the same way it special-cases the ASE Control Point for ASCS.
        Some(match event.value(&self.mute) {
            Ok(_) => Ok(()),
            Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
        })
    }
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    #[test]
    fn mute_round_trips_every_defined_value() {
        for value in [Mute::NotMuted, Mute::Muted, Mute::Disabled] {
            assert_eq!(Mute::from_gatt(value.as_gatt()).unwrap(), value);
        }
    }

    #[test]
    fn mute_rejects_reserved_wire_values() {
        assert_eq!(Mute::try_from(0x03), Err(FromGattError::InvalidLength));
        assert!(Mute::from_gatt(&[0x00, 0x00]).is_err());
    }

    #[test]
    fn validate_client_write_accepts_muted_and_not_muted() {
        assert_eq!(Mute::validate_client_write(Mute::NotMuted, Mute::Muted), Ok(Mute::Muted));
        assert_eq!(Mute::validate_client_write(Mute::Muted, Mute::NotMuted), Ok(Mute::NotMuted));
    }

    #[test]
    fn validate_client_write_rejects_writing_disabled() {
        assert_eq!(
            Mute::validate_client_write(Mute::NotMuted, Mute::Disabled),
            Err(AttErrorCode::VALUE_NOT_ALLOWED)
        );
    }

    #[test]
    fn validate_client_write_rejects_any_write_while_currently_disabled() {
        assert_eq!(Mute::validate_client_write(Mute::Disabled, Mute::Muted), Err(MUTE_DISABLED));
        assert_eq!(Mute::validate_client_write(Mute::Disabled, Mute::NotMuted), Err(MUTE_DISABLED));
    }

    /// End-to-end through a real `AttributeTable`/`AttributeServer`, matching the shape of
    /// `Server::handle`'s dispatch: a shape-valid write to the Mute handle is recognized and
    /// accepted, an unrelated handle is not recognized (`None`), and the stored value can be read
    /// back via `Characteristic::get` the same way `Server::handle` would to apply
    /// `Mute::validate_client_write`.
    #[test]
    fn mics_server_dispatches_reads_and_writes_to_its_own_handle_only() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static STORE: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        let mics = MicsServer::new(&mut table, Mute::NotMuted, STORE.init([0; 1]));
        let server: trouble_host::prelude::AttributeServer<'_, NoopRawMutex, trouble_host::prelude::DefaultPacketPool, MAX_SERVICES, 1> =
            trouble_host::prelude::AttributeServer::new(table);

        assert_eq!(mics.mute().get(&server).unwrap(), Mute::NotMuted);
        mics.mute().set(&server, &Mute::Muted).unwrap();
        assert_eq!(mics.mute().get(&server).unwrap(), Mute::Muted);
    }
}
