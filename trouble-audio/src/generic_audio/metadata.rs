use alloc::{string::String, vec::Vec};

use super::{ContextType, Ltv};
use crate::ContentControlID;
use trouble_host::types::gatt_traits::FromGattError;

fn string_from_utf8(value: &[u8]) -> Result<String, FromGattError> {
    core::str::from_utf8(value)
        .map(String::from)
        .map_err(|_| FromGattError::InvalidCharacter)
}

/// A single entry of the Metadata LTV structure.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
#[repr(u8)]
pub enum Metadata {
    PreferredAudioContexts(ContextType) = 1,
    StreamingAudioContexts(ContextType) = 2,
    /// Title and/or summary of Audio Stream content: UTF-8 format
    ProgramInfo(String) = 3,
    /// 3-byte, lower case language code as defined in ISO 639-3
    Language([u8; 3]) = 4,
    CCIDList(Vec<ContentControlID>) = 5,
    ParentalRating(ParentalRating) = 6,
    ProgramInfoURI(String) = 7,
    /// Raw, opaque Extended_Metadata LTV structures (not yet decoded further).
    ExtendedMetadata(Vec<u8>) = 0xFE,
    VenderSpecific(VenderSpecific) = 0xFF,
    AudioActiveState(AudioActiveState) = 8,
    BroadcastAudioImmediateRenderingFlag = 9,
    AssistedListeningStream(AssistedListeningStream) = 10,
    BroadcastName(String) = 11,
}

impl Ltv for Metadata {
    fn ltv_type(&self) -> u8 {
        match self {
            Self::PreferredAudioContexts(_) => 1,
            Self::StreamingAudioContexts(_) => 2,
            Self::ProgramInfo(_) => 3,
            Self::Language(_) => 4,
            Self::CCIDList(_) => 5,
            Self::ParentalRating(_) => 6,
            Self::ProgramInfoURI(_) => 7,
            Self::AudioActiveState(_) => 8,
            Self::BroadcastAudioImmediateRenderingFlag => 9,
            Self::AssistedListeningStream(_) => 10,
            Self::BroadcastName(_) => 11,
            Self::ExtendedMetadata(_) => 0xFE,
            Self::VenderSpecific(_) => 0xFF,
        }
    }

    fn encode_value(&self, out: &mut Vec<u8>) {
        match self {
            Self::PreferredAudioContexts(v) => out.extend_from_slice(&v.bits().to_le_bytes()),
            Self::StreamingAudioContexts(v) => out.extend_from_slice(&v.bits().to_le_bytes()),
            Self::ProgramInfo(v) => out.extend_from_slice(v.as_bytes()),
            Self::Language(v) => out.extend_from_slice(v),
            Self::CCIDList(v) => out.extend_from_slice(v),
            Self::ParentalRating(v) => out.push(*v as u8),
            Self::ProgramInfoURI(v) => out.extend_from_slice(v.as_bytes()),
            Self::AudioActiveState(v) => out.push(*v as u8),
            Self::BroadcastAudioImmediateRenderingFlag => {}
            Self::AssistedListeningStream(v) => out.push(*v as u8),
            Self::BroadcastName(v) => out.extend_from_slice(v.as_bytes()),
            Self::ExtendedMetadata(v) => out.extend_from_slice(v),
            Self::VenderSpecific(v) => v.encode_value(out),
        }
    }

    fn decode(ty: u8, value: &[u8]) -> Result<Self, FromGattError> {
        match ty {
            1 => Ok(Self::PreferredAudioContexts(ContextType::from_bits_truncate(
                u16::from_le_bytes(value.try_into().map_err(|_| FromGattError::InvalidLength)?),
            ))),
            2 => Ok(Self::StreamingAudioContexts(ContextType::from_bits_truncate(
                u16::from_le_bytes(value.try_into().map_err(|_| FromGattError::InvalidLength)?),
            ))),
            3 => Ok(Self::ProgramInfo(string_from_utf8(value)?)),
            4 => Ok(Self::Language(value.try_into().map_err(|_| FromGattError::InvalidLength)?)),
            5 => Ok(Self::CCIDList(Vec::from(value))),
            6 => Ok(Self::ParentalRating(ParentalRating::try_from(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )?)),
            7 => Ok(Self::ProgramInfoURI(string_from_utf8(value)?)),
            8 => Ok(Self::AudioActiveState(AudioActiveState::try_from(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )?)),
            9 => Ok(Self::BroadcastAudioImmediateRenderingFlag),
            10 => Ok(Self::AssistedListeningStream(AssistedListeningStream::try_from(
                *value.first().ok_or(FromGattError::InvalidLength)?,
            )?)),
            11 => Ok(Self::BroadcastName(string_from_utf8(value)?)),
            0xFE => Ok(Self::ExtendedMetadata(Vec::from(value))),
            0xFF => Ok(Self::VenderSpecific(VenderSpecific::decode_value(value)?)),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ParentalRating {
    NoRating = 0x00,
    AnyAge = 0x01,
    Age5orOlder = 0x02,
    Age6orOlder = 0x03,
    Age7orOlder = 0x04,
    Age8orOlder = 0x05,
    Age9orOlder = 0x06,
    Age10orOlder = 0x07,
    Age11orOlder = 0x08,
    Age12orOlder = 0x09,
    Age13orOlder = 0x0A,
    Age14orOlder = 0x0B,
    Age15orOlder = 0x0C,
    Age16orOlder = 0x0D,
    Age17orOlder = 0x0E,
    Age18orOlder = 0x0F,
}

impl TryFrom<u8> for ParentalRating {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::NoRating),
            0x01 => Ok(Self::AnyAge),
            0x02 => Ok(Self::Age5orOlder),
            0x03 => Ok(Self::Age6orOlder),
            0x04 => Ok(Self::Age7orOlder),
            0x05 => Ok(Self::Age8orOlder),
            0x06 => Ok(Self::Age9orOlder),
            0x07 => Ok(Self::Age10orOlder),
            0x08 => Ok(Self::Age11orOlder),
            0x09 => Ok(Self::Age12orOlder),
            0x0A => Ok(Self::Age13orOlder),
            0x0B => Ok(Self::Age14orOlder),
            0x0C => Ok(Self::Age15orOlder),
            0x0D => Ok(Self::Age16orOlder),
            0x0E => Ok(Self::Age17orOlder),
            0x0F => Ok(Self::Age18orOlder),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

/// Vendor_Specific metadata: a SIG-assigned Company_ID followed by vendor-defined bytes.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq)]
pub struct VenderSpecific {
    pub company_id: u16,
    pub vender_specific_metadata: Vec<u8>,
}

impl VenderSpecific {
    fn encode_value(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.company_id.to_le_bytes());
        out.extend_from_slice(&self.vender_specific_metadata);
    }

    fn decode_value(value: &[u8]) -> Result<Self, FromGattError> {
        if value.len() < 2 {
            return Err(FromGattError::InvalidLength);
        }
        let company_id = u16::from_le_bytes([value[0], value[1]]);
        Ok(Self {
            company_id,
            vender_specific_metadata: Vec::from(&value[2..]),
        })
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioActiveState {
    NotBeingTransmitted = 0,
    BeingTransmitted = 1,
}

impl TryFrom<u8> for AudioActiveState {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NotBeingTransmitted),
            1 => Ok(Self::BeingTransmitted),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AssistedListeningStream {
    UnspecifiedAudioEnhancement = 0,
}

impl TryFrom<u8> for AssistedListeningStream {
    type Error = FromGattError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::UnspecifiedAudioEnhancement),
            _ => Err(FromGattError::InvalidLength),
        }
    }
}
