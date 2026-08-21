//! Broadcast (Auracast) audio **source**: extended advertising carrying a Broadcast Audio
//! Announcement, a periodic advertising train carrying the BASE (Basic Audio Announcement), and a
//! BIG whose BIS carry the audio - BAP 1.0.2 §3.7 / Core 6 Vol 4 Part E §7.8.103.
//!
//! Broadcast needs no connection, so this module drives raw HCI itself (via [`Stack::command`])
//! rather than going through trouble-host's connection-oriented advertising API - the one thing
//! that must come back through the event path is `LE Create BIG Complete` (it carries the BIS
//! connection handles), so [`BigSource`] implements [`EventHandler`] for
//! [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler), with
//! [`drive_big`] carrying out the awaited data-path commands, exactly like `cig.rs`/`cis.rs`.
//!
//! This module is codec-agnostic by design: the application hands each BIS ready-made SDUs (e.g.
//! LC3 frames from [`crate::lc3::Lc3MonoEncoder`], or already-encoded LC3 passed straight
//! through) via [`crate::iso_tx::build_packet`] addressed with [`BigSource::bis_handle`].
//!
//! The matching broadcast **sink** role lives in [`crate::big_sink`].

use core::cell::RefCell;

use bt_hci::cmd::le::{
    LeCreateBig, LeRemoveAdvSet, LeRemoveIsoDataPath, LeSetAdvSetRandomAddr, LeSetExtAdvData,
    LeSetExtAdvEnable, LeSetExtAdvParams, LeSetPeriodicAdvData, LeSetPeriodicAdvEnable,
    LeSetPeriodicAdvParams, LeSetupIsoDataPath, LeTerminateBig,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use bt_hci::event::le::LeCreateBigComplete;
use bt_hci::param::{
    AddrKind, AdvChannelMap, AdvEventProps, AdvFilterPolicy, AdvHandle, AdvSet, BdAddr, ConnHandle,
    DataPathDirection, DataPathId, Duration, EncryptionMode, ExtDuration, Framing, Operation,
    Packing, PeriodicAdvProps, PhyKind, PhyMask,
};
use bt_hci::uuid::service;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use heapless::Vec as HVec;
use trouble_host::prelude::*;

use crate::generic_audio::{
    AudioLocation, CodecSpecificConfiguration, FrameDuration, Metadata, SamplingFrequency,
    encode_list,
};
use crate::{CodecId, CodingFormat};

/// Matches this crate's stereo scope: one BIS per channel, at most two.
pub const MAX_BIS: usize = 2;

/// Why an Auracast announcement or BASE could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastParseError {
    /// An AD structure or BASE field ended before its declared length.
    Truncated,
    /// An AD structure used the reserved zero length before the end of the payload.
    InvalidAdLength,
    /// A BASE advertised no subgroups or a subgroup advertised no BIS.
    Empty,
    /// Bytes remained after all declared BASE subgroups and BIS entries.
    TrailingData,
}

/// A discovered Broadcast Audio Announcement and the advertiser needed to synchronize to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastSource {
    pub advertiser_address_type: AddrKind,
    pub advertiser_address: BdAddr,
    pub advertising_sid: u8,
    pub broadcast_id: [u8; 3],
}

/// One BIS entry decoded from a BASE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBis {
    /// One-based BIS index.
    pub index: u8,
    /// BIS-specific Codec_Specific_Configuration LTV bytes.
    pub codec_configuration: alloc::vec::Vec<u8>,
}

/// One subgroup decoded from a BASE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseSubgroup {
    pub codec_id: CodecId,
    /// Subgroup-level Codec_Specific_Configuration LTV bytes.
    pub codec_configuration: alloc::vec::Vec<u8>,
    /// Subgroup metadata LTV bytes.
    pub metadata: alloc::vec::Vec<u8>,
    pub bis: alloc::vec::Vec<BaseBis>,
}

/// A decoded Basic Audio Announcement (BASE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub presentation_delay_us: u32,
    pub subgroups: alloc::vec::Vec<BaseSubgroup>,
}

fn service_data(data: &[u8], uuid: u16) -> Result<Option<&[u8]>, BroadcastParseError> {
    let mut offset = 0;
    while offset < data.len() {
        let len = usize::from(data[offset]);
        if len == 0 {
            return if data[offset + 1..].iter().all(|byte| *byte == 0) {
                Ok(None)
            } else {
                Err(BroadcastParseError::InvalidAdLength)
            };
        }
        let end = offset
            .checked_add(len + 1)
            .filter(|end| *end <= data.len())
            .ok_or(BroadcastParseError::Truncated)?;
        let structure = &data[offset + 1..end];
        if structure.len() >= 3
            && structure[0] == 0x16
            && u16::from_le_bytes([structure[1], structure[2]]) == uuid
        {
            return Ok(Some(&structure[3..]));
        }
        offset = end;
    }
    Ok(None)
}

/// Finds a Broadcast Audio Announcement service-data structure in an extended advertising
/// payload. Other AD structures are ignored.
pub fn parse_broadcast_audio_announcement(
    data: &[u8],
) -> Result<Option<[u8; 3]>, BroadcastParseError> {
    let uuid = u16::from_le_bytes(service::BROADCAST_AUDIO_ANNOUNCEMENT.to_le_bytes());
    let Some(payload) = service_data(data, uuid)? else {
        return Ok(None);
    };
    let id = payload.get(..3).ok_or(BroadcastParseError::Truncated)?;
    Ok(Some([id[0], id[1], id[2]]))
}

struct BaseCursor<'a> {
    remaining: &'a [u8],
}

impl<'a> BaseCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { remaining: data }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], BroadcastParseError> {
        if self.remaining.len() < len {
            return Err(BroadcastParseError::Truncated);
        }
        let (value, remaining) = self.remaining.split_at(len);
        self.remaining = remaining;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, BroadcastParseError> {
        Ok(self.take(1)?[0])
    }

    fn len_prefixed(&mut self) -> Result<&'a [u8], BroadcastParseError> {
        let len = usize::from(self.u8()?);
        self.take(len)
    }
}

/// Decodes a BASE service-data payload (without its UUID).
pub fn decode_base(data: &[u8]) -> Result<Base, BroadcastParseError> {
    let mut cursor = BaseCursor::new(data);
    let delay = cursor.take(3)?;
    let presentation_delay_us = u32::from_le_bytes([delay[0], delay[1], delay[2], 0]);
    let subgroup_count = usize::from(cursor.u8()?);
    if subgroup_count == 0 {
        return Err(BroadcastParseError::Empty);
    }

    let mut subgroups = alloc::vec::Vec::with_capacity(subgroup_count);
    for _ in 0..subgroup_count {
        let bis_count = usize::from(cursor.u8()?);
        if bis_count == 0 {
            return Err(BroadcastParseError::Empty);
        }
        let codec_id = CodecId::from_le_bytes(
            cursor
                .take(CodecId::SIZE)?
                .try_into()
                .map_err(|_| BroadcastParseError::Truncated)?,
        );
        let codec_configuration = alloc::vec::Vec::from(cursor.len_prefixed()?);
        let metadata = alloc::vec::Vec::from(cursor.len_prefixed()?);
        let mut bis = alloc::vec::Vec::with_capacity(bis_count);
        for _ in 0..bis_count {
            bis.push(BaseBis {
                index: cursor.u8()?,
                codec_configuration: alloc::vec::Vec::from(cursor.len_prefixed()?),
            });
        }
        subgroups.push(BaseSubgroup {
            codec_id,
            codec_configuration,
            metadata,
            bis,
        });
    }
    if !cursor.remaining.is_empty() {
        return Err(BroadcastParseError::TrailingData);
    }
    Ok(Base {
        presentation_delay_us,
        subgroups,
    })
}

/// Finds and decodes a Basic Audio Announcement service-data structure in periodic advertising.
pub fn parse_basic_audio_announcement(data: &[u8]) -> Result<Option<Base>, BroadcastParseError> {
    let uuid = u16::from_le_bytes(service::BASIC_AUDIO_ANNOUNCEMENT.to_le_bytes());
    service_data(data, uuid)?.map(decode_base).transpose()
}

/// Everything needed to put one subgroup of up-to-stereo LC3 broadcast audio on the air.
pub struct BroadcastConfig {
    /// BIG_Handle this source creates (0x00-0xEF, host-assigned).
    pub big_handle: u8,
    /// Extended advertising set handle this module creates and owns - must not collide with any
    /// set trouble-host's own advertising uses.
    pub adv_handle: u8,
    /// Advertising SID carried in the extended advertisements (0x0-0xF).
    pub adv_sid: u8,
    /// The random advertiser address for this set.
    pub random_addr: [u8; 6],
    /// 24-bit Broadcast_ID (BAP: generated randomly once per broadcast).
    pub broadcast_id: [u8; 3],
    /// Audio channel allocation per BIS, in BIS_index order - the length picks Num_BIS.
    pub bis: HVec<AudioLocation, MAX_BIS>,
    pub sampling_frequency: SamplingFrequency,
    pub frame_duration: FrameDuration,
    /// Octets per codec frame (also sent as Max_SDU).
    pub octets_per_frame: u16,
    /// Microseconds between SDUs (10000 for 10 ms LC3 frames).
    pub sdu_interval_us: u32,
    pub max_transport_latency_ms: u16,
    pub rtn: u8,
    /// Presentation_Delay advertised in the BASE, microseconds (24-bit).
    pub presentation_delay_us: u32,
    /// Streaming_Audio_Contexts metadata for the subgroup.
    pub streaming_contexts: crate::generic_audio::ContextType,
    /// `Some(code)` produces an encrypted BIG (Broadcast_Code as the wire's 16 LSO-first octets).
    pub broadcast_code: Option<[u8; 16]>,
}

/// Invalid host-side input rejected before creating a broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastSourceConfigError {
    InvalidBigHandle,
    InvalidAdvertisingHandle,
    InvalidAdvertisingSid,
    InvalidRandomAddress,
    EmptyBis,
    InvalidSduInterval,
    InvalidTransportLatency,
    InvalidRetransmissionNumber,
    InvalidPresentationDelay,
    InvalidOctetsPerFrame,
}

/// Failure to start an Auracast broadcast source.
#[derive(Debug)]
pub enum BroadcastSourceError<E> {
    InvalidConfig(BroadcastSourceConfigError),
    Host(BleHostError<E>),
}

impl<E> From<BleHostError<E>> for BroadcastSourceError<E> {
    fn from(error: BleHostError<E>) -> Self {
        Self::Host(error)
    }
}

impl BroadcastConfig {
    /// Validates the Core and one-LC3-frame-per-SDU constraints used by [`start_broadcast`].
    pub fn validate(&self) -> Result<(), BroadcastSourceConfigError> {
        if self.big_handle > 0xef {
            return Err(BroadcastSourceConfigError::InvalidBigHandle);
        }
        if self.adv_handle > 0xef {
            return Err(BroadcastSourceConfigError::InvalidAdvertisingHandle);
        }
        if self.adv_sid > 0x0f {
            return Err(BroadcastSourceConfigError::InvalidAdvertisingSid);
        }
        let random_part_all_zero =
            self.random_addr[..5].iter().all(|byte| *byte == 0) && self.random_addr[5] & 0x3f == 0;
        let random_part_all_one = self.random_addr[..5].iter().all(|byte| *byte == 0xff)
            && self.random_addr[5] & 0x3f == 0x3f;
        if self.random_addr[5] & 0xc0 != 0xc0 || random_part_all_zero || random_part_all_one {
            return Err(BroadcastSourceConfigError::InvalidRandomAddress);
        }
        if self.bis.is_empty() {
            return Err(BroadcastSourceConfigError::EmptyBis);
        }
        let expected_interval = match self.frame_duration {
            FrameDuration::Duration7_5MS => 7_500,
            FrameDuration::Duration10MS => 10_000,
        };
        if self.sdu_interval_us != expected_interval {
            return Err(BroadcastSourceConfigError::InvalidSduInterval);
        }
        if !(5..=4_000).contains(&self.max_transport_latency_ms) {
            return Err(BroadcastSourceConfigError::InvalidTransportLatency);
        }
        if self.rtn > 30 {
            return Err(BroadcastSourceConfigError::InvalidRetransmissionNumber);
        }
        if self.presentation_delay_us > 0x00ff_ffff {
            return Err(BroadcastSourceConfigError::InvalidPresentationDelay);
        }
        if self.octets_per_frame == 0 {
            return Err(BroadcastSourceConfigError::InvalidOctetsPerFrame);
        }
        Ok(())
    }
}

/// Encodes the BASE (Basic Audio Announcement service data payload, BAP §3.7.2.2) for `config`:
/// one subgroup, one BIS per configured channel. Pure and allocation-transparent for testing.
pub fn encode_base(config: &BroadcastConfig) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    out.extend_from_slice(&config.presentation_delay_us.to_le_bytes()[..3]);
    out.push(1); // Num_Subgroups
    out.push(config.bis.len() as u8); // Num_BIS
    out.extend_from_slice(&CodecId::new(CodingFormat::LC3).to_le_bytes());
    let subgroup_config = encode_list(&[
        CodecSpecificConfiguration::SamplingFrequency(config.sampling_frequency),
        CodecSpecificConfiguration::FrameDuration(config.frame_duration),
        CodecSpecificConfiguration::OctetsPerCodecFrame(config.octets_per_frame),
    ]);
    out.push(subgroup_config.len() as u8);
    out.extend_from_slice(&subgroup_config);
    let metadata = encode_list(&[Metadata::StreamingAudioContexts(config.streaming_contexts)]);
    out.push(metadata.len() as u8);
    out.extend_from_slice(&metadata);
    for (i, location) in config.bis.iter().enumerate() {
        out.push(i as u8 + 1); // BIS_index, 1-based
        let bis_config = encode_list(&[CodecSpecificConfiguration::AudioChannelAllocation(
            *location,
        )]);
        out.push(bis_config.len() as u8);
        out.extend_from_slice(&bis_config);
    }
    out
}

/// The extended-advertising AD payload announcing this broadcast: one Service Data AD structure
/// with the Broadcast Audio Announcement Service UUID and the Broadcast_ID.
pub fn broadcast_audio_announcement(broadcast_id: [u8; 3]) -> [u8; 7] {
    let uuid = service::BROADCAST_AUDIO_ANNOUNCEMENT.to_le_bytes();
    [
        6,
        0x16,
        uuid[0],
        uuid[1],
        broadcast_id[0],
        broadcast_id[1],
        broadcast_id[2],
    ]
}

/// Wraps a BASE in the periodic-advertising AD structure (Service Data, Basic Audio Announcement
/// Service) the spec requires.
pub fn basic_audio_announcement(base: &[u8]) -> alloc::vec::Vec<u8> {
    let uuid = service::BASIC_AUDIO_ANNOUNCEMENT.to_le_bytes();
    let mut out = alloc::vec::Vec::with_capacity(base.len() + 4);
    out.push(base.len() as u8 + 3);
    out.push(0x16);
    out.extend_from_slice(&uuid);
    out.extend_from_slice(base);
    out
}

/// A data-path action decided by [`BigSource::on_big_established`], carried out by [`drive_big`].
enum BigAction {
    SetupDataPath(ConnHandle, u8),
}

/// Broadcast-source counterpart of [`crate::cig::CigManager`]: owns the BIS handles once the
/// controller reports `LE Create BIG Complete`, sets up each BIS's ISO data path, and hands the
/// handles to the application for [`crate::iso_tx`] transmission. See the module docs.
pub struct BigSource<M: RawMutex> {
    big_handle: u8,
    bis_handles: RefCell<HVec<ConnHandle, MAX_BIS>>,
    actions: Channel<M, BigAction, 4>,
    ready: Channel<M, u8, 4>,
}

impl<M: RawMutex> BigSource<M> {
    /// `big_handle` must match [`BroadcastConfig::big_handle`].
    pub fn new(big_handle: u8) -> Self {
        Self {
            big_handle,
            bis_handles: RefCell::new(HVec::new()),
            actions: Channel::new(),
            ready: Channel::new(),
        }
    }

    /// Waits until the next BIS (0-based index, matching [`BroadcastConfig::bis`] order) has its
    /// ISO data path up and is ready for [`crate::iso_tx`] traffic.
    pub async fn next_ready_bis(&self) -> u8 {
        self.ready.receive().await
    }

    /// The ISO connection handle for the given 0-based BIS index, once established.
    pub fn bis_handle(&self, index: usize) -> Option<ConnHandle> {
        self.bis_handles.borrow().get(index).copied()
    }
}

impl<M: RawMutex> EventHandler for BigSource<M> {
    fn on_big_established(&self, event: &LeCreateBigComplete<'_>) {
        if event.status.to_result().is_err() {
            warn!("[big] BIG creation failed");
            return;
        }
        if event.big_handle.into_inner() != self.big_handle {
            return;
        }
        let mut handles = self.bis_handles.borrow_mut();
        handles.clear();
        for (i, bis) in event.bis_handles.iter().enumerate() {
            let Ok(handle) = bis.handle() else {
                warn!("[big] malformed BIS handle in LE Create BIG Complete");
                continue;
            };
            if handles.push(handle).is_err() {
                warn!("[big] controller reported more BIS than MAX_BIS");
                break;
            }
            let _ = self
                .actions
                .try_send(BigAction::SetupDataPath(handle, i as u8));
        }
        info!(
            "[big] BIG established ({} BIS), setting up ISO data paths",
            handles.len()
        );
    }
}

/// Runs the whole broadcast bring-up sequence: extended + periodic advertising (announcement and
/// BASE), then `LE Create BIG`. Returns once the commands are issued - BIS readiness arrives via
/// [`BigSource::next_ready_bis`], which requires [`drive_big`] and
/// [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler)`(&source)`
/// to be polled concurrently.
pub async fn start_broadcast<C, M: RawMutex>(
    stack: &Stack<'_, C, impl PacketPool>,
    source: &BigSource<M>,
    config: &BroadcastConfig,
) -> Result<(), BroadcastSourceError<C::Error>>
where
    C: Controller
        + ControllerCmdSync<LeSetExtAdvParams>
        + ControllerCmdSync<LeSetAdvSetRandomAddr>
        + for<'a> ControllerCmdSync<LeSetExtAdvData<'a>>
        + ControllerCmdSync<LeSetPeriodicAdvParams>
        + for<'a> ControllerCmdSync<LeSetPeriodicAdvData<'a>>
        + for<'a> ControllerCmdSync<LeSetExtAdvEnable<'a>>
        + ControllerCmdSync<LeSetPeriodicAdvEnable>
        + ControllerCmdAsync<LeCreateBig>,
{
    config
        .validate()
        .map_err(BroadcastSourceError::InvalidConfig)?;
    let adv_handle = AdvHandle::new(config.adv_handle);

    // Non-connectable, non-scannable extended advertising - the only shape that may carry a
    // periodic train.
    stack
        .command(LeSetExtAdvParams::new(
            adv_handle,
            AdvEventProps::new(),
            ExtDuration::from_millis(100),
            ExtDuration::from_millis(150),
            AdvChannelMap::ALL,
            AddrKind::RANDOM,
            AddrKind::PUBLIC,
            BdAddr::default(),
            AdvFilterPolicy::default(),
            0x7F, // no preference on TX power
            PhyKind::Le1M,
            0,
            PhyKind::Le2M,
            config.adv_sid,
            false,
        ))
        .await?;
    stack
        .command(LeSetAdvSetRandomAddr::new(
            adv_handle,
            BdAddr::new(config.random_addr),
        ))
        .await?;
    stack
        .command(LeSetExtAdvData::new(
            adv_handle,
            Operation::Complete,
            false,
            &broadcast_audio_announcement(config.broadcast_id),
        ))
        .await?;

    // The periodic train the BIG rides on, carrying the BASE.
    stack
        .command(LeSetPeriodicAdvParams::new(
            adv_handle,
            Duration::from_millis(100),
            Duration::from_millis(150),
            PeriodicAdvProps::new(),
        ))
        .await?;
    let base = encode_base(config);
    stack
        .command(LeSetPeriodicAdvData::new(
            adv_handle,
            Operation::Complete,
            &basic_audio_announcement(&base),
        ))
        .await?;
    stack
        .command(LeSetExtAdvEnable::new(
            true,
            &[AdvSet {
                adv_handle,
                duration: Duration::from_secs(0),
                max_ext_adv_events: 0,
            }],
        ))
        .await?;
    stack
        .command(LeSetPeriodicAdvEnable::new(true, adv_handle))
        .await?;

    let (encryption, code) = match config.broadcast_code {
        Some(code) => (EncryptionMode::Encrypted, code),
        None => (EncryptionMode::Unencrypted, [0u8; 16]),
    };
    stack
        .iso()
        .command_async(LeCreateBig::new(
            bt_hci::param::BigHandle(source.big_handle),
            adv_handle,
            config.bis.len() as u8,
            ExtDuration::from_micros(u64::from(config.sdu_interval_us)),
            config.octets_per_frame,
            config.max_transport_latency_ms,
            config.rtn,
            PhyMask::new().set_le_2m_phy(true),
            Packing::Sequential,
            Framing::Unframed,
            encryption,
            bt_hci::param::BroadcastCode::new(code),
        ))
        .await?;
    Ok(())
}

/// Tears the broadcast down: terminates the BIG and stops/removes the advertising sets.
pub async fn stop_broadcast<C>(
    stack: &Stack<'_, C, impl PacketPool>,
    config: &BroadcastConfig,
) -> Result<(), BleHostError<C::Error>>
where
    C: Controller
        + ControllerCmdAsync<LeTerminateBig>
        + ControllerCmdSync<LeSetPeriodicAdvEnable>
        + for<'a> ControllerCmdSync<LeSetExtAdvEnable<'a>>
        + ControllerCmdSync<LeRemoveAdvSet>,
{
    /// "Remote User Terminated Connection" - the conventional reason for a host-initiated stop.
    const REASON_REMOTE_USER_TERMINATED: u8 = 0x13;
    let adv_handle = AdvHandle::new(config.adv_handle);
    stack
        .iso()
        .command_async(LeTerminateBig::new(
            bt_hci::param::BigHandle(config.big_handle),
            REASON_REMOTE_USER_TERMINATED,
        ))
        .await?;
    stack
        .command(LeSetPeriodicAdvEnable::new(false, adv_handle))
        .await?;
    stack.command(LeSetExtAdvEnable::new(false, &[])).await?;
    stack.command(LeRemoveAdvSet::new(adv_handle)).await?;
    Ok(())
}

/// Drives the awaited HCI side of BIS bring-up (ISO data path setup), as decided by
/// [`BigSource`]'s [`EventHandler`]. Poll concurrently with the runner, like
/// [`crate::cig::drive_cig`].
pub async fn drive_big<C, M: RawMutex>(
    stack: &Stack<'_, C, impl PacketPool>,
    source: &BigSource<M>,
) -> !
where
    C: Controller
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>,
{
    let iso = stack.iso();
    loop {
        match source.actions.receive().await {
            BigAction::SetupDataPath(handle, index) => {
                let result = iso
                    .command(LeSetupIsoDataPath::new(
                        handle,
                        DataPathDirection::Input,
                        DataPathId::HCI,
                        bt_hci::param::CodecId {
                            coding_format: u8::from(CodingFormat::Transparent),
                            company_id: 0,
                            vendor_specific_codec_id: 0,
                        },
                        ExtDuration::from_u32(0),
                        &[],
                    ))
                    .await;
                match result {
                    Ok(_) => {
                        info!("[big] ISO data path up for BIS {}", index);
                        let _ = source.ready.try_send(index);
                    }
                    Err(_e) => {
                        warn!("[big] LE Setup ISO Data Path failed for BIS {}", index);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generic_audio::ContextType;

    fn config() -> BroadcastConfig {
        let mut bis = HVec::new();
        let _ = bis.push(AudioLocation::FrontLeft);
        let _ = bis.push(AudioLocation::FrontRight);
        BroadcastConfig {
            big_handle: 0,
            adv_handle: 1,
            adv_sid: 0,
            random_addr: [1, 2, 3, 4, 5, 0xC0],
            broadcast_id: [0xAB, 0xCD, 0xEF],
            bis,
            sampling_frequency: SamplingFrequency::Hz48000,
            frame_duration: FrameDuration::Duration10MS,
            octets_per_frame: 100,
            sdu_interval_us: 10_000,
            max_transport_latency_ms: 20,
            rtn: 2,
            presentation_delay_us: 40_000,
            streaming_contexts: ContextType::Media,
            broadcast_code: None,
        }
    }

    /// Field-by-field check of the BASE wire structure (BAP §3.7.2.2) for a stereo subgroup.
    #[test]
    fn base_encodes_presentation_delay_subgroup_and_per_bis_structure() {
        let base = encode_base(&config());
        assert_eq!(&base[..3], &40_000u32.to_le_bytes()[..3]); // Presentation_Delay
        assert_eq!(base[3], 1); // Num_Subgroups
        assert_eq!(base[4], 2); // Num_BIS
        assert_eq!(&base[5..10], &CodecId::new(CodingFormat::LC3).to_le_bytes()); // Codec_ID
        let cfg_len = base[10] as usize;
        // Sampling frequency (1-indexed on the wire: 0x08 = 48 kHz) leads the subgroup config.
        assert_eq!(&base[11..13], &[2, 1]); // LTV header: length 2, type 1
        assert_eq!(base[13], 0x08);
        let metadata_len_at = 11 + cfg_len;
        let metadata_len = base[metadata_len_at] as usize;
        // Streaming_Audio_Contexts (type 2) with the Media bit.
        assert_eq!(&base[metadata_len_at + 1..metadata_len_at + 3], &[3, 2]);
        // First BIS entry: index 1, then its channel-allocation LTV.
        let bis0 = metadata_len_at + 1 + metadata_len;
        assert_eq!(base[bis0], 1);
        assert_eq!(&base[bis0 + 1..bis0 + 4], &[6, 5, 3]); // len, LTV len 5, type 3
        assert_eq!(
            &base[bis0 + 4..bis0 + 8],
            &AudioLocation::FrontLeft.bits().to_le_bytes()
        );
        // Second BIS entry directly follows.
        assert_eq!(base[bis0 + 8], 2);
    }

    #[test]
    fn announcement_ad_structures_are_well_formed() {
        let ext = broadcast_audio_announcement([0xAB, 0xCD, 0xEF]);
        assert_eq!(ext[0] as usize, ext.len() - 1); // AD length covers type + payload
        assert_eq!(ext[1], 0x16); // Service Data - 16-bit UUID
        assert_eq!(&ext[2..4], &0x1852u16.to_le_bytes());
        assert_eq!(&ext[4..7], &[0xAB, 0xCD, 0xEF]);

        let base = encode_base(&config());
        let periodic = basic_audio_announcement(&base);
        assert_eq!(periodic[0] as usize, periodic.len() - 1);
        assert_eq!(periodic[1], 0x16);
        assert_eq!(&periodic[2..4], &0x1851u16.to_le_bytes());
        assert_eq!(&periodic[4..], &base[..]);
    }

    #[test]
    fn announcement_parser_ignores_unrelated_ad_structures() {
        let mut advertisement = alloc::vec![2, 0x01, 0x06];
        advertisement.extend_from_slice(&broadcast_audio_announcement([0x12, 0x34, 0x56]));
        advertisement.extend_from_slice(&[3, 0x09, b'L', b'E']);

        assert_eq!(
            parse_broadcast_audio_announcement(&advertisement).unwrap(),
            Some([0x12, 0x34, 0x56])
        );
    }

    #[test]
    fn source_base_round_trips_through_structural_decoder() {
        let encoded = encode_base(&config());
        let base = decode_base(&encoded).unwrap();

        assert_eq!(base.presentation_delay_us, 40_000);
        assert_eq!(base.subgroups.len(), 1);
        assert_eq!(base.subgroups[0].codec_id, CodecId::new(CodingFormat::LC3));
        assert_eq!(base.subgroups[0].bis[0].index, 1);
        assert_eq!(base.subgroups[0].bis[1].index, 2);
    }

    #[test]
    fn every_truncated_source_base_is_rejected_without_panicking() {
        let encoded = encode_base(&config());
        for len in 0..encoded.len() {
            assert!(
                decode_base(&encoded[..len]).is_err(),
                "accepted truncation at {len}"
            );
        }
    }

    #[test]
    fn malformed_ad_length_is_rejected() {
        let truncated = [6, 0x16, 0x52, 0x18, 1, 2];
        assert_eq!(
            parse_broadcast_audio_announcement(&truncated),
            Err(BroadcastParseError::Truncated)
        );
    }

    #[test]
    fn source_config_rejects_values_that_would_be_truncated_or_rejected_by_hci() {
        let mut invalid = config();
        invalid.adv_sid = 16;
        assert_eq!(
            invalid.validate(),
            Err(BroadcastSourceConfigError::InvalidAdvertisingSid)
        );

        invalid = config();
        invalid.random_addr = [0; 6];
        assert_eq!(
            invalid.validate(),
            Err(BroadcastSourceConfigError::InvalidRandomAddress)
        );

        invalid = config();
        invalid.bis.clear();
        assert_eq!(
            invalid.validate(),
            Err(BroadcastSourceConfigError::EmptyBis)
        );

        invalid = config();
        invalid.presentation_delay_us = 0x0100_0000;
        assert_eq!(
            invalid.validate(),
            Err(BroadcastSourceConfigError::InvalidPresentationDelay)
        );

        invalid = config();
        invalid.sdu_interval_us = 7_500;
        assert_eq!(
            invalid.validate(),
            Err(BroadcastSourceConfigError::InvalidSduInterval)
        );
    }
}
