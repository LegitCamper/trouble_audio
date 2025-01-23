use super::ContextType;
use crate::ContentControlID;
use paste::paste;
use quote::format_ident;

pub enum Metadata {
    PreferredAudioContexts(ContextType),
    StreamingAudioContexts(ContextType),
    /// Title and/or summary of Audio Stream content: UTF-8 format
    ProgramInfo(&'static str),
    /// 3-byte, lower case language code as defined in ISO 639-3
    Language([u8; 3]),
    CCIDList(&'static [ContentControlID]),
    ParentalRating(ParentalRating),
    ProgramInfoURI(&'static str),
    ExtendedMetadata(), // TODO
    VenderSpecific(),
}

impl Metadata {
    pub(crate) fn as_type(&self) -> u8 {
        match self {
            Metadata::PreferredAudioContexts(_) => 1,
            Metadata::StreamingAudioContexts(_) => 2,
            Metadata::ProgramInfo(_) => 3,
            Metadata::Language(_) => 4,
            Metadata::CCIDList(_) => 5,
            Metadata::ParentalRating(_) => 6,
            Metadata::ProgramInfoURI(_) => 7,
            Metadata::ExtendedMetadata() => 8,
        }
    }
}

pub enum ParentalRating {
    NoRating = 0x00,     // No rating
    AnyAge = 0x01,       // Recommended for listeners of any age
    Age5orOlder = 0x02,  // Recommended for listeners of age 5 or older
    Age6orOlder = 0x03,  // Recommended for listeners of age 6 or older
    Age7orOlder = 0x04,  // Recommended for listeners of age 7 or older
    Age8orOlder = 0x05,  // Recommended for listeners of age 8 or older
    Age9orOlder = 0x06,  // Recommended for listeners of age 9 or older
    Age10orOlder = 0x07, // Recommended for listeners of age 10 or older
    Age11orOlder = 0x08, // Recommended for listeners of age 11 or older
    Age12orOlder = 0x09, // Recommended for listeners of age 12 or older
    Age13orOlder = 0x0A, // Recommended for listeners of age 13 or older
    Age14orOlder = 0x0B, // Recommended for listeners of age 14 or older
    Age15orOlder = 0x0C, // Recommended for listeners of age 15 or older
    Age16orOlder = 0x0D, // Recommended for listeners of age 16 or older
    Age17orOlder = 0x0E, // Recommended for listeners of age 17 or older
    Age18orOlder = 0x0F, // Recommended for listeners of age 18 or older
    Undefined,
}

impl Into<u8> for ParentalRating {
    fn into(self) -> u8 {
        self as u8
    }
}

impl From<u8> for ParentalRating {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::NoRating,
            0x01 => Self::AnyAge,
            0x02 => Self::Age5orOlder,
            0x03 => Self::Age6orOlder,
            0x04 => Self::Age7orOlder,
            0x05 => Self::Age8orOlder,
            0x06 => Self::Age9orOlder,
            0x07 => Self::Age10orOlder,
            0x08 => Self::Age11orOlder,
            0x09 => Self::Age12orOlder,
            0x0A => Self::Age13orOlder,
            0x0B => Self::Age14orOlder,
            0x0C => Self::Age15orOlder,
            0x0D => Self::Age16orOlder,
            0x0E => Self::Age17orOlder,
            0x0F => Self::Age18orOlder,
            _ => Self::Undefined,
        }
    }
}

pub enum ExtendedMetadata {}

impl ExtendedMetadata {
    pub(crate) fn as_type(&self) -> u8 {
        // match self {}
        0
    }
}

pub struct VenderSpecific {}
