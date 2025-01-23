use super::ContextType;
use crate::ContentControlID;

#[derive(Debug, Clone)]
#[repr(u8)]
pub enum Metadata {
    PreferredAudioContexts(ContextType) = 1,
    StreamingAudioContexts(ContextType) = 2,
    /// Title and/or summary of Audio Stream content: UTF-8 format
    ProgramInfo(&'static str) = 3,
    /// 3-byte, lower case language code as defined in ISO 639-3
    Language([u8; 3]) = 4,
    CCIDList(&'static [ContentControlID]) = 5,
    ParentalRating(ParentalRating) = 6,
    ProgramInfoURI(&'static str) = 7,
    ExtendedMetadata() = 0xFE, // TODO
    VenderSpecific(VenderSpecific) = 0xFF,
    AudioActiveState(AudioActiveState) = 8,
    BroadcastAudioImmediateRenderingFlag = 9,
    AssistedListeningStream(AssistedListeningStream) = 10,
    BroadcastName(&'static str) = 11,
}

#[derive(Debug, Clone)]
#[repr(u8)]
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
}

pub enum ExtendedMetadata {}

#[derive(Debug, Clone)]
pub struct VenderSpecific {
    company_id: Option<u8>,
    vender_specific_metadata: &'static [u8],
}

#[derive(Debug, Clone)]
#[repr(u8)]
pub enum AudioActiveState {
    NotBeingTransmitted = 0,
    BeingTransmitted = 1,
}

#[derive(Debug, Clone)]
#[repr(u8)]
pub enum AssistedListeningStream {
    UnspecifiedAudioEnhancement = 0,
}
