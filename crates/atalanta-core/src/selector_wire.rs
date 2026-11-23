//! The one JSON form a [`Selector`] takes outside the process (review 02
//! M5): the DSL's flat map, `{"text": "Pay", "index": 1}` or `{"role":
//! "button", "name": "Pay", "below": {"id": "total"}}`, as a suite writes
//! it and [`crate::parser::selector_from_value`] reads it.
//!
//! `Selector`'s derived serde form (`{"index": {"selector": …, "index":
//! 1}}`) is an implementation detail: it changes whenever a variant does.
//! Persisted and wire types (`StepTrace.selector` in `steps.json`, the
//! inspector API) use this module through `#[serde(with = …)]` instead.
//! Reading also takes the derived form, so a trace written before this
//! module still loads, except a bare state filter (`{"checked": true}`),
//! which never finds anything on its own and the DSL refuses.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use velocity_common::Selector;

/// `s` as the DSL's flat map.
pub fn to_value(s: &Selector) -> Value {
    let mut m = Map::new();
    fill(&mut m, s);
    Value::Object(m)
}

fn fill(m: &mut Map<String, Value>, s: &Selector) {
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    match s {
        Selector::Id(v) => put("id", v.as_str().into()),
        Selector::Text(v) => put("text", v.as_str().into()),
        Selector::TextContains(v) => put("textContains", v.as_str().into()),
        Selector::TextMatches(v) => put("textMatches", v.as_str().into()),
        Selector::IdMatches(v) => put("idMatches", v.as_str().into()),
        Selector::AccessibilityId(v) => put("accessibilityId", v.as_str().into()),
        Selector::ClassName(v) => put("className", v.as_str().into()),
        Selector::Css(v) => put("css", v.as_str().into()),
        Selector::XPath(v) => put("xpath", v.as_str().into()),
        Selector::Role(v) => put("role", v.as_str().into()),
        Selector::Name(v) => put("name", v.as_str().into()),
        Selector::Checked(b) => put("checked", (*b).into()),
        Selector::Focused(b) => put("focused", (*b).into()),
        Selector::Selected(b) => put("selected", (*b).into()),
        Selector::Enabled(b) => put("enabled", (*b).into()),
        Selector::Index { selector, index } => {
            fill(m, selector);
            m.insert("index".into(), Value::from(*index as u64));
        }
        Selector::Compound(parts) => {
            for p in parts {
                fill(m, p);
            }
        }
        Selector::Relative {
            target,
            relation,
            anchor,
        } => {
            fill(m, target);
            m.insert(relation.to_string(), to_value(anchor));
        }
    }
}

/// A selector from its flat map, else (a file written before this form)
/// from the derived form, refusing a bare state filter there as the DSL
/// does.
pub fn from_value(v: Value) -> Result<Selector, String> {
    match crate::parser::selector_from_value(v.clone()) {
        Ok(s) => Ok(s),
        Err(flat) => match serde_json::from_value::<Selector>(v) {
            Ok(s) if !s.is_state_filter() => Ok(s),
            _ => Err(flat),
        },
    }
}

pub fn serialize<S: Serializer>(s: &Selector, ser: S) -> Result<S::Ok, S::Error> {
    to_value(s).serialize(ser)
}

pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Selector, D::Error> {
    from_value(Value::deserialize(de)?).map_err(serde::de::Error::custom)
}

/// The same for an `Option<Selector>` field. With `default` and
/// `skip_serializing_if = "Option::is_none"` on the field.
pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(s: &Option<Selector>, ser: S) -> Result<S::Ok, S::Error> {
        s.as_ref().map(to_value).serialize(ser)
    }

    /// A selector this version can't read becomes `None`: an old trace
    /// still loads, with that step described without its selector.
    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Selector>, D::Error> {
        Ok(match Option::<Value>::deserialize(de)? {
            None | Some(Value::Null) => None,
            Some(v) => from_value(v).ok(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::types::Relation;

    fn shapes() -> Vec<Selector> {
        vec![
            Selector::Id("pay".into()),
            Selector::Text("Pay ${X}".into()),
            Selector::Index {
                selector: Box::new(Selector::Text("Edit".into())),
                index: 2,
            },
            Selector::Compound(vec![
                Selector::Role("checkbox".into()),
                Selector::Name("Remember me".into()),
                Selector::Checked(false),
            ]),
            Selector::Relative {
                target: Box::new(Selector::Text("Edit".into())),
                relation: Relation::Below,
                anchor: Box::new(Selector::Id("title".into())),
            },
        ]
    }

    #[test]
    fn the_wire_form_is_the_dsl_and_round_trips() {
        for s in shapes() {
            let v = to_value(&s);
            assert_eq!(from_value(v.clone()).unwrap(), s, "{v}");
        }
        assert_eq!(
            to_value(&shapes()[2]),
            serde_json::json!({"text": "Edit", "index": 2})
        );
    }

    /// A trace written with the derived form still loads.
    #[test]
    fn the_old_derived_form_still_reads() {
        for s in shapes() {
            let old = serde_json::to_value(&s).unwrap();
            assert_eq!(from_value(old.clone()).unwrap(), s, "{old}");
        }
        assert!(from_value(serde_json::json!({"checked": true})).is_err());
    }

    #[derive(Serialize, Deserialize)]
    struct Holder {
        #[serde(default, with = "option", skip_serializing_if = "Option::is_none")]
        selector: Option<Selector>,
    }

    #[test]
    fn an_unreadable_selector_is_none_not_an_error() {
        let h: Holder = serde_json::from_str(r#"{"selector": {"future": 1}}"#).unwrap();
        assert!(h.selector.is_none());
        let h: Holder = serde_json::from_str("{}").unwrap();
        assert!(h.selector.is_none());
        let out = serde_json::to_string(&Holder {
            selector: Some(Selector::Id("a".into())),
        })
        .unwrap();
        assert_eq!(out, r#"{"selector":{"id":"a"}}"#);
    }
}
