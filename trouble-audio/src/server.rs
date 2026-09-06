use core::marker::PhantomData;

use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::{
    gatt::{GattConnection, GattEvent, ReadEvent, WriteEvent},
    prelude::{service, AsGatt, AttErrorCode, AttributeServer, AttributeTable, DefaultPacketPool, PacketPool},
};


use alloc::vec::Vec as AVec;

use crate::{
    aics::{self, AicsServer, AicsStorage, AudioInputState, AudioInputType, AICS_ATTRIBUTES},
    ascs::{AscsServer, AscsStorage, AseType},
    bap,
    bass::{self, BassServer, BassStorage, BASS_ATTRIBUTES},
    cis::CisManager,
    csis::{CsisServer, CsisStorage, Lock, Sirk, CSIS_ATTRIBUTES},
    generic_audio::AudioLocation,
    gmas::{BgrFeatures, BgsFeatures, GmapRole, GmasServer, GmasStorage, UggFeatures, UgtFeatures, GMAS_ATTRIBUTES},
    has::{self, HasServer, HasStorage, PresetRecord, HAS_ATTRIBUTES},
    mcs::{self, McsServer, McsStorage, MCS_ATTRIBUTES},
    mics::{MicsServer, MicsStorage, Mute, MICS_ATTRIBUTES},
    ots::{self, ObjectRecord, OtsFeature, OtsServer, OtsStorage, OTS_ATTRIBUTES},
    pacs::{AudioContexts, PacsServer, PacsStorage, PAC, PACS_ATTRIBUTES},
    tbs::{self, TbsInit, TbsServer, TbsStorage, TBS_ATTRIBUTES},
    tmas::{TmapRole, TmasServer, TmasStorage, TMAS_ATTRIBUTES},
    vcs::{self, VcsServer, VcsStorage, VCS_ATTRIBUTES},
    vocs::{self, VocsServer, VocsStorage, VolumeOffsetState, VOCS_ATTRIBUTES},
};

/// Fixed capacities for the const-generic LE Audio services (HAS/BASS/TBS/OTS) when wired through
/// [`ServerBuilder`] - chosen as reasonable demo/example defaults, not spec limits. A caller
/// needing different capacities can construct those servers directly instead of going through
/// `ServerBuilder`.
pub const HAS_MAX_PRESETS: usize = 4;
pub const BASS_MAX_SOURCES: usize = 2;
pub const TBS_MAX_CALLS: usize = 4;
pub const OTS_MAX_OBJECTS: usize = 4;

pub const MAX_SERVICES: usize = 4 // gap + gatt
     + 1 // cas
     + PACS_ATTRIBUTES
     + 15 // ascs
     + MICS_ATTRIBUTES
     + VCS_ATTRIBUTES
     + CSIS_ATTRIBUTES
     + MCS_ATTRIBUTES
     + GMAS_ATTRIBUTES
     + TMAS_ATTRIBUTES
     + AICS_ATTRIBUTES
     + VOCS_ATTRIBUTES
     + HAS_ATTRIBUTES
     + BASS_ATTRIBUTES
     + TBS_ATTRIBUTES
     + OTS_ATTRIBUTES
     ;

/// Implemented by each LE Audio GATT service (PACS, ASCS, ...) so [`Server`] can dispatch
/// incoming reads/writes to whichever service owns the handle.
pub trait LeAudioServerService<P: PacketPool> {
    fn handle_read_event(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>>;
    fn handle_write_event(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>>;
}

pub struct ServerBuilder<'a, const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M, P = DefaultPacketPool>
where
    M: RawMutex,
    P: PacketPool,
{
    table: AttributeTable<'a, M, MAX_SERVICES>,
    pacs: Option<PacsServer>,
    ascs: Option<AscsServer<MAX_ASES>>,
    cis: Option<&'a CisManager<M, MAX_ASES>>,
    mics: Option<MicsServer>,
    vcs: Option<VcsServer>,
    csis: Option<CsisServer>,
    mcs: Option<McsServer>,
    gmas: Option<GmasServer>,
    tmas: Option<TmasServer>,
    aics: Option<AicsServer>,
    vocs: Option<VocsServer>,
    has: Option<HasServer<HAS_MAX_PRESETS>>,
    bass: Option<BassServer<BASS_MAX_SOURCES>>,
    tbs: Option<TbsServer<TBS_MAX_CALLS>>,
    ots: Option<OtsServer<OTS_MAX_OBJECTS>>,
    _p: PhantomData<P>,
}

impl<'a, const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M, P>
    ServerBuilder<'a, MAX_ASES, MAX_CONNECTIONS, M, P>
where
    M: RawMutex,
    P: PacketPool,
{
    /// Starts a new GATT table with the mandatory GAP and GATT services.
    pub fn new(name_id: &'a (impl AsGatt + ?Sized), appearance: &'a (impl AsGatt + ?Sized)) -> Self {
        let mut table: AttributeTable<'_, M, MAX_SERVICES> = AttributeTable::new();
        let mut svc = table.add_service(trouble_host::attribute::Service::new(0x1800u16));
        let _ = svc.add_characteristic_ro(0x2a00u16, name_id);
        let _ = svc.add_characteristic_ro(0x2a01u16, appearance);
        svc.build();

        // Generic attribute service (mandatory)
        table.add_service(trouble_host::attribute::Service::new(0x1801u16));

        // Common Audio Service (mandatory for CAP compliance): marks this device as a valid LE
        // Audio peripheral. No characteristics of its own for a non-coordinated single device;
        // without it, some LE Audio-aware stacks (e.g. Android's system Bluetooth settings) will
        // pair and encrypt successfully but then refuse the connection outright.
        table.add_service(trouble_host::attribute::Service::new(service::COMMON_AUDIO));

        Self {
            table,
            pacs: None,
            ascs: None,
            cis: None,
            mics: None,
            vcs: None,
            csis: None,
            mcs: None,
            gmas: None,
            tmas: None,
            aics: None,
            vocs: None,
            has: None,
            bass: None,
            tbs: None,
            ots: None,
            _p: PhantomData,
        }
    }

    /// Finishes construction. Panics if [`Self::add_pacs`] was never called - PACS is mandatory.
    pub fn build(self) -> Server<'a, MAX_ASES, MAX_CONNECTIONS, M, P> {
        Server {
            server: AttributeServer::<M, P, MAX_SERVICES, MAX_CONNECTIONS>::new(self.table),
            pacs: self.pacs.expect("Pacs is a mandatory service"),
            ascs: self.ascs,
            cis: self.cis,
            mics: self.mics,
            vcs: self.vcs,
            csis: self.csis,
            mcs: self.mcs,
            gmas: self.gmas,
            tmas: self.tmas,
            aics: self.aics,
            vocs: self.vocs,
            has: self.has,
            bass: self.bass,
            tbs: self.tbs,
            ots: self.ots,
        }
    }

    /// Adds the (mandatory) Published Audio Capabilities service.
    pub fn add_pacs(
        mut self,
        sink_pac: Option<&'a PAC>,
        sink_audio_locations: Option<&'a AudioLocation>,
        source_pac: Option<&'a PAC>,
        source_audio_locations: Option<&'a AudioLocation>,
        supported_audio_contexts: &'a AudioContexts,
        available_audio_contexts: &'a AudioContexts,
        storage: &'a mut PacsStorage,
    ) -> Self {
        let pacs = PacsServer::new(
            &mut self.table,
            sink_pac.map(|pac| (pac, &mut storage.sink_pac[..])),
            sink_audio_locations.map(|loc| (loc, &mut storage.sink_audio_locations[..])),
            source_pac.map(|pac| (pac, &mut storage.source_pac[..])),
            source_audio_locations.map(|loc| (loc, &mut storage.source_audio_locations[..])),
            supported_audio_contexts,
            available_audio_contexts,
            &mut storage.available_audio_contexts,
        );
        self.pacs = Some(pacs);
        self
    }

    /// Adds the (optional) Audio Stream Control service with the given initial ASEs.
    pub fn add_ascs(mut self, ases: Vec<AseType, MAX_ASES>, storage: &'a mut AscsStorage<MAX_ASES>) -> Self {
        let ascs = AscsServer::new(
            &mut self.table,
            ases,
            &mut storage.ase_control_point,
            storage.ases.iter_mut().map(|s| &mut s[..]),
        );
        self.ascs = Some(ascs);
        self
    }

    /// Wires up a [`CisManager`] so ASE Control Point writes feed its codec/CIG/CIS side-table
    /// (see [`CisManager::observe_operation`]). Requires [`Self::add_ascs`] to have been called.
    pub fn add_cis_manager(mut self, cis: &'a CisManager<M, MAX_ASES>) -> Self {
        self.cis = Some(cis);
        self
    }

    /// Adds the (optional) Microphone Control service.
    pub fn add_mics(mut self, initial: Mute, storage: &'a mut MicsStorage) -> Self {
        self.mics = Some(MicsServer::new(&mut self.table, initial, &mut storage.mute));
        self
    }

    /// Adds the (optional) Volume Control service.
    pub fn add_vcs(mut self, initial: vcs::VolumeState, flags: vcs::VolumeFlags, step: u8, storage: &'a mut VcsStorage) -> Self {
        self.vcs = Some(VcsServer::new(
            &mut self.table,
            initial,
            flags,
            step,
            &mut storage.volume_state,
            &mut storage.volume_control_point,
            &mut storage.volume_flags,
        ));
        self
    }

    /// Adds the (optional) Coordinated Set Identification service.
    pub fn add_csis(mut self, sirk: Sirk, set_size: Option<u8>, lock: Lock, rank: Option<u8>, storage: &'a mut CsisStorage) -> Self {
        self.csis = Some(CsisServer::new(
            &mut self.table,
            sirk,
            set_size,
            lock,
            rank,
            &mut storage.sirk,
            &mut storage.set_size,
            &mut storage.lock,
            &mut storage.rank,
        ));
        self
    }

    /// Adds the (optional) (Generic) Media Control service.
    /// `object_ids`, if given, additionally exposes the optional Object ID characteristics
    /// referencing objects served via [`Self::add_ots`].
    pub fn add_mcs(
        mut self,
        init: mcs::McsInit,
        playing_orders_supported: &'a mcs::PlayingOrdersSupported,
        opcodes_supported: &'a mcs::MediaControlPointOpcodesSupported,
        object_ids: Option<mcs::McsObjectIds>,
        storage: &'a mut McsStorage,
    ) -> Self {
        self.mcs = Some(McsServer::new(
            &mut self.table,
            init,
            playing_orders_supported,
            opcodes_supported,
            object_ids,
            storage.as_store(),
        ));
        self
    }

    /// Adds the (optional) Gaming Audio service.
    pub fn add_gmas(
        mut self,
        role: GmapRole,
        ugg_features: Option<UggFeatures>,
        ugt_features: Option<UgtFeatures>,
        bgs_features: Option<BgsFeatures>,
        bgr_features: Option<BgrFeatures>,
        storage: &'a mut GmasStorage,
    ) -> Self {
        self.gmas = Some(GmasServer::new(
            &mut self.table,
            role,
            ugg_features,
            ugt_features,
            bgs_features,
            bgr_features,
            &mut storage.role,
            &mut storage.ugg_features,
            &mut storage.ugt_features,
            &mut storage.bgs_features,
            &mut storage.bgr_features,
        ));
        self
    }

    /// Adds the (optional) Telephony and Media Audio service.
    pub fn add_tmas(mut self, role: TmapRole, storage: &'a mut TmasStorage) -> Self {
        self.tmas = Some(TmasServer::new(&mut self.table, role, &mut storage.role));
        self
    }

    /// Adds the (optional) Audio Input Control service.
    pub fn add_aics(
        mut self,
        initial_state: AudioInputState,
        gain_settings_attribute: aics::GainSettingsAttribute,
        input_type: AudioInputType,
        initial_status: aics::AudioInputStatus,
        description: heapless::String<32>,
        storage: &'a mut AicsStorage,
    ) -> Self {
        let (gain_store, type_store, store) = storage.split();
        self.aics = Some(AicsServer::new(
            &mut self.table,
            initial_state,
            gain_settings_attribute,
            input_type,
            initial_status,
            description,
            gain_store,
            type_store,
            store,
        ));
        self
    }

    /// Adds the (optional) Volume Offset Control service.
    pub fn add_vocs(
        mut self,
        initial_state: VolumeOffsetState,
        audio_location: AudioLocation,
        description: heapless::String<32>,
        storage: &'a mut VocsStorage,
    ) -> Self {
        self.vocs = Some(VocsServer::new(
            &mut self.table,
            initial_state,
            audio_location,
            description,
            storage.as_store(),
        ));
        self
    }

    /// Adds the (optional) Hearing Access service.
    pub fn add_has(
        mut self,
        features: has::HearingAidFeatures,
        presets: heapless::Vec<PresetRecord, HAS_MAX_PRESETS>,
        active_preset_index: u8,
        storage: &'a mut HasStorage,
    ) -> Self {
        self.has = Some(HasServer::new(
            &mut self.table,
            features,
            presets,
            active_preset_index,
            &mut storage.features,
            &mut storage.preset_control_point,
            &mut storage.active_preset_index,
        ));
        self
    }

    /// Adds the (optional) Broadcast Audio Scan service, with `BASS_MAX_SOURCES` receive-state
    /// slots.
    pub fn add_bass(mut self, storage: &'a mut BassStorage<BASS_MAX_SOURCES>) -> Self {
        self.bass = Some(BassServer::new(
            &mut self.table,
            &mut storage.control_point,
            storage.receive_states.iter_mut().map(|s| &mut s[..]),
        ));
        self
    }

    /// Adds the (optional) (Generic) Telephone Bearer service.
    pub fn add_tbs(mut self, init: TbsInit, storage: &'a mut TbsStorage) -> Self {
        self.tbs = Some(TbsServer::new(&mut self.table, init, storage.as_store()));
        self
    }

    /// Adds the (optional) Object Transfer service, with the given initial object list.
    pub fn add_ots(mut self, feature: OtsFeature, objects: AVec<ObjectRecord>, storage: &'a mut OtsStorage) -> Self {
        self.ots = Some(OtsServer::new(&mut self.table, feature, objects, storage.as_store()));
        self
    }
}

pub struct Server<'a, const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M, P = DefaultPacketPool>
where
    M: RawMutex,
    P: PacketPool,
{
    pub server: AttributeServer<'a, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    pacs: PacsServer,
    ascs: Option<AscsServer<MAX_ASES>>,
    cis: Option<&'a CisManager<M, MAX_ASES>>,
    mics: Option<MicsServer>,
    vcs: Option<VcsServer>,
    csis: Option<CsisServer>,
    mcs: Option<McsServer>,
    gmas: Option<GmasServer>,
    tmas: Option<TmasServer>,
    aics: Option<AicsServer>,
    vocs: Option<VocsServer>,
    has: Option<HasServer<HAS_MAX_PRESETS>>,
    bass: Option<BassServer<BASS_MAX_SOURCES>>,
    tbs: Option<TbsServer<TBS_MAX_CALLS>>,
    ots: Option<OtsServer<OTS_MAX_OBJECTS>>,
}

/// Sends the ATT reply for a decoded write outcome: accept on `Ok`, reject with the code on
/// `Err`. A failure from `accept`/`reject` itself (peer already gone) is dropped, same as every
/// dispatch site below did inline before this was factored out.
async fn reply<P: PacketPool>(event: GattEvent<'_, '_, P>, outcome: Result<(), AttErrorCode>) {
    match outcome {
        Ok(()) => {
            if let Ok(reply) = event.accept() {
                reply.send().await;
            }
        }
        Err(err) => {
            if let Ok(reply) = event.reject(err) {
                reply.send().await;
            }
        }
    }
}

/// Tries `$self.pacs` (not an `Option`, unlike every other service) then each `$field` in order,
/// returning the first `Some`; mirrors the handle-collision order of [`Server::handle`].
macro_rules! dispatch_to_service {
    ($self:expr, $event:expr, $method:ident, [$($field:ident),+ $(,)?]) => {{
        if let Some(res) = $self.pacs.$method($event) {
            return Some(res);
        }
        $(
            if let Some(res) = $self.$field.as_ref().and_then(|svc| svc.$method($event)) {
                return Some(res);
            }
        )+
        None
    }};
}

impl<const MAX_ASES: usize, const MAX_CONNECTIONS: usize, M, P> Server<'_, MAX_ASES, MAX_CONNECTIONS, M, P>
where
    M: RawMutex,
    P: PacketPool,
{
    /// The simple LE Audio event loop: dispatches a [`GattEvent`] observed on `conn` to whichever
    /// service owns its handle. Writes to the ASE Control Point additionally drive the ASE state
    /// machine (see [`bap::drive_ase_control_point`]) and send back the Control Point
    /// notification the spec requires. Returns `false` for events this server doesn't otherwise
    /// touch (e.g. `GattEvent::Other`/`NotAllowed`), so the caller can still inspect them.
    pub async fn handle(&self, conn: &GattConnection<'_, '_, P>, event: GattEvent<'_, '_, P>) -> bool {
        // Family B (vcs/aics/vocs/has/bass): decode the control-point write, run it through
        // `drive_*`, and reply with whatever `Result` that returns.
        macro_rules! control_point_reply {
            ($svc:ident, $char:ident, $drive:path) => {
                if let (GattEvent::Write(write_event), Some($svc)) = (&event, &self.$svc) {
                    if write_event.handle() == $svc.$char().handle {
                        let operation = write_event.value($svc.$char());
                        let outcome = match operation {
                            Ok(operation) => $drive(&self.server, $svc, conn, &operation).await,
                            Err(_) => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
                        };
                        reply(event, outcome).await;
                        return true;
                    }
                }
            };
        }

        // Family C (mcs/tbs/ots OACP/ots OLCP): accept immediately, then drive only once the
        // value has decoded (a malformed write still gets the spec-mandated reply, just no
        // procedure to run).
        macro_rules! control_point_drive {
            ($svc:ident, $char:ident, $drive:path, $msg:literal) => {
                if let (GattEvent::Write(write_event), Some($svc)) = (&event, &self.$svc) {
                    if write_event.handle() == $svc.$char().handle {
                        let operation = write_event.value($svc.$char());
                        match event.accept() {
                            Ok(resp) => resp.send().await,
                            Err(_) => return true,
                        }
                        if let Ok(operation) = operation {
                            $drive(&self.server, $svc, conn, &operation).await;
                        } else {
                            warn!($msg);
                        }
                        return true;
                    }
                }
            };
        }

        // Family A (mics/csis): validate the written value against current state; on success
        // accept and notify the new value, on failure reject.
        if let (GattEvent::Write(write_event), Some(mics)) = (&event, &self.mics) {
            if write_event.handle() == mics.mute().handle {
                let outcome = write_event.value(mics.mute()).map_err(|_| AttErrorCode::WRITE_REQUEST_REJECTED).and_then(|requested| {
                    let current = mics.mute().get(&self.server).unwrap_or_default();
                    Mute::validate_client_write(current, requested)
                });
                match outcome {
                    Ok(new_mute) => {
                        reply(event, Ok(())).await;
                        let _ = mics.mute().notify(conn, &new_mute, true).await;
                    }
                    Err(err) => reply(event, Err(err)).await,
                }
                return true;
            }
        }

        control_point_reply!(vcs, volume_control_point, vcs::drive_volume_control_point);

        if let (GattEvent::Write(write_event), Some(csis)) = (&event, &self.csis) {
            if write_event.handle() == csis.lock().handle {
                let raw_byte = write_event.with_data(|_, data| data.first().copied());
                let outcome = match raw_byte {
                    Some(raw_byte) => {
                        let current = csis.lock().get(&self.server).unwrap_or_default();
                        Lock::validate_client_write(current, raw_byte)
                    }
                    None => Err(AttErrorCode::WRITE_REQUEST_REJECTED),
                };
                match outcome {
                    Ok(new_lock) => {
                        reply(event, Ok(())).await;
                        let _ = csis.lock().notify(conn, &new_lock, true).await;
                    }
                    Err(err) => reply(event, Err(err)).await,
                }
                return true;
            }
        }

        control_point_drive!(mcs, media_control_point, mcs::drive_media_control_point, "[le audio] malformed Media Control Point write");
        control_point_reply!(aics, audio_input_control_point, aics::drive_input_control_point);
        control_point_reply!(vocs, volume_offset_control_point, vocs::drive_volume_offset_control_point);
        control_point_reply!(has, preset_control_point, has::drive_preset_control_point);
        control_point_reply!(bass, control_point, bass::drive_control_point);
        control_point_drive!(tbs, call_control_point, tbs::drive_call_control_point, "[le audio] malformed Call Control Point write");
        control_point_drive!(ots, object_action_control_point, ots::drive_oacp, "[le audio] malformed Object Action Control Point write");
        control_point_drive!(ots, object_list_control_point, ots::drive_olcp, "[le audio] malformed Object List Control Point write");

        // ASCS also feeds CisManager's side-table before driving, so it doesn't fit family C.
        if let (GattEvent::Write(write_event), Some(ascs)) = (&event, &self.ascs) {
            if write_event.handle() == ascs.ase_control_point().handle {
                let operation = write_event.value(ascs.ase_control_point());
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(_) => return true,
                }
                // Decode the operation once here; both consumers below take the decoded form.
                if let Some(operation) = operation.ok().and_then(|op| op.operation().ok()) {
                    debug!("[le audio] ASE Control Point write: opcode {}", operation.opcode());
                    if let Some(cis) = self.cis {
                        cis.observe_operation(&self.server, ascs, &operation);
                    }
                    let _ = bap::drive_ase_control_point(&self.server, ascs, conn, &operation).await;
                } else {
                    warn!("[le audio] malformed ASE Control Point write");
                }
                return true;
            }
        }

        let result = match &event {
            GattEvent::Read(event) => self.handle_read(event),
            GattEvent::Write(event) => self.handle_write(event),
            _ => return false,
        };

        match result {
            Some(outcome) => reply(event, outcome).await,
            None => {
                // Neither PACS nor ASCS recognizes this handle as one of their own
                // characteristic *values* - which is also true of every CCCD (framework-managed,
                // not an application-level value). By the time an event reaches here it has
                // already passed `can_read`/`can_write`'s existence-and-permission check, so
                // "unrecognized" doesn't mean "invalid" - it means "let the attribute server
                // handle it generically" (e.g. actually storing a CCCD subscription). Rejecting
                // here instead silently discarded every CCCD write with an Invalid Handle error,
                // leaving centrals subscribed to nothing and notifications never delivered.
                if let Ok(reply) = event.accept() {
                    reply.send().await;
                }
            }
        }
        true
    }

    /// Autonomously transitions a Sink ASE to `Streaming` once its CIS/ISO data path is up (see
    /// [`bap::notify_ase_streaming`]). No-op if this server has no ASCS.
    pub async fn notify_ase_streaming(&self, conn: &GattConnection<'_, '_, P>, ase_id: u8) {
        if let Some(ascs) = &self.ascs {
            bap::notify_ase_streaming(&self.server, ascs, conn, ase_id).await;
        }
    }

    /// Completes a Release procedure after the ISO data path has been removed.
    pub async fn notify_ase_released(&self, conn: &GattConnection<'_, '_, P>, ase_id: u8) {
        if let Some(ascs) = &self.ascs {
            bap::notify_ase_released(&self.server, ascs, conn, ase_id).await;
        }
    }

    /// Restores an ASE's cached QoS state after an unexpected CIS link loss.
    pub async fn notify_ase_qos_configured(&self, conn: &GattConnection<'_, '_, P>, ase_id: u8) {
        if let Some(ascs) = &self.ascs {
            bap::notify_ase_qos_configured(&self.server, ascs, conn, ase_id).await;
        }
    }

    /// Clears connection-scoped ASE and CIS state after an ACL disconnect.
    pub fn reset_connection(&self) {
        if let Some(ascs) = &self.ascs {
            ascs.reset_connection(&self.server);
        }
        if let Some(cis) = self.cis {
            cis.reset_connection();
        }
    }

    /// This server's (Generic) Media Control service, if [`ServerBuilder::add_mcs`] was called -
    /// e.g. so a caller can read [`McsServer::media_state`] after [`Self::handle`] processes a
    /// Media Control Point write, to react to the resulting Play/Pause/... transition itself.
    pub fn mcs(&self) -> Option<&McsServer> {
        self.mcs.as_ref()
    }

    /// This server's Volume Control service, if [`ServerBuilder::add_vcs`] was called - e.g. so a
    /// caller can read [`VcsServer::volume_state`] after [`Self::handle`] processes a Volume
    /// Control Point write, to apply the resulting volume and mute to its own audio path.
    pub fn vcs(&self) -> Option<&VcsServer> {
        self.vcs.as_ref()
    }

    fn handle_read(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        dispatch_to_service!(self, event, handle_read_event, [ascs, mics, vcs, csis, mcs, gmas, tmas, aics, vocs, has, bass, tbs, ots])
    }

    fn handle_write(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        dispatch_to_service!(self, event, handle_write_event, [ascs, mics, vcs, csis, mcs, gmas, tmas, aics, vocs, has, bass, tbs, ots])
    }
}
