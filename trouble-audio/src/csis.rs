//! ## Coordinated Set Identification Service
//!
//! The Coordinated Set Identification Service (CSIS) lets a client discover that this device
//! belongs to a coordinated set (e.g. one earbud of a stereo pair) and lock exclusive access to
//! it while performing a multi-step, set-wide procedure. Bluetooth CSIS v1.0 Section 3.
//!
//! Coordination across *multiple* physical devices (resolving each member's identity from the
//! group's SIRK, deciding lock ordering across members) is out of scope for a GATT server - it's
//! the client-side Set Coordinator's job. This module only implements the server's half: storage
//! and access rules for the four characteristics.

use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::{ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{LeAudioServerService, MAX_SERVICES};

/// Application error codes (CSIS 3.4). Unlike VCS/MICS's, these are reconstructed from public
/// documentation rather than cross-checked against the official Bluetooth SIG assigned-numbers
/// PDF - verify against it before relying on the exact values for certification.
pub const LOCK_DENIED: AttErrorCode = AttErrorCode::new(0x80);
pub const RELEASE_NOT_ALLOWED: AttErrorCode = AttErrorCode::new(0x81);
pub const INVALID_LOCK_VALUE: AttErrorCode = AttErrorCode::new(0x82);

/// SIRK_Type (CSIS 3.1, Table 3.2): whether `Sirk::key` is encrypted or given in the clear.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SirkType {
    /// Encrypted using the CSIS "sef" key-obfuscation function keyed on the session LTK, so only
    /// a bonded peer can recover the real SIRK - see [`Sirk::encrypted`]/[`Sirk::decrypt`].
    Encrypted = 0x00,
    Plaintext = 0x01,
}

impl TryFrom<u8> for SirkType {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Encrypted),
            0x01 => Ok(Self::Plaintext),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// The Set Identity Resolving Key characteristic value (CSIS 3.1): SIRK_Type (1) | SIRK (16).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Sirk {
    pub sirk_type: SirkType,
    pub key: [u8; 16],
}

impl Sirk {
    pub fn plaintext(key: [u8; 16]) -> Self {
        Self { sirk_type: SirkType::Plaintext, key }
    }

    /// Builds an Encrypted-type SIRK value: `sirk` obfuscated with [`sef`] keyed on the session's
    /// LTK. Both arrays are in wire (least-significant-octet-first) order, matching this
    /// characteristic's value and `BondInformation`'s LTK.
    pub fn encrypted(sirk: [u8; 16], ltk: [u8; 16]) -> Self {
        Self {
            sirk_type: SirkType::Encrypted,
            key: sef(&ltk, &sirk),
        }
    }

    /// Recovers the plain SIRK: the key itself for a Plaintext value, or [`sdf`] (the inverse of
    /// [`sef`]) applied with the session's LTK for an Encrypted one.
    pub fn decrypt(&self, ltk: [u8; 16]) -> [u8; 16] {
        match self.sirk_type {
            SirkType::Plaintext => self.key,
            SirkType::Encrypted => sdf(&ltk, &self.key),
        }
    }
}

/// AES-CMAC over a spec-order (most-significant-octet-first) key and message.
fn aes_cmac(key_be: &[u8; 16], msg: &[u8]) -> [u8; 16] {
    use cmac::Mac;
    let mut mac = cmac::Cmac::<aes::Aes128>::new_from_slice(key_be).unwrap();
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn reversed(bytes: &[u8; 16]) -> [u8; 16] {
    let mut out = *bytes;
    out.reverse();
    out
}

/// CSIS `s1(M) = AES-CMAC` keyed with 16 zero octets (CSIS 4.3).
fn s1(m: &[u8]) -> [u8; 16] {
    aes_cmac(&[0u8; 16], m)
}

/// CSIS `k1(N, SALT, P) = AES-CMAC_T(P)` with `T = AES-CMAC_SALT(N)` (CSIS 4.4). All values in
/// spec (most-significant-octet-first) order.
fn k1(n: &[u8], salt: &[u8; 16], p: &[u8]) -> [u8; 16] {
    let t = aes_cmac(salt, n);
    aes_cmac(&t, p)
}

/// CSIS SIRK encryption function `sef(K, SIRK) = k1(K, s1("SIRKenc"), "csis") XOR SIRK`
/// (CSIS 4.5). `k` (the session LTK) and `sirk` are in wire (least-significant-octet-first)
/// order, as this crate stores them; the spec-order conversion happens internally.
pub fn sef(k: &[u8; 16], sirk: &[u8; 16]) -> [u8; 16] {
    let mut out = k1(&reversed(k), &s1(b"SIRKenc"), b"csis");
    out.reverse(); // back to wire order
    for (o, s) in out.iter_mut().zip(sirk) {
        *o ^= s;
    }
    out
}

/// CSIS SIRK decryption function `sdf` (CSIS 4.6) - the same XOR construction as [`sef`], so
/// decryption is just re-application.
pub fn sdf(k: &[u8; 16], encrypted_sirk: &[u8; 16]) -> [u8; 16] {
    sef(k, encrypted_sirk)
}

/// CSIS RSI hash function `sih(K, R) = e(K, R') mod 2^24` (CSIS 4.7) - the same construction as
/// the Core spec's RPA hash `ah`. `k`/`prand` and the returned hash are in wire
/// (least-significant-octet-first) order.
pub fn sih(k: &[u8; 16], prand: [u8; 3]) -> [u8; 3] {
    use aes::cipher::{BlockEncrypt, KeyInit};
    // R' in spec order: 13 padding octets, then R with its most significant octet first.
    let mut block = [0u8; 16];
    block[13] = prand[2];
    block[14] = prand[1];
    block[15] = prand[0];
    let cipher = aes::Aes128::new_from_slice(&reversed(k)).unwrap();
    let mut b = aes::Block::clone_from_slice(&block);
    cipher.encrypt_block(&mut b);
    // The 24 least significant bits, returned least-significant-octet first.
    [b[15], b[14], b[13]]
}

/// Builds the 6-octet Resolvable Set Identifier AD value (CSIS 4.8) from this set's SIRK and a
/// caller-supplied 24-bit random `prand` (both least-significant-octet-first): `hash` then
/// `prand`, the same layout as a resolvable private address. Advertise it under the RSI AD type
/// (0x2E); a Set Coordinator resolves it by checking `hash == sih(SIRK, prand)`.
pub fn rsi(sirk: &[u8; 16], prand: [u8; 3]) -> [u8; 6] {
    let hash = sih(sirk, prand);
    [hash[0], hash[1], hash[2], prand[0], prand[1], prand[2]]
}

/// Checks whether an RSI AD value was generated from `sirk` - the Set Coordinator's half of RSI
/// resolution.
pub fn resolve_rsi(sirk: &[u8; 16], rsi: &[u8; 6]) -> bool {
    let prand = [rsi[3], rsi[4], rsi[5]];
    sih(sirk, prand) == [rsi[0], rsi[1], rsi[2]]
}

impl AsGatt for Sirk {
    const MIN_SIZE: usize = 17;
    const MAX_SIZE: usize = 17;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`, `SirkType` is `#[repr(u8)]` and `key` immediately follows with no
        // padding (both 1-byte-aligned), matching the wire layout: SIRK_Type (1) | SIRK (16).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 17) }
    }
}

impl FromGatt for Sirk {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 17 {
            return Err(FromGattError::InvalidLength);
        }
        let sirk_type = SirkType::try_from(data[0])?;
        let key: [u8; 16] = data[1..17].try_into().unwrap();
        Ok(Self { sirk_type, key })
    }
}

impl FixedGattValue for Sirk {
    const SIZE: usize = 17;
}

/// Set Member Lock characteristic value (CSIS 3.1, Table 3.4). `0x00` is RFU and never valid.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Lock {
    #[default]
    Unlocked = 0x01,
    Locked = 0x02,
}

impl TryFrom<u8> for Lock {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Unlocked),
            0x02 => Ok(Self::Locked),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for Lock {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for Lock {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for Lock {
    const SIZE: usize = 1;
}

impl Lock {
    /// Validates a raw Set Member Lock write against the characteristic's current value, per
    /// CSIS 3.1's write behavior. Takes the raw byte (rather than a decoded `Lock`) so an
    /// out-of-range value can be reported as [`INVALID_LOCK_VALUE`] specifically, rather than a
    /// generic decode failure - this crate's one-connection-at-a-time model (see the module
    /// doc) means "Lock Denied" only ever means "already locked", since there's no second client
    /// to contend with.
    pub fn validate_client_write(current: Self, requested_raw: u8) -> Result<Self, AttErrorCode> {
        let requested = Self::try_from(requested_raw).map_err(|_| INVALID_LOCK_VALUE)?;
        match (current, requested) {
            (Self::Unlocked, Self::Locked) => Ok(Self::Locked),
            (Self::Locked, Self::Unlocked) => Ok(Self::Unlocked),
            (Self::Locked, Self::Locked) => Err(LOCK_DENIED),
            (Self::Unlocked, Self::Unlocked) => Err(RELEASE_NOT_ALLOWED),
        }
    }
}

/// A Gatt service client for discovering a device's coordinated-set membership.
pub struct CsisClient {
    handle: ServiceHandle,
    pub sirk: Characteristic<Sirk>,
    pub set_size: Option<Characteristic<u8>>,
    pub lock: Characteristic<Lock>,
    pub rank: Option<Characteristic<u8>>,
}

impl CsisClient {
    /// Discovers the CSIS service and its characteristics on an already-connected `GattClient`.
    /// Returns `None` if the peer doesn't expose CSIS (it's an optional service) - see
    /// [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client
            .services_by_uuid(&Uuid::from(service::COORDINATED_SET_IDENTIFICATION))
            .await
            .ok()?;
        let handle = services.first()?;

        let sirk = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SET_IDENTITY_RESOLVING_KEY))
            .await
            .ok()?;
        let set_size = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::COORDINATED_SET_SIZE))
            .await
            .ok();
        let lock = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SET_MEMBER_LOCK))
            .await
            .ok()?;
        let rank = client
            .characteristic_by_uuid(handle, &Uuid::from(characteristic::SET_MEMBER_RANK))
            .await
            .ok();

        Some(Self {
            handle: handle.clone(),
            sirk,
            set_size,
            lock,
            rank,
        })
    }
}

/// A Gatt service server exposing this device's coordinated-set membership.
pub struct CsisServer {
    handle: u16,
    sirk: Characteristic<Sirk>,
    set_size: Option<Characteristic<u8>>,
    lock: Characteristic<Lock>,
    rank: Option<Characteristic<u8>>,
}

/// Owned backing storage for CSIS's characteristics.
#[derive(Default)]
pub struct CsisStorage {
    pub sirk: [u8; 17],
    pub set_size: [u8; 1],
    pub lock: [u8; 1],
    pub rank: [u8; 1],
}

pub const CSIS_ATTRIBUTES: usize = 14;

impl CsisServer {
    /// Creates a new CSIS Gatt service. `set_size`/`rank` are optional per CSIS 3.1 (present when
    /// this device is part of a set of more than one member).
    #[allow(clippy::too_many_arguments)]
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        sirk: Sirk,
        set_size: Option<u8>,
        lock: Lock,
        rank: Option<u8>,
        sirk_store: &'a mut [u8],
        set_size_store: &'a mut [u8],
        lock_store: &'a mut [u8],
        rank_store: &'a mut [u8],
    ) -> Self {
        let mut service = table.add_service(Service::new(service::COORDINATED_SET_IDENTIFICATION));

        let sirk = service
            .add_characteristic(
                characteristic::SET_IDENTITY_RESOLVING_KEY,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                sirk,
                sirk_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let set_size = set_size.map(|size| {
            service
                .add_characteristic(
                    characteristic::COORDINATED_SET_SIZE,
                    &[CharacteristicProp::Read, CharacteristicProp::Notify],
                    size,
                    set_size_store,
                )
                .read_permission(PermissionLevel::EncryptionRequired)
                .build()
        });

        let lock = service
            .add_characteristic(
                characteristic::SET_MEMBER_LOCK,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                lock,
                lock_store,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let rank = rank.map(|rank| {
            service
                .add_characteristic(
                    characteristic::SET_MEMBER_RANK,
                    &[CharacteristicProp::Read],
                    rank,
                    rank_store,
                )
                .read_permission(PermissionLevel::EncryptionRequired)
                .build()
        });

        Self {
            handle: service.build(),
            sirk,
            set_size,
            lock,
            rank,
        }
    }

    pub fn sirk(&self) -> &Characteristic<Sirk> {
        &self.sirk
    }
    pub fn set_size(&self) -> Option<&Characteristic<u8>> {
        self.set_size.as_ref()
    }
    pub fn lock(&self) -> &Characteristic<Lock> {
        &self.lock
    }
    pub fn rank(&self) -> Option<&Characteristic<u8>> {
        self.rank.as_ref()
    }
}

impl<P: PacketPool> LeAudioServerService<P> for CsisServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.sirk.handle {
            return Some(Ok(()));
        }
        if let Some(set_size) = &self.set_size {
            if event.handle() == set_size.handle {
                return Some(Ok(()));
            }
        }
        if event.handle() == self.lock.handle {
            return Some(Ok(()));
        }
        if let Some(rank) = &self.rank {
            if event.handle() == rank.handle {
                return Some(Ok(()));
            }
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        // The Set Member Lock's business rule needs the current stored `Lock` (and must report
        // `INVALID_LOCK_VALUE` for out-of-range bytes rather than a generic decode failure), so
        // `Server::handle` special-cases it (via `Lock::validate_client_write`) before it would
        // otherwise reach this generic path - the same way it special-cases the ASE Control Point
        // for ASCS.
        if event.handle() == self.sirk.handle {
            return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
        }
        if let Some(set_size) = &self.set_size {
            if event.handle() == set_size.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
        }
        if let Some(rank) = &self.rank {
            if event.handle() == rank.handle {
                return Some(Err(AttErrorCode::WRITE_NOT_PERMITTED));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    #[test]
    fn sirk_round_trips() {
        let sirk = Sirk::plaintext([0xAB; 16]);
        assert_eq!(Sirk::from_gatt(sirk.as_gatt()).unwrap(), sirk);
    }

    #[test]
    fn sirk_rejects_wrong_length() {
        assert!(Sirk::from_gatt(&[0x01; 16]).is_err());
        assert!(Sirk::from_gatt(&[0x01; 18]).is_err());
    }

    #[test]
    fn sirk_rejects_invalid_type_byte() {
        let mut bytes = [0u8; 17];
        bytes[0] = 0x02;
        assert!(Sirk::from_gatt(&bytes).is_err());
    }

    #[test]
    fn lock_round_trips() {
        assert_eq!(Lock::from_gatt(Lock::Unlocked.as_gatt()).unwrap(), Lock::Unlocked);
        assert_eq!(Lock::from_gatt(Lock::Locked.as_gatt()).unwrap(), Lock::Locked);
    }

    #[test]
    fn lock_write_unlocks_and_locks() {
        assert_eq!(Lock::validate_client_write(Lock::Unlocked, 0x02), Ok(Lock::Locked));
        assert_eq!(Lock::validate_client_write(Lock::Locked, 0x01), Ok(Lock::Unlocked));
    }

    #[test]
    fn lock_write_rejects_relocking_an_already_locked_set() {
        assert_eq!(Lock::validate_client_write(Lock::Locked, 0x02), Err(LOCK_DENIED));
    }

    #[test]
    fn lock_write_rejects_releasing_an_already_unlocked_set() {
        assert_eq!(Lock::validate_client_write(Lock::Unlocked, 0x01), Err(RELEASE_NOT_ALLOWED));
    }

    #[test]
    fn lock_write_rejects_reserved_and_out_of_range_byte_values() {
        assert_eq!(Lock::validate_client_write(Lock::Unlocked, 0x00), Err(INVALID_LOCK_VALUE));
        assert_eq!(Lock::validate_client_write(Lock::Unlocked, 0x03), Err(INVALID_LOCK_VALUE));
    }

    #[test]
    fn csis_server_exposes_sirk_size_lock_and_rank_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();
        static SIRK: static_cell::StaticCell<[u8; 17]> = static_cell::StaticCell::new();
        static SIZE: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static LOCK: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static RANK: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        let csis = CsisServer::new(
            &mut table,
            Sirk::plaintext([0x11; 16]),
            Some(2),
            Lock::Unlocked,
            Some(1),
            SIRK.init([0; 17]),
            SIZE.init([0; 1]),
            LOCK.init([0; 1]),
            RANK.init([0; 1]),
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(csis.sirk().get(&server).unwrap(), Sirk::plaintext([0x11; 16]));
        assert_eq!(csis.set_size().unwrap().get(&server).unwrap(), 2);
        assert_eq!(csis.lock().get(&server).unwrap(), Lock::Unlocked);
        csis.lock().set(&server, &Lock::Locked).unwrap();
        assert_eq!(csis.lock().get(&server).unwrap(), Lock::Locked);
    }

    /// RFC 4493's first two AES-CMAC test vectors, proving the `aes`/`cmac` wiring (key order,
    /// finalization) underlying s1/k1/sef.
    #[test]
    fn aes_cmac_matches_rfc_4493_vectors() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c,
        ];
        assert_eq!(
            aes_cmac(&key, &[]),
            [0xbb, 0x1d, 0x69, 0x29, 0xe9, 0x59, 0x37, 0x28, 0x7f, 0xa3, 0x7d, 0x12, 0x9b, 0x75, 0x67, 0x46]
        );
        let msg = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17, 0x2a,
        ];
        assert_eq!(
            aes_cmac(&key, &msg),
            [0x07, 0x0a, 0x16, 0xb4, 0x6b, 0x4d, 0x41, 0x44, 0xf7, 0x9b, 0xdd, 0x9d, 0xd0, 0x4a, 0x28, 0x7c]
        );
    }

    /// `sih` is the same construction as the Core spec's RPA hash `ah` (Vol 3 Part H 2.2.2), so
    /// the Core spec's `ah` sample data applies: IRK ec0234a357c8ad05341010a60a397d9b,
    /// prand 0x708194 -> hash 0x0dfbaa. Inputs here are least-significant-octet-first.
    #[test]
    fn sih_matches_the_core_spec_ah_sample() {
        let mut k = [
            0xec, 0x02, 0x34, 0xa3, 0x57, 0xc8, 0xad, 0x05, 0x34, 0x10, 0x10, 0xa6, 0x0a, 0x39, 0x7d, 0x9b,
        ];
        k.reverse();
        let hash = sih(&k, [0x94, 0x81, 0x70]);
        assert_eq!(hash, [0xaa, 0xfb, 0x0d]);
    }

    #[test]
    fn sef_then_sdf_round_trips_and_actually_obfuscates() {
        let ltk = [0x11; 16];
        let sirk = [
            0x45, 0x7d, 0x7d, 0x09, 0x21, 0xa1, 0xfd, 0x22, 0xce, 0xec, 0xd3, 0x2c, 0x66, 0x5e, 0xa6, 0xe5,
        ];
        let encrypted = sef(&ltk, &sirk);
        assert_ne!(encrypted, sirk);
        assert_eq!(sdf(&ltk, &encrypted), sirk);

        let value = Sirk::encrypted(sirk, ltk);
        assert_eq!(value.sirk_type, SirkType::Encrypted);
        assert_eq!(value.decrypt(ltk), sirk);
        assert_eq!(Sirk::plaintext(sirk).decrypt(ltk), sirk);
    }

    #[test]
    fn rsi_resolves_only_with_the_right_sirk() {
        let sirk = [0x5a; 16];
        let ad = rsi(&sirk, [0x12, 0x34, 0x56]);
        assert!(resolve_rsi(&sirk, &ad));
        assert!(!resolve_rsi(&[0x00; 16], &ad));
    }
}
