//! ## Telephony and Media Audio Service
//!
//! The Telephony and Media Audio Service (TMAS) is TMAP's one GATT contribution: a read-only
//! advertisement of which TMAP roles this device supports. The rest of TMAP is a profile
//! (procedures/QoS policy layered on PACS/ASCS/BAP/MCP/CCP), not additional GATT servers.
//!
//! Role bit layout is reconstructed from public documentation, not cross-checked against the
//! official Bluetooth SIG assigned-numbers PDF - verify before relying on the exact bit
//! positions for certification.

use bitflags::bitflags;
use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{LeAudioServerService, MAX_SERVICES};

bitflags! {
    /// TMAP Role characteristic value (TMAS): which TMAP roles this device implements.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TmapRole: u16 {
        /// Call Gateway.
        const CallGateway = 1 << 0;
        /// Call Terminal.
        const CallTerminal = 1 << 1;
        /// Unicast Media Sender.
        const UnicastMediaSender = 1 << 2;
        /// Unicast Media Receiver.
        const UnicastMediaReceiver = 1 << 3;
        /// Broadcast Media Sender.
        const BroadcastMediaSender = 1 << 4;
        /// Broadcast Media Receiver.
        const BroadcastMediaReceiver = 1 << 5;
    }
}

impl AsGatt for TmapRole {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 2;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u16 here).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 2) }
    }
}

impl FromGatt for TmapRole {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 2] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u16::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for TmapRole {
    const SIZE: usize = 2;
}

/// A Gatt service client for reading a device's TMAP role support.
pub struct TmasClient {
    handle: ServiceHandle,
    pub role: Characteristic<TmapRole>,
}

impl TmasClient {
    /// Discovers the TMAS service and its role characteristic on an already-connected `GattClient`.
    pub async fn new<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Self {
        let services = client
            .services_by_uuid(&Uuid::from(service::TELEPHONY_AND_MEDIA_AUDIO))
            .await
            .unwrap();
        let handle = services.first().unwrap();

        let role = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::TMAP_ROLE))
            .await
            .expect("TMAP Role is mandatory on TMAS");

        Self { handle: handle.clone(), role }
    }
}

/// A Gatt service server exposing this device's TMAP role support.
pub struct TmasServer {
    handle: u16,
    role: Characteristic<TmapRole>,
}

pub const TMAS_ATTRIBUTES: usize = 3;

impl TmasServer {
    /// Creates a new TMAS Gatt service.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        role: TmapRole,
        store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::TELEPHONY_AND_MEDIA_AUDIO));

        let role = service
            .add_characteristic(characteristic::TMAP_ROLE, &[CharacteristicProp::Read], role, store)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        Self { handle: service.build(), role }
    }

    pub fn role(&self) -> &Characteristic<TmapRole> {
        &self.role
    }
}

impl<P: PacketPool> LeAudioServerService<P> for TmasServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.role.handle {
            return Some(Ok(()));
        }
        None
    }

    fn handle_write_event(&self, _event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    #[test]
    fn tmap_role_round_trips() {
        let role = TmapRole::UnicastMediaSender | TmapRole::UnicastMediaReceiver;
        assert_eq!(TmapRole::from_gatt(role.as_gatt()).unwrap(), role);
    }

    #[test]
    fn tmas_server_exposes_role_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static ROLE: static_cell::StaticCell<[u8; 2]> = static_cell::StaticCell::new();
        let tmas = TmasServer::new(&mut table, TmapRole::CallGateway | TmapRole::CallTerminal, ROLE.init([0; 2]));
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(tmas.role().get(&server).unwrap(), TmapRole::CallGateway | TmapRole::CallTerminal);
    }
}
