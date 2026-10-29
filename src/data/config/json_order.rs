//! Key order for rewriting a hand-edited JSON document.
//!
//! `serde_json::Value` keeps object keys sorted (the `preserve_order` feature
//! is off crate-wide, and turning it on would reorder API schemas and other
//! output). A migration edits a `Value`, so writing it back would re-sort every
//! object in a version-controlled file. [`KeyOrder`] records the original
//! order and [`Ordered`] serializes the edited `Value` in it.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::Value;

/// The key order of one parsed JSON value.
#[derive(Debug, Default)]
pub struct KeyOrder {
    keys: Vec<(String, KeyOrder)>,
    items: Vec<KeyOrder>,
}

impl KeyOrder {
    /// The order of `content`'s keys, or none when it does not parse.
    pub fn of(content: &str) -> Self {
        serde_json::from_str(content).unwrap_or_default()
    }

    fn child(&self, key: &str) -> Option<&KeyOrder> {
        self.keys.iter().find(|(k, _)| k == key).map(|(_, o)| o)
    }
}

impl<'de> Deserialize<'de> for KeyOrder {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderVisitor;
        impl<'de> Visitor<'de> for OrderVisitor {
            type Value = KeyOrder;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_unit<E: de::Error>(self) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<KeyOrder, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(KeyOrder {
                    keys: Vec::new(),
                    items,
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<KeyOrder, A::Error> {
                let mut keys: Vec<(String, KeyOrder)> = Vec::new();
                while let Some((key, order)) = map.next_entry::<String, KeyOrder>()? {
                    // A duplicate key: the last value wins, as in `Value`.
                    keys.retain(|(k, _)| k != &key);
                    keys.push((key, order));
                }
                Ok(KeyOrder {
                    keys,
                    items: Vec::new(),
                })
            }
        }
        deserializer.deserialize_any(OrderVisitor)
    }
}

/// `value` serialized with each object's keys in `order`: keys the original
/// had keep their place, new keys take the slot of the first key that was
/// removed (so a renamed key stays where it was), or go last.
pub struct Ordered<'a> {
    pub value: &'a Value,
    pub order: Option<&'a KeyOrder>,
}

impl Serialize for Ordered<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.value {
            Value::Object(map) => {
                let original: &[(String, KeyOrder)] = self.order.map_or(&[], |o| &o.keys);
                let mut added: Vec<&String> = map
                    .keys()
                    .filter(|k| !original.iter().any(|(o, _)| o == *k))
                    .collect();
                let mut keys: Vec<&String> = Vec::with_capacity(map.len());
                for (key, _) in original {
                    match map.get_key_value(key) {
                        Some((present, _)) => keys.push(present),
                        None => keys.append(&mut added),
                    }
                }
                keys.append(&mut added);
                let mut out = serializer.serialize_map(Some(keys.len()))?;
                for key in keys {
                    out.serialize_entry(
                        key,
                        &Ordered {
                            value: &map[key],
                            order: self.order.and_then(|o| o.child(key)),
                        },
                    )?;
                }
                out.end()
            }
            Value::Array(items) => {
                let mut out = serializer.serialize_seq(Some(items.len()))?;
                for (index, item) in items.iter().enumerate() {
                    out.serialize_element(&Ordered {
                        value: item,
                        order: self.order.and_then(|o| o.items.get(index)),
                    })?;
                }
                out.end()
            }
            scalar => scalar.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(original: &str, edit: impl FnOnce(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(original).unwrap();
        edit(&mut value);
        let order = KeyOrder::of(original);
        serde_json::to_string(&Ordered {
            value: &value,
            order: Some(&order),
        })
        .unwrap()
    }

    #[test]
    fn original_order_survives_at_every_depth() {
        let out = rewrite(r#"{"z":1,"a":{"y":[{"q":1,"b":2}],"c":3}}"#, |_| {});
        assert_eq!(out, r#"{"z":1,"a":{"y":[{"q":1,"b":2}],"c":3}}"#);
    }

    #[test]
    fn a_new_key_takes_the_removed_keys_slot_and_extras_go_last() {
        let out = rewrite(r#"{"z":1,"old":2,"a":3}"#, |v| {
            let map = v.as_object_mut().unwrap();
            map.remove("old");
            map.insert("new".into(), Value::from(2));
        });
        assert_eq!(out, r#"{"z":1,"new":2,"a":3}"#);
        let out = rewrite(r#"{"z":1,"a":3}"#, |v| {
            v.as_object_mut()
                .unwrap()
                .insert("b".into(), Value::from(0));
        });
        assert_eq!(out, r#"{"z":1,"a":3,"b":0}"#);
    }

    #[test]
    fn without_an_order_the_value_serializes_as_usual() {
        let value: Value = serde_json::from_str(r#"{"z":1,"a":2}"#).unwrap();
        let out = serde_json::to_string(&Ordered {
            value: &value,
            order: None,
        })
        .unwrap();
        assert_eq!(out, serde_json::to_string(&value).unwrap());
    }
}
