//! ## Gaming Audio Service
//!
//! The Gaming Audio Service (GMAS) is GMAP's one GATT contribution: a read-only advertisement of
//! which GMAP roles (and per-role optional features) this device supports. The rest of GMAP is a
//! profile (procedures/QoS policy layered on PACS/ASCS/BAP/CSIP/VCP), not additional GATT servers.
//!
//! Role/feature bit layouts are reconstructed from public documentation, not cross-checked
//! against the official Bluetooth SIG assigned-numbers PDF - verify before relying on the exact
//! bit positions for certification.

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
    /// GMAP Role characteristic value (GMAS): which GMAP roles this device implements.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GmapRole: u8 {
        /// Unicast Game Gateway.
        const UGG = 1 << 0;
        /// Unicast Game Terminal.
        const UGT = 1 << 1;
        /// Broadcast Game Sender.
        const BGS = 1 << 2;
        /// Broadcast Game Receiver.
        const BGR = 1 << 3;
    }
}

bitflags! {
    /// UGG Features characteristic value - only meaningful (and only present) when
    /// [`GmapRole::UGG`] is set.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UggFeatures: u8 {
        const Multiplex = 1 << 0;
        const Kbps96Source = 1 << 1;
        const Multisink = 1 << 2;
    }
}

bitflags! {
    /// UGT Features characteristic value - only meaningful (and only present) when
    /// [`GmapRole::UGT`] is set.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UgtFeatures: u8 {
        const Source = 1 << 0;
        const Kbps80Source = 1 << 1;
        const Multiplex = 1 << 2;
        const Kbps96Source = 1 << 3;
        const Multisink = 1 << 4;
    }
}

bitflags! {
    /// BGS Features characteristic value - only meaningful (and only present) when
    /// [`GmapRole::BGS`] is set.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct BgsFeatures: u8 {
        const Kbps96Source = 1 << 0;
    }
}

bitflags! {
    /// BGR Features characteristic value - only meaningful (and only present) when
    /// [`GmapRole::BGR`] is set.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct BgrFeatures: u8 {
        const Multisink = 1 << 0;
        const Multiplex = 1 << 1;
    }
}

macro_rules! u8_bitflags_gatt {
    ($ty:ty) => {
        impl AsGatt for $ty {
            const MIN_SIZE: usize = 1;
            const MAX_SIZE: usize = 1;

            fn as_gatt(&self) -> &[u8] {
                // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u8 here).
                unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
            }
        }

        impl FromGatt for $ty {
            fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
                if data.len() != 1 {
                    return Err(FromGattError::InvalidLength);
                }
                Ok(Self::from_bits_truncate(data[0]))
            }
        }

        impl FixedGattValue for $ty {
            const SIZE: usize = 1;
        }
    };
}

u8_bitflags_gatt!(GmapRole);
u8_bitflags_gatt!(UggFeatures);
u8_bitflags_gatt!(UgtFeatures);
u8_bitflags_gatt!(BgsFeatures);
u8_bitflags_gatt!(BgrFeatures);

/// A Gatt service client for reading a device's GMAP role/feature support.
pub struct GmasClient {
    handle: ServiceHandle,
    pub role: Characteristic<GmapRole>,
    pub ugg_features: Option<Characteristic<UggFeatures>>,
    pub ugt_features: Option<Characteristic<UgtFeatures>>,
    pub bgs_features: Option<Characteristic<BgsFeatures>>,
    pub bgr_features: Option<Characteristic<BgrFeatures>>,
}

impl GmasClient {
    /// Discovers the GMAS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose GMAS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client.services_by_uuid(&Uuid::from(service::GAMING_AUDIO)).await.ok()?;
        let handle = services.first()?;

        let role = client.characteristic_by_uuid(handle, &Uuid::from(characteristic::GMAP_ROLE)).await.ok()?;
        let ugg_features = client.characteristic_by_uuid(handle, &Uuid::from(characteristic::UGG_FEATURES)).await.ok();
        let ugt_features = client.characteristic_by_uuid(handle, &Uuid::from(characteristic::UGT_FEATURES)).await.ok();
        let bgs_features = client.characteristic_by_uuid(handle, &Uuid::from(characteristic::BGS_FEATURES)).await.ok();
        let bgr_features = client.characteristic_by_uuid(handle, &Uuid::from(characteristic::BGR_FEATURES)).await.ok();

        Some(Self {
            handle: handle.clone(),
            role,
            ugg_features,
            ugt_features,
            bgs_features,
            bgr_features,
        })
    }
}

/// A Gatt service server exposing this device's GMAP role/feature support.
pub struct GmasServer {
    handle: u16,
    role: Characteristic<GmapRole>,
    ugg_features: Option<Characteristic<UggFeatures>>,
    ugt_features: Option<Characteristic<UgtFeatures>>,
    bgs_features: Option<Characteristic<BgsFeatures>>,
    bgr_features: Option<Characteristic<BgrFeatures>>,
}

pub const GMAS_ATTRIBUTES: usize = 11;

impl GmasServer {
    /// Creates a new GMAS Gatt service. Each `*_features` argument should be `Some` iff the
    /// corresponding [`GmapRole`] bit is set in `role`; this isn't enforced, since a peripheral
    /// under test may deliberately want to exercise the malformed case.
    #[allow(clippy::too_many_arguments)]
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        role: GmapRole,
        ugg_features: Option<UggFeatures>,
        ugt_features: Option<UgtFeatures>,
        bgs_features: Option<BgsFeatures>,
        bgr_features: Option<BgrFeatures>,
        role_store: &'a mut [u8],
        ugg_store: &'a mut [u8],
        ugt_store: &'a mut [u8],
        bgs_store: &'a mut [u8],
        bgr_store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::GAMING_AUDIO));

        let role = service
            .add_characteristic(characteristic::GMAP_ROLE, &[CharacteristicProp::Read], role, role_store)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let ugg_features = ugg_features.map(|v| {
            service
                .add_characteristic(characteristic::UGG_FEATURES, &[CharacteristicProp::Read], v, ugg_store)
                .read_permission(PermissionLevel::EncryptionRequired)
                .build()
        });
        let ugt_features = ugt_features.map(|v| {
            service
                .add_characteristic(characteristic::UGT_FEATURES, &[CharacteristicProp::Read], v, ugt_store)
                .read_permission(PermissionLevel::EncryptionRequired)
                .build()
        });
        let bgs_features = bgs_features.map(|v| {
            service
                .add_characteristic(characteristic::BGS_FEATURES, &[CharacteristicProp::Read], v, bgs_store)
                .read_permission(PermissionLevel::EncryptionRequired)
                .build()
        });
        let bgr_features = bgr_features.map(|v| {
            service
                .add_characteristic(characteristic::BGR_FEATURES, &[CharacteristicProp::Read], v, bgr_store)
                .read_permission(PermissionLevel::EncryptionRequired)
                .build()
        });

        Self {
            handle: service.build(),
            role,
            ugg_features,
            ugt_features,
            bgs_features,
            bgr_features,
        }
    }

    pub fn role(&self) -> &Characteristic<GmapRole> {
        &self.role
    }
}

impl<P: PacketPool> LeAudioServerService<P> for GmasServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.role.handle {
            return Some(Ok(()));
        }
        for characteristic in [
            self.ugg_features.as_ref().map(|c| c.handle),
            self.ugt_features.as_ref().map(|c| c.handle),
            self.bgs_features.as_ref().map(|c| c.handle),
            self.bgr_features.as_ref().map(|c| c.handle),
        ]
        .into_iter()
        .flatten()
        {
            if event.handle() == characteristic {
                return Some(Ok(()));
            }
        }
        None
    }

    fn handle_write_event(&self, _event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        // Every GMAS characteristic is read-only; none of them are ever a valid write target, so
        // there's nothing to recognize here.
        None
    }
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    #[test]
    fn gmap_role_round_trips() {
        let role = GmapRole::UGG | GmapRole::BGR;
        assert_eq!(GmapRole::from_gatt(role.as_gatt()).unwrap(), role);
    }

    #[test]
    fn gmas_server_exposes_role_and_conditional_features_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static ROLE: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static UGG: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static UGT: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static BGS: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static BGR: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();

        let gmas = GmasServer::new(
            &mut table,
            GmapRole::UGT,
            None,
            Some(UgtFeatures::Multiplex | UgtFeatures::Kbps96Source),
            None,
            None,
            ROLE.init([0; 1]),
            UGG.init([0; 1]),
            UGT.init([0; 1]),
            BGS.init([0; 1]),
            BGR.init([0; 1]),
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(gmas.role().get(&server).unwrap(), GmapRole::UGT);
        assert!(gmas.ugg_features.is_none());
        assert_eq!(gmas.ugt_features.unwrap().get(&server).unwrap(), UgtFeatures::Multiplex | UgtFeatures::Kbps96Source);
    }
}
