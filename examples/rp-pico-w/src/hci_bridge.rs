//! Bridges cyw43's `BtDriver` (which implements bt-hci **0.9**'s typed `Transport`) to the
//! byte-level `bt_hci_transport::Transport` that bt-hci 0.10's `ExternalController` requires -
//! embassy hasn't migrated cyw43 off bt-hci 0.9 yet.
//!
//! Read side: the 0.9 driver copies the raw packet (indicator byte first) into a scratch buffer;
//! its typed 0.9 view is dropped and the bytes are re-parsed with the 0.10 parser. Write side:
//! 0.9's typed `write` can't take raw bytes, so [`RawCmd`]/[`RawAcl`]/[`RawIso`] implement 0.9's
//! `WriteHci` over an already-encoded packet body.

use core::cell::RefCell;
use core::future::Future;

use bt_hci_transport::{PacketKind, PacketToController, PacketToHost, Transport};
use cyw43::bluetooth::BtDriver;

/// cyw43's `BT_HCI_MTU` (1024) plus the indicator byte - its 0.9 `read` asserts the target
/// buffer is strictly larger than the packet.
const SCRATCH_SIZE: usize = 1025;

pub struct Cyw43Transport<'d> {
    driver: BtDriver<'d>,
    rx: RefCell<[u8; SCRATCH_SIZE]>,
    tx: RefCell<[u8; SCRATCH_SIZE]>,
}

#[derive(Debug)]
pub enum BridgeError {
    Driver(cyw43::bluetooth::Error),
    Hci,
}

impl core::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}

impl core::error::Error for BridgeError {}

// `ExternalController<T, N>: Controller` requires the transport error to absorb read-side HCI
// parse errors.
impl<E: embedded_io::Error> From<bt_hci_transport::ReadHciError<E>> for BridgeError {
    fn from(_: bt_hci_transport::ReadHciError<E>) -> Self {
        Self::Hci
    }
}

impl embedded_io::Error for BridgeError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            Self::Driver(e) => embedded_io::Error::kind(e),
            Self::Hci => embedded_io::ErrorKind::InvalidData,
        }
    }
}

impl embedded_io::ErrorType for Cyw43Transport<'_> {
    type Error = BridgeError;
}

macro_rules! raw_packet {
    ($name:ident, $kind:ident) => {
        struct $name<'a>(&'a [u8]);

        impl bt_hci09::WriteHci for $name<'_> {
            fn size(&self) -> usize {
                self.0.len()
            }

            fn write_hci<W: embedded_io::Write>(&self, mut writer: W) -> Result<(), W::Error> {
                writer.write_all(self.0)
            }

            async fn write_hci_async<W: embedded_io_async::Write>(&self, mut writer: W) -> Result<(), W::Error> {
                writer.write_all(self.0).await
            }
        }

        impl bt_hci09::HostToControllerPacket for $name<'_> {
            const KIND: bt_hci09::PacketKind = bt_hci09::PacketKind::$kind;
        }
    };
}
raw_packet!(RawCmd, Cmd);
raw_packet!(RawAcl, AclData);
raw_packet!(RawIso, IsoData);

impl<'d> Cyw43Transport<'d> {
    pub fn new(driver: BtDriver<'d>) -> Self {
        Self {
            driver,
            rx: RefCell::new([0; SCRATCH_SIZE]),
            tx: RefCell::new([0; SCRATCH_SIZE]),
        }
    }
}

impl Transport for Cyw43Transport<'_> {
    fn read<'a, P: PacketToHost<'a>>(&self, rx: &'a mut [u8]) -> impl Future<Output = Result<P, Self::Error>> {
        async {
            let scratch = &mut *self.rx.borrow_mut();
            {
                // Drop the typed 0.9 view immediately - only the raw bytes it copied into
                // `scratch` (indicator first) are used.
                let _ = bt_hci09::transport::Transport::read(&self.driver, &mut scratch[..])
                    .await
                    .map_err(BridgeError::Driver)?;
            }
            let mut reader: &[u8] = &scratch[..];
            let kind = PacketKind::read(&mut reader).map_err(|_| BridgeError::Hci)?;
            P::read_hci_async(kind, &mut reader, rx).await.map_err(|_| BridgeError::Hci)
        }
    }

    fn write<P: PacketToController>(&self, tx: &P) -> impl Future<Output = Result<(), Self::Error>> {
        async {
            let scratch = &mut *self.tx.borrow_mut();
            let size = tx.size();
            if size > scratch.len() {
                return Err(BridgeError::Hci);
            }
            {
                let mut writer = &mut scratch[..];
                tx.write_hci(&mut writer).map_err(|_| BridgeError::Hci)?;
            }
            let body = &scratch[..size];
            match P::KIND {
                PacketKind::Cmd => bt_hci09::transport::Transport::write(&self.driver, &RawCmd(body)).await,
                PacketKind::AclData => bt_hci09::transport::Transport::write(&self.driver, &RawAcl(body)).await,
                PacketKind::IsoData => bt_hci09::transport::Transport::write(&self.driver, &RawIso(body)).await,
                // Sync data / events only flow controller-to-host.
                PacketKind::SyncData | PacketKind::Event => return Err(BridgeError::Hci),
            }
            .map_err(BridgeError::Driver)
        }
    }
}
