use primitive_types::{H160, H256, H512, U128, U256, U512};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt::{self, LowerHex};
use std::marker::PhantomData;
use std::str::FromStr;
#[derive(Debug, Hash, Clone, Eq, PartialEq)]
pub struct Hex<T>(pub T);
#[derive(Debug, Clone)]
pub struct Bytes(pub Vec<u8>);

type Error = (); // TODO: Rewrite errors.

fn format_hex_trimmed<T: LowerHex>(val: &T) -> String {
    let hex_str = format!("{:x}", val);
    format!("0x{}", hex_str.trim_start_matches('0'))
}

impl<T: FormatHex> Hex<T> {
    pub fn from_hex(data: &str) -> Result<Self, Error> {
        if &data[0..2] != "0x" {
            return Err(());
        }
        T::from_hex(&data[2..]).map(Hex)
    }
}

pub trait FormatHex {
    fn format_hex(&self) -> String;
    fn from_hex(data: &str) -> Result<Self, Error>
    where
        Self: Sized;
}

impl FormatHex for usize {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }

    fn from_hex(data: &str) -> Result<Self, Error> {
        Self::from_str_radix(data, 16).map_err(|_| ())
    }
}

impl FormatHex for u8 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(data: &str) -> Result<Self, Error> {
        Self::from_str_radix(data, 16).map_err(|_| ())
    }
}

impl FormatHex for u16 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(data: &str) -> Result<Self, Error> {
        Self::from_str_radix(data, 16).map_err(|_| ())
    }
}
impl FormatHex for u32 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(data: &str) -> Result<Self, Error> {
        Self::from_str_radix(data, 16).map_err(|_| ())
    }
}

impl FormatHex for u64 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(data: &str) -> Result<Self, Error> {
        Self::from_str_radix(data, 16).map_err(|_| ())
    }
}

impl FormatHex for U128 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(s: &str) -> Result<Self, Error> {
        FromStr::from_str(&s).map_err(|_| ())
    }
}

impl FormatHex for U256 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(s: &str) -> Result<Self, Error> {
        FromStr::from_str(&s).map_err(|_| ())
    }
}

impl FormatHex for U512 {
    fn format_hex(&self) -> String {
        format_hex_trimmed(self)
    }
    fn from_hex(s: &str) -> Result<Self, Error> {
        FromStr::from_str(&s).map_err(|_| ())
    }
}

impl FormatHex for H512 {
    fn format_hex(&self) -> String {
        format!("0x{:x}", self)
    }
    fn from_hex(s: &str) -> Result<Self, Error> {
        FromStr::from_str(&s).map_err(|_| ())
    }
}

impl FormatHex for H256 {
    fn format_hex(&self) -> String {
        format!("0x{:x}", self)
    }
    fn from_hex(s: &str) -> Result<Self, Error> {
        FromStr::from_str(&s).map_err(|_| ())
    }
}

impl FormatHex for H160 {
    fn format_hex(&self) -> String {
        format!("0x{:x}", self)
    }
    fn from_hex(s: &str) -> Result<Self, Error> {
        FromStr::from_str(&s).map_err(|_| ())
    }
}

impl<T: FormatHex> Serialize for Hex<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = self.0.format_hex();
        if &value == "0x" {
            serializer.serialize_str("0x0")
        } else {
            serializer.serialize_str(&value)
        }
    }
}

impl Serialize for Bytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", &hex::encode(&self.0)))
    }
}

struct HexVisitor<T> {
    _marker: PhantomData<T>,
}

impl<'de, T: FormatHex> de::Visitor<'de> for HexVisitor<T> {
    type Value = Hex<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Must be a valid hex string")
    }

    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match T::from_hex(&s[2..]) {
            Ok(d) if &s[..2] == "0x" => Ok(Hex(d)),
            _ => Err(de::Error::invalid_value(de::Unexpected::Str(s), &self)),
        }
    }
}

struct BytesVisitor;

impl<'de> de::Visitor<'de> for BytesVisitor {
    type Value = Bytes;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Must be a valid hex string")
    }

    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match hex::decode(&s[2..]) {
            Ok(d) if &s[..2] == "0x" => Ok(Bytes(d)),
            _ => Err(de::Error::invalid_value(de::Unexpected::Str(s), &self)),
        }
    }
}

impl<'de, T: FormatHex> Deserialize<'de> for Hex<T> {
    fn deserialize<D>(deserializer: D) -> Result<Hex<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(HexVisitor {
            _marker: PhantomData,
        })
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(BytesVisitor)
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(b: Vec<u8>) -> Self {
        Bytes(b)
    }
}
impl<T: FormatHex + FromStr> From<T> for Hex<T> {
    fn from(b: T) -> Self {
        Hex(b)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use primitive_types::U256;
    use serde_json;

    //TODO: WTF? Is Ethereum hex remove in-byte zero? Why it expect 0x1 not 0x01?
    #[test]
    fn hex_single_digit() {
        assert_eq!("\"0x1\"", serde_json::to_string(&Hex(U256::one())).unwrap());
    }

    #[test]
    fn hex_zero() {
        assert_eq!(
            "\"0x0\"",
            serde_json::to_string(&Hex(U256::zero())).unwrap()
        );
    }

    #[test]
    fn hex_deserialize() {
        assert_eq!(
            0x7bb9369dcbaec019,
            serde_json::from_str::<Hex<u64>>("\"0x7bb9369dcbaec019\"")
                .unwrap()
                .0
        );
    }

    #[test]
    fn bytes_single_digit() {
        assert_eq!("\"0x01\"", serde_json::to_string(&Bytes(vec![1])).unwrap());
    }
}