#![cfg_attr(not(test), no_std, no_main)]
#![feature(generic_const_exprs)]

//! LE Audio data structures are variable-length (PAC records, codec-specific
//! configuration, metadata, ...) to a degree that doesn't fit `heapless`'s
//! fixed-capacity collections well. This crate uses `alloc` instead, so the
//! consuming binary must install a global allocator.
extern crate alloc;

// Several `AsGatt` impls reinterpret native integers as little-endian wire bytes in place.
#[cfg(target_endian = "big")]
compile_error!("trouble_audio assumes a little-endian target");

#[macro_use]
mod fmt;

#[cfg(test)]
mod test_alloc;

pub mod aics;
#[allow(dead_code)]
pub mod ascs;
pub mod ase_client;
pub mod bap;
pub mod bass;
pub mod cig;
pub mod cis;
pub mod csis;
mod server;
pub use server::*;
mod client;
pub use client::*;
pub mod generic_audio;
pub mod gmas;
pub mod has;
pub mod iso_tx;
pub mod lc3;
pub mod mcs;
pub mod mics;
pub mod ots;
pub mod pacs;
pub mod tbs;
pub mod tmas;
pub mod vcs;
pub mod vocs;

/// Re-exports the types most commonly needed to build an LE Audio peripheral/central, so callers
/// don't need to remember which of this crate's 14+ per-service modules each one lives in.
/// Deliberately excludes optional-service types that collide by name across services (e.g.
/// `mics::Mute` vs `vcs::Mute`) - import those from their own module.
pub mod prelude {
    pub use bt_hci::cmd::le::{LeAcceptCisRequest, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetupIsoDataPath};

    pub use crate::{
        ascs::{Ase, AseType},
        cis::CisManager,
        generic_audio::{
            AudioLocation, CodecSpecificCapabilities, ContextType, OctetsPerCodecFrame,
            SamplingFrequency, SupportedFrameDurations, SupportedSamplingFrequencies,
        },
        pacs::{AudioContexts, PACRecord, PAC},
        CodecId, Server, ServerBuilder,
    };
}

pub type ContentControlID = u8;

/// Why discovering a mandatory LE Audio service on a peer failed. A peer's GATT table is
/// untrusted input, so discovery is fallible rather than panicking.
#[derive(Debug)]
pub enum DiscoveryError<E> {
    /// The underlying GATT request failed.
    Host(trouble_host::BleHostError<E>),
    /// The peer doesn't expose the service at all.
    ServiceNotFound,
    /// The service exists but is missing a characteristic its spec makes mandatory.
    CharacteristicNotFound,
}

impl<E> From<trouble_host::BleHostError<E>> for DiscoveryError<E> {
    fn from(e: trouble_host::BleHostError<E>) -> Self {
        Self::Host(e)
    }
}

/// Codec_ID: identifies a codec, either a standard Bluetooth SIG codec (`coding_format` alone,
/// with `company_id`/`vendor_specific_codec_id` both zero) or a vendor-specific one. 5 octets on
/// the wire: Coding_Format (1) | Company_ID (2, LE) | Vendor_Specific_Codec_ID (2, LE).
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecId {
    pub coding_format: CodingFormat,
    pub company_id: u16,
    pub vendor_specific_codec_id: u16,
}

impl CodecId {
    pub const SIZE: usize = 5;

    /// Builds a standard (non-vendor-specific) Codec_ID for the given coding format.
    pub const fn new(coding_format: CodingFormat) -> Self {
        Self {
            coding_format,
            company_id: 0,
            vendor_specific_codec_id: 0,
        }
    }

    /// Encodes this Codec_ID into its 5-octet wire representation.
    pub fn to_le_bytes(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0] = u8::from(self.coding_format);
        out[1..3].copy_from_slice(&self.company_id.to_le_bytes());
        out[3..5].copy_from_slice(&self.vendor_specific_codec_id.to_le_bytes());
        out
    }

    /// Decodes a Codec_ID from its 5-octet wire representation.
    pub fn from_le_bytes(bytes: [u8; Self::SIZE]) -> Self {
        Self {
            coding_format: CodingFormat::from(bytes[0]),
            company_id: u16::from_le_bytes([bytes[1], bytes[2]]),
            vendor_specific_codec_id: u16::from_le_bytes([bytes[3], bytes[4]]),
        }
    }
}

impl Default for CodecId {
    fn default() -> Self {
        Self::new(CodingFormat::LC3)
    }
}

/// Coding_Format, as assigned by the Bluetooth SIG "Codec ID" assigned numbers table.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingFormat {
    ULaw,
    ALaw,
    Cvsd,
    Transparent,
    LinearPcm,
    Msbc,
    LC3,
    G729A,
    VendorSpecific,
    Other(u8),
}

impl From<u8> for CodingFormat {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::ULaw,
            0x01 => Self::ALaw,
            0x02 => Self::Cvsd,
            0x03 => Self::Transparent,
            0x04 => Self::LinearPcm,
            0x05 => Self::Msbc,
            0x06 => Self::LC3,
            0x07 => Self::G729A,
            0xFF => Self::VendorSpecific,
            other => Self::Other(other),
        }
    }
}

impl From<CodingFormat> for u8 {
    fn from(value: CodingFormat) -> Self {
        match value {
            CodingFormat::ULaw => 0x00,
            CodingFormat::ALaw => 0x01,
            CodingFormat::Cvsd => 0x02,
            CodingFormat::Transparent => 0x03,
            CodingFormat::LinearPcm => 0x04,
            CodingFormat::Msbc => 0x05,
            CodingFormat::LC3 => 0x06,
            CodingFormat::G729A => 0x07,
            CodingFormat::VendorSpecific => 0xFF,
            CodingFormat::Other(v) => v,
        }
    }
}
