use core::marker::PhantomData;

use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec;
use trouble_host::{
    gatt::{GattConnection, GattEvent, ReadEvent, WriteEvent},
    prelude::{service, AsGatt, AttErrorCode, AttributeServer, AttributeTable, DefaultPacketPool, PacketPool},
};

#[cfg(feature = "defmt")]
use defmt::*;

use crate::{
    ascs::{AscsServer, AseType},
    bap,
    cis::CisManager,
    generic_audio::AudioLocation,
    pacs::{AudioContexts, PacsServer, PAC, PACS_ATTRIBUTES},
};

pub const MAX_SERVICES: usize = 4 // gap + gatt
     + 1 // cas
     + PACS_ATTRIBUTES
     + 15 // ascs
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
        }
    }

    /// Adds the (mandatory) Published Audio Capabilities service.
    pub fn add_pacs(
        mut self,
        sink_pac: Option<(&'a PAC, &'a mut [u8])>,
        sink_audio_locations: Option<(&'a AudioLocation, &'a mut [u8])>,
        source_pac: Option<(&'a PAC, &'a mut [u8])>,
        source_audio_locations: Option<(&'a AudioLocation, &'a mut [u8])>,
        supported_audio_contexts: &'a AudioContexts,
        available_audio_contexts: &'a AudioContexts,
        available_audio_contexts_store: &'a mut [u8],
    ) -> Self {
        let pacs = PacsServer::new(
            &mut self.table,
            sink_pac,
            sink_audio_locations,
            source_pac,
            source_audio_locations,
            supported_audio_contexts,
            available_audio_contexts,
            available_audio_contexts_store,
        );
        self.pacs = Some(pacs);
        self
    }

    /// Adds the (optional) Audio Stream Control service with the given initial ASEs.
    pub fn add_ascs(mut self, ases: Vec<AseType, MAX_ASES>) -> Self {
        let ascs = AscsServer::new(&mut self.table, ases);
        self.ascs = Some(ascs);
        self
    }

    /// Wires up a [`CisManager`] so ASE Control Point writes feed its codec/CIG/CIS side-table
    /// (see [`CisManager::observe_operation`]). Requires [`Self::add_ascs`] to have been called.
    pub fn add_cis_manager(mut self, cis: &'a CisManager<M, MAX_ASES>) -> Self {
        self.cis = Some(cis);
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
        if let (GattEvent::Write(write_event), Some(ascs)) = (&event, &self.ascs) {
            if write_event.handle() == ascs.ase_control_point().handle {
                let operation = write_event.value(ascs.ase_control_point());
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(_) => return true,
                }
                if let Ok(operation) = operation {
                    if let Some(cis) = self.cis {
                        cis.observe_operation(&self.server, ascs, &operation);
                    }
                    let notification = bap::drive_ase_control_point(&self.server, ascs, conn, &operation).await;
                    // The Control Point characteristic's write value (`AseControlPointOperation`)
                    // and its notified response (`AseControlPointNotification`) are different
                    // logical shapes multiplexed onto the same ATT value, so `notify_raw` is used
                    // here instead of the type-checked `notify`.
                    let _ = ascs
                        .ase_control_point()
                        .notify_raw(conn, notification.as_gatt(), false)
                        .await;
                } else {
                    #[cfg(feature = "defmt")]
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
            Some(Ok(())) => {
                if let Ok(reply) = event.accept() {
                    reply.send().await;
                }
            }
            Some(Err(err)) => {
                if let Ok(reply) = event.reject(err) {
                    reply.send().await;
                }
            }
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

    fn handle_read(&self, event: &ReadEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if let Some(res) = self.pacs.handle_read_event(event) {
            Some(res)
        } else if let Some(ascs) = &self.ascs {
            ascs.handle_read_event(event)
        } else {
            None
        }
    }

    fn handle_write(&self, event: &WriteEvent<'_, '_, P>) -> Option<Result<(), AttErrorCode>> {
        if let Some(res) = self.pacs.handle_write_event(event) {
            Some(res)
        } else if let Some(ascs) = &self.ascs {
            ascs.handle_write_event(event)
        } else {
            None
        }
    }
}
