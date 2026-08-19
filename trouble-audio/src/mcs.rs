//! ## Media Control Service
//!
//! The (Generic) Media Control Service lets a client discover and control this device's single
//! media player: play/pause/stop/seek/skip track, and observe track metadata and playback state.
//! Bluetooth MCS v1.0.
//!
//! This implements the **Generic** MCS (service UUID `GENERIC_MEDIA_CONTROL`, singular per
//! device) rather than per-player MCS instances, matching this crate's single-active-connection,
//! single-player model (see the analogous note atop ascs.rs).
//!
//! The optional Object ID characteristics (Media Player Icon / Current Track Segments / Current
//! Track / Next Track / Parent Group / Current Group Object ID) are supported via
//! [`McsObjectIds`], referencing objects served by this crate's [`crate::ots`] - add both
//! services to expose a browsable hierarchy. Still out of scope: Search Control Point/Search
//! Results Object ID (OTS object *filtering*, which `crate::ots` doesn't implement either).

use bitflags::bitflags;
use bt_hci::uuid::{characteristic, service};
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::String as HString;
use trouble_host::{
    gatt::{GattConnection, ReadEvent, WriteEvent},
    prelude::*,
    types::gatt_traits::*,
};

use crate::{ots::ObjectId, LeAudioServerService, MAX_SERVICES};

/// Media Control Point Notification result codes (MCS, "Media Control Point Notification result
/// codes" table). Reconstructed from public documentation - verify against the official
/// Bluetooth SIG assigned-numbers PDF before relying on the exact values for certification.
pub const RESULT_SUCCESS: u8 = 0x01;
pub const RESULT_OPCODE_NOT_SUPPORTED: u8 = 0x02;
pub const RESULT_MEDIA_PLAYER_INACTIVE: u8 = 0x03;
pub const RESULT_COMMAND_CANNOT_BE_COMPLETED: u8 = 0x04;

/// Media State characteristic value (MCS, "Media State" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaState {
    #[default]
    Inactive = 0x00,
    Playing = 0x01,
    Paused = 0x02,
    Seeking = 0x03,
}

impl TryFrom<u8> for MediaState {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Inactive),
            0x01 => Ok(Self::Playing),
            0x02 => Ok(Self::Paused),
            0x03 => Ok(Self::Seeking),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for MediaState {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for MediaState {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for MediaState {
    const SIZE: usize = 1;
}

/// Playing Order characteristic value (MCS, "Playing Order" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PlayingOrder {
    SingleOnce = 0x01,
    SingleRepeat = 0x02,
    InOrderOnce = 0x03,
    InOrderRepeat = 0x04,
    OldestOnce = 0x05,
    OldestRepeat = 0x06,
    NewestOnce = 0x07,
    NewestRepeat = 0x08,
    ShuffleOnce = 0x09,
    ShuffleRepeat = 0x0A,
}

impl PlayingOrder {
    /// This variant's bit position in [`PlayingOrdersSupported`] (MCS: bit `n` corresponds to the
    /// Playing_Order value `n + 1`).
    fn supported_bit(self) -> PlayingOrdersSupported {
        PlayingOrdersSupported::from_bits_truncate(1 << (self as u8 - 1))
    }
}

impl TryFrom<u8> for PlayingOrder {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::SingleOnce),
            0x02 => Ok(Self::SingleRepeat),
            0x03 => Ok(Self::InOrderOnce),
            0x04 => Ok(Self::InOrderRepeat),
            0x05 => Ok(Self::OldestOnce),
            0x06 => Ok(Self::OldestRepeat),
            0x07 => Ok(Self::NewestOnce),
            0x08 => Ok(Self::NewestRepeat),
            0x09 => Ok(Self::ShuffleOnce),
            0x0A => Ok(Self::ShuffleRepeat),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

impl AsGatt for PlayingOrder {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 1;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(u8)]` fieldless enum - same layout as its discriminant.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 1) }
    }
}

impl FromGatt for PlayingOrder {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        if data.len() != 1 {
            return Err(FromGattError::InvalidLength);
        }
        Self::try_from(data[0])
    }
}

impl FixedGattValue for PlayingOrder {
    const SIZE: usize = 1;
}

bitflags! {
    /// Playing Orders Supported characteristic value (MCS): a bitmask of which
    /// [`PlayingOrder`] values this player accepts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PlayingOrdersSupported: u16 {
        const SingleOnce = 1 << 0;
        const SingleRepeat = 1 << 1;
        const InOrderOnce = 1 << 2;
        const InOrderRepeat = 1 << 3;
        const OldestOnce = 1 << 4;
        const OldestRepeat = 1 << 5;
        const NewestOnce = 1 << 6;
        const NewestRepeat = 1 << 7;
        const ShuffleOnce = 1 << 8;
        const ShuffleRepeat = 1 << 9;
    }
}

impl AsGatt for PlayingOrdersSupported {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 2;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u16 here).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 2) }
    }
}

impl FromGatt for PlayingOrdersSupported {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 2] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u16::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for PlayingOrdersSupported {
    const SIZE: usize = 2;
}

/// Media Control Point Opcodes (MCS, "Media Control Point procedure requirements" table).
/// Reconstructed from public documentation - verify the exact numeric values against the
/// official assigned-numbers PDF before relying on them for certification.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaControlPointOpcode {
    Play,
    Pause,
    FastRewind,
    FastForward,
    Stop,
    /// Signed offset in hundredths of a second, relative to the current Track Position.
    MoveRelative(i32),
    PreviousSegment,
    NextSegment,
    FirstSegment,
    LastSegment,
    GotoSegment(i32),
    PreviousTrack,
    NextTrack,
    FirstTrack,
    LastTrack,
    GotoTrack(i32),
    PreviousGroup,
    NextGroup,
    FirstGroup,
    LastGroup,
    GotoGroup(i32),
}

impl MediaControlPointOpcode {
    fn raw_opcode(&self) -> u8 {
        match self {
            Self::Play => 0x01,
            Self::Pause => 0x02,
            Self::FastRewind => 0x03,
            Self::FastForward => 0x04,
            Self::Stop => 0x05,
            Self::MoveRelative(_) => 0x10,
            Self::PreviousSegment => 0x20,
            Self::NextSegment => 0x21,
            Self::FirstSegment => 0x22,
            Self::LastSegment => 0x23,
            Self::GotoSegment(_) => 0x24,
            Self::PreviousTrack => 0x30,
            Self::NextTrack => 0x31,
            Self::FirstTrack => 0x32,
            Self::LastTrack => 0x33,
            Self::GotoTrack(_) => 0x34,
            Self::PreviousGroup => 0x40,
            Self::NextGroup => 0x41,
            Self::FirstGroup => 0x42,
            Self::LastGroup => 0x43,
            Self::GotoGroup(_) => 0x44,
        }
    }

    /// This opcode's bit position in [`MediaControlPointOpcodesSupported`] - a compact index
    /// (0..=20) rather than the sparse `raw_opcode` value, per how the Opcodes Supported bitmask
    /// is defined.
    fn supported_bit(&self) -> MediaControlPointOpcodesSupported {
        let index = match self {
            Self::Play => 0,
            Self::Pause => 1,
            Self::FastRewind => 2,
            Self::FastForward => 3,
            Self::Stop => 4,
            Self::MoveRelative(_) => 5,
            Self::PreviousSegment => 6,
            Self::NextSegment => 7,
            Self::FirstSegment => 8,
            Self::LastSegment => 9,
            Self::GotoSegment(_) => 10,
            Self::PreviousTrack => 11,
            Self::NextTrack => 12,
            Self::FirstTrack => 13,
            Self::LastTrack => 14,
            Self::GotoTrack(_) => 15,
            Self::PreviousGroup => 16,
            Self::NextGroup => 17,
            Self::FirstGroup => 18,
            Self::LastGroup => 19,
            Self::GotoGroup(_) => 20,
        };
        MediaControlPointOpcodesSupported::from_bits_truncate(1 << index)
    }

    fn param(&self) -> Option<i32> {
        match *self {
            Self::MoveRelative(v) | Self::GotoSegment(v) | Self::GotoTrack(v) | Self::GotoGroup(v) => Some(v),
            _ => None,
        }
    }

    fn from_raw(opcode: u8, param: Option<i32>) -> Result<Self, FromGattError> {
        Ok(match (opcode, param) {
            (0x01, None) => Self::Play,
            (0x02, None) => Self::Pause,
            (0x03, None) => Self::FastRewind,
            (0x04, None) => Self::FastForward,
            (0x05, None) => Self::Stop,
            (0x10, Some(v)) => Self::MoveRelative(v),
            (0x20, None) => Self::PreviousSegment,
            (0x21, None) => Self::NextSegment,
            (0x22, None) => Self::FirstSegment,
            (0x23, None) => Self::LastSegment,
            (0x24, Some(v)) => Self::GotoSegment(v),
            (0x30, None) => Self::PreviousTrack,
            (0x31, None) => Self::NextTrack,
            (0x32, None) => Self::FirstTrack,
            (0x33, None) => Self::LastTrack,
            (0x34, Some(v)) => Self::GotoTrack(v),
            (0x40, None) => Self::PreviousGroup,
            (0x41, None) => Self::NextGroup,
            (0x42, None) => Self::FirstGroup,
            (0x43, None) => Self::LastGroup,
            (0x44, Some(v)) => Self::GotoGroup(v),
            _ => return Err(FromGattError::InvalidLength),
        })
    }
}

bitflags! {
    /// Media Control Point Opcodes Supported characteristic value (MCS): a bitmask of which
    /// [`MediaControlPointOpcode`] variants this player accepts. See
    /// [`MediaControlPointOpcode::supported_bit`] for the bit layout.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MediaControlPointOpcodesSupported: u32 {
        const Play = 1 << 0;
        const Pause = 1 << 1;
        const FastRewind = 1 << 2;
        const FastForward = 1 << 3;
        const Stop = 1 << 4;
        const MoveRelative = 1 << 5;
        const PreviousSegment = 1 << 6;
        const NextSegment = 1 << 7;
        const FirstSegment = 1 << 8;
        const LastSegment = 1 << 9;
        const GotoSegment = 1 << 10;
        const PreviousTrack = 1 << 11;
        const NextTrack = 1 << 12;
        const FirstTrack = 1 << 13;
        const LastTrack = 1 << 14;
        const GotoTrack = 1 << 15;
        const PreviousGroup = 1 << 16;
        const NextGroup = 1 << 17;
        const FirstGroup = 1 << 18;
        const LastGroup = 1 << 19;
        const GotoGroup = 1 << 20;
    }
}

impl AsGatt for MediaControlPointOpcodesSupported {
    const MIN_SIZE: usize = 4;
    const MAX_SIZE: usize = 4;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: bitflags types are `#[repr(transparent)]` over their bit type (u32 here).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 4) }
    }
}

impl FromGatt for MediaControlPointOpcodesSupported {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let bytes: [u8; 4] = data.try_into().map_err(|_| FromGattError::InvalidLength)?;
        Ok(Self::from_bits_truncate(u32::from_le_bytes(bytes)))
    }
}

impl FixedGattValue for MediaControlPointOpcodesSupported {
    const SIZE: usize = 4;
}

/// The value written to the Media Control Point characteristic: Opcode (1) | optional signed
/// LE parameter (4 - only present for `MoveRelative`/`Goto*` opcodes). Wire bytes are cached in
/// `encoded` for the same reason as `vcs::VolumeControlPointOperation`.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaControlPointOperation {
    pub opcode: MediaControlPointOpcode,
    encoded: [u8; 5],
    encoded_len: u8,
}

impl MediaControlPointOperation {
    pub fn new(opcode: MediaControlPointOpcode) -> Self {
        let mut encoded = [0u8; 5];
        encoded[0] = opcode.raw_opcode();
        let mut encoded_len = 1;
        if let Some(param) = opcode.param() {
            encoded[1..5].copy_from_slice(&param.to_le_bytes());
            encoded_len = 5;
        }
        Self { opcode, encoded, encoded_len }
    }
}

impl AsGatt for MediaControlPointOperation {
    const MIN_SIZE: usize = 1;
    const MAX_SIZE: usize = 5;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded[..self.encoded_len as usize]
    }
}

impl FromGatt for MediaControlPointOperation {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        let raw_opcode = *data.first().ok_or(FromGattError::InvalidLength)?;
        let param = match data.len() {
            1 => None,
            5 => Some(i32::from_le_bytes(data[1..5].try_into().unwrap())),
            _ => return Err(FromGattError::InvalidLength),
        };
        let opcode = MediaControlPointOpcode::from_raw(raw_opcode, param)?;
        Ok(Self::new(opcode))
    }
}

impl Default for MediaControlPointOperation {
    /// An arbitrary valid operation - only used to seed the characteristic's backing store at
    /// construction time (a client always writes a fresh value before this is ever read).
    fn default() -> Self {
        Self::new(MediaControlPointOpcode::Pause)
    }
}

/// The Media Control Point notification sent back to the client after processing an operation:
/// Requested_Opcode (1) | Result_Code (1) (MCS, "Media Control Point notification" table).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct MediaControlPointNotification {
    pub requested_opcode: u8,
    pub result_code: u8,
}

impl AsGatt for MediaControlPointNotification {
    const MIN_SIZE: usize = 2;
    const MAX_SIZE: usize = 2;

    fn as_gatt(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]`, two adjacent `u8` fields with no padding.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 2) }
    }
}

/// This player's mutable playback state - the subset of MCS characteristics that
/// [`apply_media_operation`] can change. Track Title/Duration/Playing Order and the player's
/// track list itself are managed by the application (e.g. when it loads a new track, it's
/// expected to update Track Duration and notify Track Changed itself) - this only tracks what a
/// Media Control Point operation directly implies.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPlayerState {
    pub media_state: MediaState,
    /// Hundredths of a second. Negative means "unknown" (this crate's stand-in for MCS's
    /// `0xFFFFFFFF` sentinel - kept signed throughout for arithmetic simplicity, since
    /// `MoveRelative`'s offset must be signed anyway).
    pub track_position: i32,
    pub track_duration: i32,
    pub seeking_speed: i8,
}

impl Default for MediaPlayerState {
    fn default() -> Self {
        Self {
            media_state: MediaState::Inactive,
            track_position: 0,
            track_duration: -1,
            seeking_speed: 0,
        }
    }
}

/// This implementation's seeking speed multiplier while Fast Rewind/Fast Forward is active.
const SEEKING_SPEED_MULTIPLIER: i8 = 4;

/// Applies one Media Control Point operation to `current`, per MCS's Media Control Point
/// procedure requirements. Returns the (possibly unchanged) new state alongside the
/// Result_Code the Control Point notification should report - unlike `vcs`'s Volume Control
/// Point, an unsuccessful MCS operation is still an *accepted* ATT write (the failure is
/// conveyed only through the notification's Result_Code), matching how ASCS's ASE Control Point
/// behaves (see `bap::drive_ase_control_point`).
///
/// Segment/Group navigation opcodes always report [`RESULT_COMMAND_CANNOT_BE_COMPLETED`] - this
/// crate models no segments or groups (that requires OTS; see the module doc comment).
/// Track navigation opcodes (`PreviousTrack`/`NextTrack`/`GotoTrack`/...) succeed and reset
/// `track_position` to 0, but do not - cannot, without a real playlist - change Track
/// Title/Duration themselves; the application is expected to follow up once it has loaded
/// whichever track the client asked for.
pub fn apply_media_operation(
    current: MediaPlayerState,
    supported: MediaControlPointOpcodesSupported,
    operation: MediaControlPointOperation,
) -> (MediaPlayerState, u8) {
    if !supported.contains(operation.opcode.supported_bit()) {
        return (current, RESULT_OPCODE_NOT_SUPPORTED);
    }

    let inactive = current.media_state == MediaState::Inactive;
    use MediaControlPointOpcode::*;
    match operation.opcode {
        Play if inactive => (current, RESULT_MEDIA_PLAYER_INACTIVE),
        Play => (
            MediaPlayerState { media_state: MediaState::Playing, seeking_speed: 0, ..current },
            RESULT_SUCCESS,
        ),
        Pause if inactive => (current, RESULT_MEDIA_PLAYER_INACTIVE),
        Pause => (
            MediaPlayerState { media_state: MediaState::Paused, seeking_speed: 0, ..current },
            RESULT_SUCCESS,
        ),
        Stop if inactive => (current, RESULT_MEDIA_PLAYER_INACTIVE),
        Stop => (
            MediaPlayerState {
                media_state: MediaState::Paused,
                track_position: 0,
                seeking_speed: 0,
                ..current
            },
            RESULT_SUCCESS,
        ),
        FastRewind if inactive => (current, RESULT_MEDIA_PLAYER_INACTIVE),
        FastRewind => (
            MediaPlayerState {
                media_state: MediaState::Seeking,
                seeking_speed: -SEEKING_SPEED_MULTIPLIER,
                ..current
            },
            RESULT_SUCCESS,
        ),
        FastForward if inactive => (current, RESULT_MEDIA_PLAYER_INACTIVE),
        FastForward => (
            MediaPlayerState {
                media_state: MediaState::Seeking,
                seeking_speed: SEEKING_SPEED_MULTIPLIER,
                ..current
            },
            RESULT_SUCCESS,
        ),
        MoveRelative(_) if inactive => (current, RESULT_MEDIA_PLAYER_INACTIVE),
        MoveRelative(offset) => {
            let mut position = current.track_position.saturating_add(offset).max(0);
            if current.track_duration >= 0 {
                position = position.min(current.track_duration);
            }
            (MediaPlayerState { track_position: position, ..current }, RESULT_SUCCESS)
        }
        PreviousTrack | NextTrack | FirstTrack | LastTrack | GotoTrack(_) if inactive => {
            (current, RESULT_MEDIA_PLAYER_INACTIVE)
        }
        PreviousTrack | NextTrack | FirstTrack | LastTrack | GotoTrack(_) => {
            (MediaPlayerState { track_position: 0, ..current }, RESULT_SUCCESS)
        }
        PreviousSegment | NextSegment | FirstSegment | LastSegment | GotoSegment(_) => {
            (current, RESULT_COMMAND_CANNOT_BE_COMPLETED)
        }
        PreviousGroup | NextGroup | FirstGroup | LastGroup | GotoGroup(_) => {
            (current, RESULT_COMMAND_CANNOT_BE_COMPLETED)
        }
    }
}

/// A Gatt service client for reading/controlling a device's single media player.
pub struct McsClient {
    handle: ServiceHandle,
    pub media_player_name: Characteristic<HString<32>>,
    pub track_changed: Characteristic<()>,
    pub track_title: Characteristic<HString<64>>,
    pub track_duration: Characteristic<i32>,
    pub track_position: Characteristic<i32>,
    pub playback_speed: Characteristic<i8>,
    pub seeking_speed: Characteristic<i8>,
    pub playing_order: Characteristic<PlayingOrder>,
    pub playing_orders_supported: Characteristic<PlayingOrdersSupported>,
    pub media_state: Characteristic<MediaState>,
    pub media_control_point: Characteristic<MediaControlPointOperation>,
    pub media_control_point_opcodes_supported: Characteristic<MediaControlPointOpcodesSupported>,
    pub content_control_id: Characteristic<u8>,
}

impl McsClient {
    /// Discovers the (Generic) MCS service and its characteristics on an already-connected
    /// `GattClient`. Returns `None` if the peer doesn't expose MCS (it's an optional service) -
    /// see [`crate::LeAudioClient`].
    pub async fn discover<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
        client: &mut GattClient<'_, T, P, MAX_SERVICES>,
    ) -> Option<Self> {
        let services = client
            .services_by_uuid(&Uuid::from(service::GENERIC_MEDIA_CONTROL))
            .await
            .ok()?;
        let handle = services.first()?;

        macro_rules! required {
            ($uuid:expr) => {
                client.characteristic_by_uuid(handle, &Uuid::from($uuid)).await.ok()?
            };
        }

        Some(Self {
            handle: handle.clone(),
            media_player_name: required!(characteristic::MEDIA_PLAYER_NAME),
            track_changed: required!(characteristic::TRACK_CHANGED),
            track_title: required!(characteristic::TRACK_TITLE),
            track_duration: required!(characteristic::TRACK_DURATION),
            track_position: required!(characteristic::TRACK_POSITION),
            playback_speed: required!(characteristic::PLAYBACK_SPEED),
            seeking_speed: required!(characteristic::SEEKING_SPEED),
            playing_order: required!(characteristic::PLAYING_ORDER),
            playing_orders_supported: required!(characteristic::PLAYING_ORDERS_SUPPORTED),
            media_state: required!(characteristic::MEDIA_STATE),
            media_control_point: required!(characteristic::MEDIA_CONTROL_POINT),
            media_control_point_opcodes_supported: required!(characteristic::MEDIA_CONTROL_POINT_OPCODES_SUPPORTED),
            content_control_id: required!(characteristic::CONTENT_CONTROL_ID),
        })
    }
}

/// Initial values for [`McsServer::new`].
/// An OTS Object ID as an MCS characteristic value: 48 bits, 6 octets LE (OTS 1.0 "Object ID").
/// `ObjectId(0)` conventionally means "no object selected" here - the spec's alternative
/// (a zero-length value) doesn't fit a fixed-size characteristic.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ObjectIdValue {
    encoded: [u8; 6],
}

impl From<ObjectId> for ObjectIdValue {
    fn from(id: ObjectId) -> Self {
        let mut encoded = [0u8; 6];
        encoded.copy_from_slice(&id.0.to_le_bytes()[..6]);
        Self { encoded }
    }
}

impl ObjectIdValue {
    pub fn object_id(&self) -> ObjectId {
        let mut widened = [0u8; 8];
        widened[..6].copy_from_slice(&self.encoded);
        ObjectId(u64::from_le_bytes(widened))
    }
}

impl AsGatt for ObjectIdValue {
    const MIN_SIZE: usize = 6;
    const MAX_SIZE: usize = 6;

    fn as_gatt(&self) -> &[u8] {
        &self.encoded
    }
}

impl FromGatt for ObjectIdValue {
    fn from_gatt(data: &[u8]) -> Result<Self, FromGattError> {
        Ok(Self {
            encoded: data.try_into().map_err(|_| FromGattError::InvalidLength)?,
        })
    }
}

impl FixedGattValue for ObjectIdValue {
    const SIZE: usize = 6;
}

/// Initial values for MCS's optional Object ID characteristics, each referencing an object this
/// device also serves through [`crate::ots::OtsServer`]. Passing `Some` of this to
/// [`McsServer::new`] exposes all six characteristics.
#[derive(Debug, Default, Clone, Copy)]
pub struct McsObjectIds {
    pub media_player_icon: ObjectId,
    pub current_track_segments: ObjectId,
    pub current_track: ObjectId,
    pub next_track: ObjectId,
    pub parent_group: ObjectId,
    pub current_group: ObjectId,
}

/// The built Object ID characteristics, present when [`McsServer::new`] was given
/// [`McsObjectIds`].
struct McsObjectIdChars {
    media_player_icon: Characteristic<ObjectIdValue>,
    current_track_segments: Characteristic<ObjectIdValue>,
    current_track: Characteristic<ObjectIdValue>,
    next_track: Characteristic<ObjectIdValue>,
    parent_group: Characteristic<ObjectIdValue>,
    current_group: Characteristic<ObjectIdValue>,
}

pub struct McsInit {
    pub media_player_name: HString<32>,
    pub track_title: HString<64>,
    pub track_duration: i32,
    pub playback_speed: i8,
    pub playing_order: PlayingOrder,
    pub playing_orders_supported: PlayingOrdersSupported,
    pub media_control_point_opcodes_supported: MediaControlPointOpcodesSupported,
    pub content_control_id: u8,
}

/// Backing storage buffers for [`McsServer::new`], sized to each characteristic's `MAX_SIZE`.
pub struct McsStore<'a> {
    pub media_player_name: &'a mut [u8],
    pub track_changed: &'a mut [u8],
    pub track_title: &'a mut [u8],
    pub track_duration: &'a mut [u8],
    pub track_position: &'a mut [u8],
    pub playback_speed: &'a mut [u8],
    pub seeking_speed: &'a mut [u8],
    pub playing_order: &'a mut [u8],
    pub media_state: &'a mut [u8],
    pub media_control_point: &'a mut [u8],
    pub content_control_id: &'a mut [u8],
    /// One 6-byte store per Object ID characteristic, in [`McsObjectIds`] field order. Unused
    /// (may be empty) when no [`McsObjectIds`] is given.
    pub object_ids: [&'a mut [u8]; 6],
}

/// Owned backing storage for MCS's characteristics.
pub struct McsStorage {
    pub media_player_name: [u8; 32],
    pub object_ids: [[u8; 6]; 6],
    pub track_title: [u8; 64],
    pub track_duration: [u8; 4],
    pub track_position: [u8; 4],
    pub playback_speed: [u8; 1],
    pub seeking_speed: [u8; 1],
    pub playing_order: [u8; 1],
    pub media_state: [u8; 1],
    pub media_control_point: [u8; 5],
    pub content_control_id: [u8; 1],
}

impl Default for McsStorage {
    fn default() -> Self {
        Self {
            media_player_name: [0; 32],
            object_ids: [[0; 6]; 6],
            track_title: [0; 64],
            track_duration: [0; 4],
            track_position: [0; 4],
            playback_speed: [0; 1],
            seeking_speed: [0; 1],
            playing_order: [0; 1],
            media_state: [0; 1],
            media_control_point: [0; 5],
            content_control_id: [0; 1],
        }
    }
}

impl McsStorage {
    pub fn as_store(&mut self) -> McsStore<'_> {
        McsStore {
            media_player_name: &mut self.media_player_name,
            // Track Changed carries no value - notification-only.
            track_changed: &mut [],
            track_title: &mut self.track_title,
            track_duration: &mut self.track_duration,
            track_position: &mut self.track_position,
            playback_speed: &mut self.playback_speed,
            seeking_speed: &mut self.seeking_speed,
            playing_order: &mut self.playing_order,
            media_state: &mut self.media_state,
            media_control_point: &mut self.media_control_point,
            content_control_id: &mut self.content_control_id,
            object_ids: {
                // `[&mut ...; 6]` can't be built by mapping in stable const fashion - split the
                // array of arrays into per-element borrows instead.
                let [a, b, c, d, e, f] = &mut self.object_ids;
                [&mut a[..], &mut b[..], &mut c[..], &mut d[..], &mut e[..], &mut f[..]]
            },
        }
    }
}

/// A Gatt service server exposing this device's single media player.
pub struct McsServer {
    handle: u16,
    media_player_name: Characteristic<HString<32>>,
    track_changed: Characteristic<()>,
    track_title: Characteristic<HString<64>>,
    track_duration: Characteristic<i32>,
    track_position: Characteristic<i32>,
    playback_speed: Characteristic<i8>,
    seeking_speed: Characteristic<i8>,
    playing_order: Characteristic<PlayingOrder>,
    playing_orders_supported: PlayingOrdersSupported,
    playing_orders_supported_char: Characteristic<PlayingOrdersSupported>,
    media_state: Characteristic<MediaState>,
    media_control_point: Characteristic<MediaControlPointOperation>,
    opcodes_supported: MediaControlPointOpcodesSupported,
    media_control_point_opcodes_supported_char: Characteristic<MediaControlPointOpcodesSupported>,
    content_control_id: Characteristic<u8>,
    object_ids: Option<McsObjectIdChars>,
}

pub const MCS_ATTRIBUTES: usize = 41 + 6 * 3; // + the optional Object ID characteristics

impl McsServer {
    /// Creates a new (Generic) MCS Gatt service. `Playing Orders Supported`/`Media Control Point
    /// Opcodes Supported` are read-only constants fixed at construction (`init`), so they use
    /// `add_characteristic_ro` and need no backing store of their own.
    pub fn new<'a, M: RawMutex>(
        table: &mut trouble_host::attribute::AttributeTable<'a, M, MAX_SERVICES>,
        init: McsInit,
        playing_orders_supported: &'a PlayingOrdersSupported,
        opcodes_supported: &'a MediaControlPointOpcodesSupported,
        object_ids: Option<McsObjectIds>,
        store: McsStore<'a>,
    ) -> Self {
        let mut service = table.add_service(Service::new(service::GENERIC_MEDIA_CONTROL));

        let media_player_name = service
            .add_characteristic(
                characteristic::MEDIA_PLAYER_NAME,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                init.media_player_name,
                store.media_player_name,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        // No value of its own (just a signal) and so no read/write permission to set - matches
        // OTS's Object Changed characteristic (see `ots::OtsServer::new`).
        let track_changed = service
            .add_characteristic(characteristic::TRACK_CHANGED, &[CharacteristicProp::Notify], (), store.track_changed)
            .build();

        let track_title = service
            .add_characteristic(
                characteristic::TRACK_TITLE,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                init.track_title,
                store.track_title,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let track_duration = service
            .add_characteristic(
                characteristic::TRACK_DURATION,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                init.track_duration,
                store.track_duration,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let track_position = service
            .add_characteristic(
                characteristic::TRACK_POSITION,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                0i32,
                store.track_position,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let playback_speed = service
            .add_characteristic(
                characteristic::PLAYBACK_SPEED,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                init.playback_speed,
                store.playback_speed,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let seeking_speed = service
            .add_characteristic(
                characteristic::SEEKING_SPEED,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                0i8,
                store.seeking_speed,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let playing_order = service
            .add_characteristic(
                characteristic::PLAYING_ORDER,
                &[CharacteristicProp::Read, CharacteristicProp::Write, CharacteristicProp::Notify],
                init.playing_order,
                store.playing_order,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let playing_orders_supported_char = service
            .add_characteristic_ro(characteristic::PLAYING_ORDERS_SUPPORTED, playing_orders_supported)
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let media_state = service
            .add_characteristic(
                characteristic::MEDIA_STATE,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                MediaState::Inactive,
                store.media_state,
            )
            .read_permission(PermissionLevel::EncryptionRequired)
            .build();

        let media_control_point = service
            .add_characteristic(
                characteristic::MEDIA_CONTROL_POINT,
                &[CharacteristicProp::Write, CharacteristicProp::Notify],
                MediaControlPointOperation::default(),
                store.media_control_point,
            )
            .write_permission(PermissionLevel::EncryptionRequired)
            .build();

        let media_control_point_opcodes_supported_char = service
            .add_characteristic_ro(characteristic::MEDIA_CONTROL_POINT_OPCODES_SUPPORTED, opcodes_supported)
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

        let object_id_chars = object_ids.map(|ids| {
            let [icon_store, segments_store, track_store, next_store, parent_store, group_store] = store.object_ids;
            let mut object_id_char = |uuid, id: ObjectId, char_store: &'a mut [u8]| {
                service
                    .add_characteristic(
                        uuid,
                        &[CharacteristicProp::Read, CharacteristicProp::Notify],
                        ObjectIdValue::from(id),
                        char_store,
                    )
                    .read_permission(PermissionLevel::EncryptionRequired)
                    .build()
            };
            McsObjectIdChars {
                media_player_icon: object_id_char(
                    characteristic::MEDIA_PLAYER_ICON_OBJECT_ID,
                    ids.media_player_icon,
                    icon_store,
                ),
                current_track_segments: object_id_char(
                    characteristic::CURRENT_TRACK_SEGMENTS_OBJECT_ID,
                    ids.current_track_segments,
                    segments_store,
                ),
                current_track: object_id_char(characteristic::CURRENT_TRACK_OBJECT_ID, ids.current_track, track_store),
                next_track: object_id_char(characteristic::NEXT_TRACK_OBJECT_ID, ids.next_track, next_store),
                parent_group: object_id_char(characteristic::PARENT_GROUP_OBJECT_ID, ids.parent_group, parent_store),
                current_group: object_id_char(characteristic::CURRENT_GROUP_OBJECT_ID, ids.current_group, group_store),
            }
        });

        Self {
            handle: service.build(),
            media_player_name,
            track_changed,
            track_title,
            track_duration,
            track_position,
            playback_speed,
            seeking_speed,
            playing_order,
            playing_orders_supported: *playing_orders_supported,
            playing_orders_supported_char,
            media_state,
            media_control_point,
            opcodes_supported: *opcodes_supported,
            media_control_point_opcodes_supported_char,
            content_control_id,
            object_ids: object_id_chars,
        }
    }

    pub fn media_player_name(&self) -> &Characteristic<HString<32>> {
        &self.media_player_name
    }
    /// Notify-only, no value of its own - notify it (`.notify(conn, &(), ...)`) whenever the
    /// current track changes, e.g. after loading a new Track Title/Duration, or after a
    /// successful track-navigation Media Control Point operation (see
    /// [`drive_media_control_point`], which already does the latter automatically).
    pub fn track_changed(&self) -> &Characteristic<()> {
        &self.track_changed
    }
    pub fn track_title(&self) -> &Characteristic<HString<64>> {
        &self.track_title
    }
    pub fn track_duration(&self) -> &Characteristic<i32> {
        &self.track_duration
    }
    pub fn track_position(&self) -> &Characteristic<i32> {
        &self.track_position
    }
    pub fn playback_speed(&self) -> &Characteristic<i8> {
        &self.playback_speed
    }
    pub fn seeking_speed(&self) -> &Characteristic<i8> {
        &self.seeking_speed
    }
    pub fn playing_order(&self) -> &Characteristic<PlayingOrder> {
        &self.playing_order
    }
    pub fn playing_orders_supported(&self) -> PlayingOrdersSupported {
        self.playing_orders_supported
    }
    pub fn media_state(&self) -> &Characteristic<MediaState> {
        &self.media_state
    }
    pub fn media_control_point(&self) -> &Characteristic<MediaControlPointOperation> {
        &self.media_control_point
    }
    pub fn opcodes_supported(&self) -> MediaControlPointOpcodesSupported {
        self.opcodes_supported
    }
    pub fn content_control_id(&self) -> &Characteristic<u8> {
        &self.content_control_id
    }
    /// The Current Track Object ID characteristic, when built with [`McsObjectIds`]. Update it
    /// (`.set(...)` + `.notify(...)`) alongside [`Self::track_changed`] when the player moves to
    /// a track backed by a different OTS object.
    pub fn current_track_object_id(&self) -> Option<&Characteristic<ObjectIdValue>> {
        self.object_ids.as_ref().map(|o| &o.current_track)
    }
    pub fn next_track_object_id(&self) -> Option<&Characteristic<ObjectIdValue>> {
        self.object_ids.as_ref().map(|o| &o.next_track)
    }
    pub fn media_player_icon_object_id(&self) -> Option<&Characteristic<ObjectIdValue>> {
        self.object_ids.as_ref().map(|o| &o.media_player_icon)
    }
    pub fn current_track_segments_object_id(&self) -> Option<&Characteristic<ObjectIdValue>> {
        self.object_ids.as_ref().map(|o| &o.current_track_segments)
    }
    pub fn parent_group_object_id(&self) -> Option<&Characteristic<ObjectIdValue>> {
        self.object_ids.as_ref().map(|o| &o.parent_group)
    }
    pub fn current_group_object_id(&self) -> Option<&Characteristic<ObjectIdValue>> {
        self.object_ids.as_ref().map(|o| &o.current_group)
    }
}

impl<P: PacketPool> LeAudioServerService<P> for McsServer {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        let handles = [
            self.media_player_name.handle,
            self.track_title.handle,
            self.track_duration.handle,
            self.track_position.handle,
            self.playback_speed.handle,
            self.seeking_speed.handle,
            self.playing_order.handle,
            self.playing_orders_supported_char.handle,
            self.media_state.handle,
            self.media_control_point_opcodes_supported_char.handle,
            self.content_control_id.handle,
        ];
        if handles.contains(&event.handle()) {
            return Some(Ok(()));
        }
        if let Some(objects) = &self.object_ids {
            let object_handles = [
                objects.media_player_icon.handle,
                objects.current_track_segments.handle,
                objects.current_track.handle,
                objects.next_track.handle,
                objects.parent_group.handle,
                objects.current_group.handle,
            ];
            if object_handles.contains(&event.handle()) {
                return Some(Ok(()));
            }
        }
        None
    }

    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if event.handle() == self.track_position.handle {
            return Some(match event.value(&self.track_position) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        if event.handle() == self.playback_speed.handle {
            return Some(match event.value(&self.playback_speed) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        if event.handle() == self.playing_order.handle {
            return Some(match event.value(&self.playing_order) {
                Ok(order) if self.playing_orders_supported.contains(order.supported_bit()) => Ok(()),
                Ok(_) => Err(AttErrorCode::VALUE_NOT_ALLOWED),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        // The Media Control Point's business rule needs the current `MediaPlayerState`, so
        // `Server::handle` special-cases it (see [`drive_media_control_point`]) the same way it
        // special-cases the ASE Control Point for ASCS; it never reaches this generic path. Kept
        // here (shape-only) for completeness, matching `AscsServer::handle_write_event`.
        if event.handle() == self.media_control_point.handle {
            return Some(match event.value(&self.media_control_point) {
                Ok(_) => Ok(()),
                Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
            });
        }
        None
    }
}

/// Whether a Media Control Point operation, given the [`apply_media_operation`] result code it
/// produced, moved the player onto a different current track - i.e. a track-navigation opcode
/// that succeeded. This crate has no real playlist to pull the new track's Title/Duration from
/// (see the module doc comment), but the fact that the current track *changed* is still known,
/// and worth telling a client so it knows to re-read Track Title/Duration itself.
fn changes_track(opcode: MediaControlPointOpcode, result_code: u8) -> bool {
    use MediaControlPointOpcode::*;
    result_code == RESULT_SUCCESS
        && matches!(opcode, PreviousTrack | NextTrack | FirstTrack | LastTrack | GotoTrack(_))
}

/// Applies a Media Control Point write end to end: reads the current [`MediaPlayerState`] back
/// out of the individual characteristics it's spread across, applies `operation` (see
/// [`apply_media_operation`]), stores + notifies whichever characteristics changed (including
/// [`McsServer::track_changed`] on a successful track-navigation opcode - see [`changes_track`]),
/// and sends the resulting [`MediaControlPointNotification`] on the Media Control Point
/// characteristic itself (mirroring `bap::drive_ase_control_point`'s accept-then-notify handling
/// of the ASE Control Point). Unlike VCS's Control Point, this never rejects the ATT write itself
/// - MCS conveys failure only through the notification's Result_Code.
pub async fn drive_media_control_point<M: RawMutex, P: PacketPool, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    mcs: &McsServer,
    conn: &GattConnection<'_, '_, P>,
    operation: &MediaControlPointOperation,
) {
    let current = MediaPlayerState {
        media_state: mcs.media_state.get(server).unwrap_or_default(),
        track_position: mcs.track_position.get(server).unwrap_or(0),
        track_duration: mcs.track_duration.get(server).unwrap_or(-1),
        seeking_speed: mcs.seeking_speed.get(server).unwrap_or(0),
    };
    let (next, result_code) = apply_media_operation(current, mcs.opcodes_supported, *operation);

    if next.media_state != current.media_state {
        let _ = mcs.media_state.notify(conn, &next.media_state, true).await;
    }
    if next.track_position != current.track_position {
        let _ = mcs.track_position.notify(conn, &next.track_position, true).await;
    }
    if next.seeking_speed != current.seeking_speed {
        let _ = mcs.seeking_speed.notify(conn, &next.seeking_speed, true).await;
    }
    if changes_track(operation.opcode, result_code) {
        let _ = mcs.track_changed.notify(conn, &(), true).await;
    }

    let notification = MediaControlPointNotification {
        requested_opcode: operation.opcode.raw_opcode(),
        result_code,
    };
    let _ = mcs.media_control_point.notify_raw(conn, notification.as_gatt(), false).await;
}

#[cfg(test)]
mod tests {
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::MAX_SERVICES;

    fn playing(track_position: i32) -> MediaPlayerState {
        MediaPlayerState {
            media_state: MediaState::Playing,
            track_position,
            track_duration: 30_000,
            seeking_speed: 0,
        }
    }

    const ALL_SUPPORTED: MediaControlPointOpcodesSupported = MediaControlPointOpcodesSupported::all();

    #[test]
    fn media_state_round_trips_every_defined_value() {
        for state in [MediaState::Inactive, MediaState::Playing, MediaState::Paused, MediaState::Seeking] {
            assert_eq!(MediaState::from_gatt(state.as_gatt()).unwrap(), state);
        }
    }

    #[test]
    fn playing_order_supported_bit_matches_bit_n_minus_1() {
        assert_eq!(PlayingOrder::SingleOnce.supported_bit(), PlayingOrdersSupported::SingleOnce);
        assert_eq!(PlayingOrder::ShuffleRepeat.supported_bit(), PlayingOrdersSupported::ShuffleRepeat);
    }

    #[test]
    fn control_point_operation_round_trips_every_opcode_shape() {
        for opcode in [
            MediaControlPointOpcode::Play,
            MediaControlPointOpcode::Pause,
            MediaControlPointOpcode::FastRewind,
            MediaControlPointOpcode::FastForward,
            MediaControlPointOpcode::Stop,
            MediaControlPointOpcode::MoveRelative(-500),
            MediaControlPointOpcode::PreviousSegment,
            MediaControlPointOpcode::NextSegment,
            MediaControlPointOpcode::FirstSegment,
            MediaControlPointOpcode::LastSegment,
            MediaControlPointOpcode::GotoSegment(3),
            MediaControlPointOpcode::PreviousTrack,
            MediaControlPointOpcode::NextTrack,
            MediaControlPointOpcode::FirstTrack,
            MediaControlPointOpcode::LastTrack,
            MediaControlPointOpcode::GotoTrack(7),
            MediaControlPointOpcode::PreviousGroup,
            MediaControlPointOpcode::NextGroup,
            MediaControlPointOpcode::FirstGroup,
            MediaControlPointOpcode::LastGroup,
            MediaControlPointOpcode::GotoGroup(1),
        ] {
            let op = MediaControlPointOperation::new(opcode);
            assert_eq!(MediaControlPointOperation::from_gatt(op.as_gatt()).unwrap(), op);
        }
    }

    #[test]
    fn control_point_rejects_goto_without_its_parameter() {
        assert!(MediaControlPointOperation::from_gatt(&[0x34]).is_err());
    }

    #[test]
    fn control_point_rejects_a_parameter_on_a_parameterless_opcode() {
        assert!(MediaControlPointOperation::from_gatt(&[0x01, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn control_point_rejects_unknown_opcode() {
        assert!(MediaControlPointOperation::from_gatt(&[0xEE]).is_err());
    }

    #[test]
    fn play_from_paused_transitions_to_playing() {
        let current = MediaPlayerState { media_state: MediaState::Paused, ..playing(1000) };
        let (next, result) = apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::Play));
        assert_eq!(result, RESULT_SUCCESS);
        assert_eq!(next.media_state, MediaState::Playing);
    }

    #[test]
    fn play_while_inactive_reports_media_player_inactive_and_does_not_change_state() {
        let current = MediaPlayerState::default();
        let (next, result) = apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::Play));
        assert_eq!(result, RESULT_MEDIA_PLAYER_INACTIVE);
        assert_eq!(next, current);
    }

    #[test]
    fn unsupported_opcode_is_rejected_before_state_is_examined() {
        let current = MediaPlayerState::default(); // Inactive - would also fail the state check
        let supported = MediaControlPointOpcodesSupported::empty();
        let (next, result) = apply_media_operation(current, supported, MediaControlPointOperation::new(MediaControlPointOpcode::Play));
        assert_eq!(result, RESULT_OPCODE_NOT_SUPPORTED);
        assert_eq!(next, current);
    }

    #[test]
    fn stop_pauses_and_resets_track_position_to_zero() {
        let current = playing(15_000);
        let (next, result) = apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::Stop));
        assert_eq!(result, RESULT_SUCCESS);
        assert_eq!(next.media_state, MediaState::Paused);
        assert_eq!(next.track_position, 0);
    }

    #[test]
    fn fast_forward_and_fast_rewind_enter_seeking_with_opposite_sign_speeds() {
        let current = playing(1000);
        let (ff, _) = apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::FastForward));
        assert_eq!(ff.media_state, MediaState::Seeking);
        assert!(ff.seeking_speed > 0);

        let (rw, _) = apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::FastRewind));
        assert_eq!(rw.media_state, MediaState::Seeking);
        assert!(rw.seeking_speed < 0);
    }

    #[test]
    fn move_relative_clamps_to_zero_and_to_track_duration() {
        let current = playing(1000);
        let (before_start, _) =
            apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::MoveRelative(-5000)));
        assert_eq!(before_start.track_position, 0);

        let (past_end, _) = apply_media_operation(
            current,
            ALL_SUPPORTED,
            MediaControlPointOperation::new(MediaControlPointOpcode::MoveRelative(1_000_000)),
        );
        assert_eq!(past_end.track_position, current.track_duration);
    }

    #[test]
    fn next_track_resets_position_but_leaves_media_state_untouched() {
        let current = playing(20_000);
        let (next, result) =
            apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(MediaControlPointOpcode::NextTrack));
        assert_eq!(result, RESULT_SUCCESS);
        assert_eq!(next.track_position, 0);
        assert_eq!(next.media_state, MediaState::Playing);
    }

    #[test]
    fn segment_and_group_navigation_always_report_command_cannot_be_completed() {
        let current = playing(1000);
        for opcode in [
            MediaControlPointOpcode::NextSegment,
            MediaControlPointOpcode::GotoSegment(1),
            MediaControlPointOpcode::NextGroup,
            MediaControlPointOpcode::GotoGroup(1),
        ] {
            let (next, result) = apply_media_operation(current, ALL_SUPPORTED, MediaControlPointOperation::new(opcode));
            assert_eq!(result, RESULT_COMMAND_CANNOT_BE_COMPLETED);
            assert_eq!(next, current);
        }
    }

    #[test]
    fn mcs_server_exposes_characteristics_through_a_real_attribute_table() {
        let mut table: trouble_host::attribute::AttributeTable<'_, NoopRawMutex, MAX_SERVICES> =
            trouble_host::attribute::AttributeTable::new();

        static NAME: static_cell::StaticCell<[u8; 32]> = static_cell::StaticCell::new();
        static CHANGED: static_cell::StaticCell<[u8; 0]> = static_cell::StaticCell::new();
        static TITLE: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
        static DURATION: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static POSITION: static_cell::StaticCell<[u8; 4]> = static_cell::StaticCell::new();
        static SPEED: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static SEEK: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static ORDER: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static STATE: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static CP: static_cell::StaticCell<[u8; 5]> = static_cell::StaticCell::new();
        static CCID: static_cell::StaticCell<[u8; 1]> = static_cell::StaticCell::new();
        static ORDERS_SUP: static_cell::StaticCell<PlayingOrdersSupported> = static_cell::StaticCell::new();
        static OPS_SUP: static_cell::StaticCell<MediaControlPointOpcodesSupported> = static_cell::StaticCell::new();
        static OBJ_STORES: static_cell::StaticCell<[[u8; 6]; 6]> = static_cell::StaticCell::new();

        let mcs = McsServer::new(
            &mut table,
            McsInit {
                media_player_name: HString::try_from("Trouble Player").unwrap(),
                track_title: HString::try_from("Track One").unwrap(),
                track_duration: 180_000,
                playback_speed: 0,
                playing_order: PlayingOrder::InOrderOnce,
                playing_orders_supported: PlayingOrdersSupported::InOrderOnce | PlayingOrdersSupported::ShuffleOnce,
                media_control_point_opcodes_supported: ALL_SUPPORTED,
                content_control_id: 1,
            },
            ORDERS_SUP.init(PlayingOrdersSupported::InOrderOnce | PlayingOrdersSupported::ShuffleOnce),
            OPS_SUP.init(ALL_SUPPORTED),
            Some(McsObjectIds {
                current_track: ObjectId(0x0100),
                ..Default::default()
            }),
            McsStore {
                media_player_name: NAME.init([0; 32]),
                track_changed: CHANGED.init([0; 0]),
                track_title: TITLE.init([0; 64]),
                track_duration: DURATION.init([0; 4]),
                track_position: POSITION.init([0; 4]),
                playback_speed: SPEED.init([0; 1]),
                seeking_speed: SEEK.init([0; 1]),
                playing_order: ORDER.init([0; 1]),
                media_state: STATE.init([0; 1]),
                media_control_point: CP.init([0; 5]),
                content_control_id: CCID.init([0; 1]),
                object_ids: {
                    let [a, b, c, d, e, f] = OBJ_STORES.init([[0; 6]; 6]);
                    [&mut a[..], &mut b[..], &mut c[..], &mut d[..], &mut e[..], &mut f[..]]
                },
            },
        );
        let server: AttributeServer<'_, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1> = AttributeServer::new(table);

        assert_eq!(mcs.media_player_name().get(&server).unwrap(), HString::<32>::try_from("Trouble Player").unwrap());
        assert_eq!(mcs.track_changed().get(&server).unwrap(), ());
        assert_eq!(mcs.track_duration().get(&server).unwrap(), 180_000);
        assert_eq!(mcs.media_state().get(&server).unwrap(), MediaState::Inactive);
        assert_eq!(mcs.content_control_id().get(&server).unwrap(), 1);
        assert_eq!(mcs.playing_orders_supported(), PlayingOrdersSupported::InOrderOnce | PlayingOrdersSupported::ShuffleOnce);

        mcs.media_state().set(&server, &MediaState::Playing).unwrap();
        assert_eq!(mcs.media_state().get(&server).unwrap(), MediaState::Playing);

        // The OTS-backed Object ID characteristics round-trip through the real attribute table.
        let current_track = mcs.current_track_object_id().expect("built with McsObjectIds");
        assert_eq!(current_track.get(&server).unwrap().object_id(), ObjectId(0x0100));
        current_track.set(&server, &ObjectIdValue::from(ObjectId(0x0200))).unwrap();
        assert_eq!(current_track.get(&server).unwrap().object_id(), ObjectId(0x0200));
        assert_eq!(mcs.next_track_object_id().unwrap().get(&server).unwrap().object_id(), ObjectId(0));
    }

    #[test]
    fn changes_track_only_on_successful_track_navigation() {
        for opcode in [
            MediaControlPointOpcode::PreviousTrack,
            MediaControlPointOpcode::NextTrack,
            MediaControlPointOpcode::FirstTrack,
            MediaControlPointOpcode::LastTrack,
            MediaControlPointOpcode::GotoTrack(2),
        ] {
            assert!(changes_track(opcode, RESULT_SUCCESS));
            assert!(!changes_track(opcode, RESULT_MEDIA_PLAYER_INACTIVE));
        }
    }

    #[test]
    fn changes_track_is_false_for_non_navigation_opcodes() {
        for opcode in [
            MediaControlPointOpcode::Play,
            MediaControlPointOpcode::Pause,
            MediaControlPointOpcode::Stop,
            MediaControlPointOpcode::MoveRelative(100),
            MediaControlPointOpcode::NextSegment,
            MediaControlPointOpcode::NextGroup,
        ] {
            assert!(!changes_track(opcode, RESULT_SUCCESS));
        }
    }
}
