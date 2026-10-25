//! What `Extract` produces: records made of typed values.

use std::{fmt, ops::Index};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// An optional field that was not there.
    Null,
    Text(String),
    Int(i64),
    Number(f64),
    Bool(bool),
    List(Vec<Value>),
}

static NULL: Value = Value::Null;

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Text(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Text(s)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}
impl From<f64> for Value {
    fn from(n: f64) -> Value {
        Value::Number(n)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Numbers, and integers widened.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.write_json(&mut s);
        s
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Text(t) => json_string(out, t),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::List(l) => {
                out.push('[');
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
        }
    }
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Text(s) => f.write_str(s),
            Value::Int(i) => write!(f, "{i}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::List(l) => {
                f.write_str("[")?;
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
        }
    }
}

/// One matched element, reduced to the fields a rule asked for, in the order it listed them.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub(crate) rule: usize,
    pub(crate) rule_name: Option<String>,
    pub(crate) fields: Vec<(String, Value)>,
}

impl Record {
    /// Position of the rule that produced this record.
    pub fn rule_index(&self) -> usize {
        self.rule
    }

    pub fn rule_name(&self) -> Option<&str> {
        self.rule_name.as_deref()
    }

    pub fn get(&self, field: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == field).map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// `{"id":"1","total":9.5}`
    pub fn to_json(&self) -> String {
        self.json(false)
    }

    /// Like [`to_json`](Self::to_json), with a leading `"_rule"` key when the rule is named.
    pub fn to_json_tagged(&self) -> String {
        self.json(true)
    }

    fn json(&self, tag: bool) -> String {
        let mut s = String::from("{");
        let mut first = true;
        if let (true, Some(n)) = (tag, &self.rule_name) {
            s.push_str("\"_rule\":");
            json_string(&mut s, n);
            first = false;
        }
        for (k, v) in &self.fields {
            if !first {
                s.push(',');
            }
            first = false;
            json_string(&mut s, k);
            s.push(':');
            v.write_json(&mut s);
        }
        s.push('}');
        s
    }
}

/// `record["id"]`; a missing field reads as `Value::Null` instead of panicking.
impl Index<&str> for Record {
    type Output = Value;

    fn index(&self, field: &str) -> &Value {
        self.get(field).unwrap_or(&NULL)
    }
}
