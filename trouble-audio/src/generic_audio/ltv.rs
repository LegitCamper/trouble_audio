//! Generic LTV (Length, Type, Value) encoding shared by Codec_Specific_Capabilities,
//! Codec_Specific_Configuration and Metadata, as defined by the Generic Audio assigned numbers.
//!
//! Wire format of a single entry: `Length (1 octet) | Type (1 octet) | Value (Length - 1 octets)`,
//! where `Length` covers the Type octet plus the Value octets. A characteristic value is a
//! back-to-back sequence of these entries.

use alloc::vec::Vec;
use trouble_host::types::gatt_traits::FromGattError;

/// A single value that can appear in an LTV structure list.
pub trait Ltv: Sized {
    /// The `Type` octet for this variant, as assigned by the Generic Audio spec.
    fn ltv_type(&self) -> u8;
    /// Appends this variant's `Value` octets (without the Length/Type header) to `out`.
    fn encode_value(&self, out: &mut Vec<u8>);
    /// Reconstructs a variant from its `Type` octet and `Value` octets.
    fn decode(ty: u8, value: &[u8]) -> Result<Self, FromGattError>;
}

/// Encodes a list of LTV entries back-to-back.
pub fn encode_list<T: Ltv>(items: &[T]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        let len_pos = out.len();
        out.push(0); // Length placeholder, patched below
        out.push(item.ltv_type());
        item.encode_value(&mut out);
        let len = out.len() - len_pos - 1;
        out[len_pos] = len as u8;
    }
    out
}

/// Decodes a back-to-back sequence of LTV entries.
pub fn decode_list<T: Ltv>(mut data: &[u8]) -> Result<Vec<T>, FromGattError> {
    let mut items = Vec::new();
    while !data.is_empty() {
        let len = data[0] as usize;
        if len == 0 || data.len() < 1 + len {
            return Err(FromGattError::InvalidLength);
        }
        let ty = data[1];
        let value = &data[2..1 + len];
        items.push(T::decode(ty, value)?);
        data = &data[1 + len..];
    }
    Ok(items)
}
