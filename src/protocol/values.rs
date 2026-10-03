//! Tagged values: game rules, ability values and pack settings.

use super::consts::PackSettingType;
use super::io::{Codec, Error, Reader, Result, Writer, length};

/// A game rule and its value (gophertunnel `GameRule`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GameRule {
    /// The name of the game rule.
    pub name: String,
    /// Whether players may change the rule.
    pub can_be_modified_by_player: bool,
    /// The value of the rule.
    pub value: GameRuleValue,
}

/// A game rule value; the variant is the wire type.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum GameRuleValue {
    /// Type 0: no value (Go `nil`).
    #[default]
    Empty,
    /// Type 1: a bool.
    Bool(bool),
    /// Type 2: a little-endian uint32.
    Uint32(u32),
    /// Type 3: a little-endian float32.
    Float32(f32),
}

/// An ability value: a type byte, a bool and a float32, one of which is meaningful.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AbilityValue {
    /// Type 1: the bool.
    Bool(bool),
    /// Type 2: the float32.
    Float32(f32),
}

impl Default for AbilityValue {
    fn default() -> Self {
        Self::Bool(false)
    }
}

/// A resource pack setting (gophertunnel `PackSetting`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PackSetting {
    /// The name of the setting.
    pub name: String,
    /// The value of the setting.
    pub value: PackSettingValue,
}

/// A pack setting value; the variant is the wire type.
#[derive(Debug, Clone, PartialEq)]
pub enum PackSettingValue {
    /// Type 0: a little-endian float32.
    Float32(f32),
    /// Type 1: a bool.
    Bool(bool),
    /// Type 2: a string.
    String(String),
    /// Type 3: a list of strings.
    StringList(Vec<String>),
}

impl Default for PackSettingValue {
    fn default() -> Self {
        Self::Float32(0.0)
    }
}

impl Reader<'_> {
    /// `GameRule`
    pub(crate) fn game_rule(&mut self) -> Result<GameRule> {
        let name = self.string()?;
        let can_be_modified_by_player = self.bool()?;
        let value = match self.varuint32()? {
            0 => GameRuleValue::Empty,
            1 => GameRuleValue::Bool(self.bool()?),
            2 => GameRuleValue::Uint32(self.uint32()?),
            3 => GameRuleValue::Float32(self.float32()?),
            t => {
                return Err(Error::UnknownEnum {
                    name: "game rule type",
                    value: t.into(),
                });
            }
        };
        Ok(GameRule {
            name,
            can_be_modified_by_player,
            value,
        })
    }

    /// `AbilityValue`
    pub(crate) fn ability_value(&mut self) -> Result<AbilityValue> {
        let value_type = self.uint8()?;
        let bool_value = self.bool()?;
        let float_value = self.float32()?;
        match value_type {
            1 => Ok(AbilityValue::Bool(bool_value)),
            2 => Ok(AbilityValue::Float32(float_value)),
            t => Err(Error::UnknownEnum {
                name: "ability value type",
                value: t.into(),
            }),
        }
    }

    /// `PackSetting`
    pub(crate) fn pack_setting(&mut self) -> Result<PackSetting> {
        let name = self.string()?;
        let value = match PackSettingType::try_from(self.varuint32()?)? {
            PackSettingType::Float => PackSettingValue::Float32(self.float32()?),
            PackSettingType::Bool => PackSettingValue::Bool(self.bool()?),
            PackSettingType::String => PackSettingValue::String(self.string()?),
            PackSettingType::StringList => {
                let count = self.varuint32()?;
                PackSettingValue::StringList(self.list(count.into(), Reader::string)?)
            }
        };
        Ok(PackSetting { name, value })
    }
}

impl Writer {
    /// `GameRule`
    pub(crate) fn game_rule(&mut self, x: &GameRule) -> Result<()> {
        self.string(&x.name)?;
        self.bool(x.can_be_modified_by_player);
        match x.value {
            GameRuleValue::Empty => self.varuint32(0),
            GameRuleValue::Bool(v) => {
                self.varuint32(1);
                self.bool(v);
            }
            GameRuleValue::Uint32(v) => {
                self.varuint32(2);
                self.uint32(v);
            }
            GameRuleValue::Float32(v) => {
                self.varuint32(3);
                self.float32(v);
            }
        }
        Ok(())
    }

    /// `AbilityValue`
    pub(crate) fn ability_value(&mut self, x: AbilityValue) {
        let (value_type, bool_value, float_value) = match x {
            AbilityValue::Bool(v) => (1, v, 0.0),
            AbilityValue::Float32(v) => (2, false, v),
        };
        self.uint8(value_type);
        self.bool(bool_value);
        self.float32(float_value);
    }

    /// `PackSetting`
    pub(crate) fn pack_setting(&mut self, x: &PackSetting) -> Result<()> {
        self.string(&x.name)?;
        match &x.value {
            PackSettingValue::Float32(v) => {
                self.varuint32(PackSettingType::Float.into());
                self.float32(*v);
            }
            PackSettingValue::Bool(v) => {
                self.varuint32(PackSettingType::Bool.into());
                self.bool(*v);
            }
            PackSettingValue::String(v) => {
                self.varuint32(PackSettingType::String.into());
                self.string(v)?;
            }
            PackSettingValue::StringList(v) => {
                self.varuint32(PackSettingType::StringList.into());
                self.varuint32(length(v.len())?);
                for s in v {
                    self.string(s)?;
                }
            }
        }
        Ok(())
    }
}

impl Codec for GameRule {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.game_rule()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.game_rule(self)
    }
}

impl Codec for AbilityValue {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.ability_value()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.ability_value(*self);
        Ok(())
    }
}

impl Codec for PackSetting {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.pack_setting()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.pack_setting(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fuzz_round_trip;

    fn encode<T: Codec>(x: &T) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        x.encode(&mut w).unwrap();
        w.into_inner()
    }

    fn decode<T: Codec>(data: &[u8]) -> Result<T> {
        T::decode(&mut Reader::new(data, 1 << 20, 0))
    }

    #[test]
    fn game_rules_match_gophertunnel_bytes() {
        let x = GameRule {
            name: "a".to_owned(),
            can_be_modified_by_player: true,
            value: GameRuleValue::Uint32(5),
        };
        assert_eq!(encode(&x), [1, b'a', 1, 2, 5, 0, 0, 0]);
        for value in [
            GameRuleValue::Empty,
            GameRuleValue::Bool(true),
            GameRuleValue::Uint32(u32::MAX),
            GameRuleValue::Float32(-1.5),
        ] {
            let x = GameRule { value, ..x.clone() };
            assert_eq!(decode(&encode(&x)), Ok(x));
        }
        assert!(decode::<GameRule>(&[0, 0, 4]).is_err());
        assert_eq!(
            decode::<GameRule>(&[0, 0, 1, 2]),
            Err(Error::InvalidBool(2))
        );
    }

    #[test]
    fn ability_values_match_gophertunnel_bytes() {
        assert_eq!(encode(&AbilityValue::Float32(0.5)), [2, 0, 0, 0, 0, 0x3f]);
        assert_eq!(encode(&AbilityValue::Bool(true)), [1, 1, 0, 0, 0, 0]);
        for x in [AbilityValue::Bool(true), AbilityValue::Float32(2.0)] {
            assert_eq!(decode(&encode(&x)), Ok(x));
        }
        assert!(decode::<AbilityValue>(&[3, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn pack_settings_round_trip() {
        for value in [
            PackSettingValue::Float32(1.0),
            PackSettingValue::Bool(false),
            PackSettingValue::String("s".to_owned()),
            PackSettingValue::StringList(vec!["a".to_owned(), "bc".to_owned()]),
        ] {
            let x = PackSetting {
                name: "n".to_owned(),
                value,
            };
            assert_eq!(decode(&encode(&x)), Ok(x));
        }
        assert!(decode::<PackSetting>(&[0, 4]).is_err());
        let mut w = Writer::new(vec![0, 3], 0);
        w.varuint32(1025);
        let mut bytes = w.into_inner();
        bytes.extend([0; 1100]);
        assert_eq!(decode::<PackSetting>(&bytes), Err(Error::Size));
    }

    #[test]
    fn mutated_input_never_panics() {
        let rule = GameRule {
            name: "rule".to_owned(),
            can_be_modified_by_player: false,
            value: GameRuleValue::Float32(1.0),
        };
        let setting = PackSetting {
            name: "n".to_owned(),
            value: PackSettingValue::StringList(vec!["a".to_owned(); 3]),
        };
        let write = |w: &mut Writer, x: &GameRule| x.encode(w);
        assert!(fuzz_round_trip(&encode(&rule), 0, GameRule::decode, write) > 0);
        let write = |w: &mut Writer, x: &PackSetting| x.encode(w);
        assert!(fuzz_round_trip(&encode(&setting), 0, PackSetting::decode, write) > 0);
        let write = |w: &mut Writer, x: &AbilityValue| x.encode(w);
        let seed = encode(&AbilityValue::Bool(true));
        assert!(fuzz_round_trip(&seed, 0, AbilityValue::decode, write) > 0);
    }
}
