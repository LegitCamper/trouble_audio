use super::ContextType;

pub enum Metadata {
    PreferredAudioContexts(ContextType),
    StreamingAudioContexts(ContextType),
    /// Title and/or summary of Audio Stream content: UTF-8 format
    ProgramInfo(&'static str),
    /// 3-byte, lower case language code as defined in ISO 639-3
    Language([u8; 3]),
    CCIDList,
}

impl Metadata {
    pub(crate) fn as_type(&self) -> u8 {
        match self {
            Metadata::PreferredAudioContexts(_) => 1,
            Metadata::StreamingAudioContexts(_) => 2,
            Metadata::ProgramInfo(_) => 3,
        }
    }
}
