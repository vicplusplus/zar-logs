//! A JSON tree that remembers the order its keys were added in.
//!
//! `serde_json::Value` sorts object keys unless its `preserve_order` feature is on, and turning that feature on
//! here would switch it on for every crate in the workspace build. The layout is easier to read and to diff with
//! fields in declaration order (which is memory order), so the generator builds this small tree instead; it
//! converts to a `serde_json::Value` for callers and writes text in the same shape as the Python generator's
//! `json.dumps(layout, indent=1)`, so a file from either generator can be compared line by line.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn obj() -> J {
        J::Obj(Vec::new())
    }

    /// Insert or replace. A replaced key keeps its original position, like a Python dict (types can declare the
    /// same field name twice across merged definitions; the last value wins).
    pub fn set(&mut self, key: impl Into<String>, value: J) {
        let J::Obj(items) = self else { panic!("J::set on a non-object") };
        let key = key.into();
        match items.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => items.push((key, value)),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        matches!(self, J::Obj(items) if items.iter().any(|(k, _)| k == key))
    }

    pub fn to_value(&self) -> Value {
        match self {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(*b),
            J::Int(i) => Value::from(*i),
            J::UInt(u) => Value::from(*u),
            // NaN and infinities have no JSON form
            J::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            J::Str(s) => Value::String(s.clone()),
            J::Arr(a) => Value::Array(a.iter().map(J::to_value).collect()),
            J::Obj(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), v.to_value())).collect()),
        }
    }

    /// Text with one space of indentation per level and no trailing newline.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, level: usize) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Int(i) => out.push_str(&i.to_string()),
            J::UInt(u) => out.push_str(&u.to_string()),
            J::Float(f) => out.push_str(&float(*f)),
            J::Str(s) => string(out, s),
            J::Arr(items) if items.is_empty() => out.push_str("[]"),
            J::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    out.push_str(if i == 0 { "\n" } else { ",\n" });
                    indent(out, level + 1);
                    v.write(out, level + 1);
                }
                out.push('\n');
                indent(out, level);
                out.push(']');
            }
            J::Obj(items) if items.is_empty() => out.push_str("{}"),
            J::Obj(items) => {
                out.push('{');
                for (i, (k, v)) in items.iter().enumerate() {
                    out.push_str(if i == 0 { "\n" } else { ",\n" });
                    indent(out, level + 1);
                    string(out, k);
                    out.push_str(": ");
                    v.write(out, level + 1);
                }
                out.push('\n');
                indent(out, level);
                out.push('}');
            }
        }
    }
}

fn indent(out: &mut String, level: usize) {
    out.extend(std::iter::repeat_n(' ', level));
}

/// ASCII-only output (everything else as \uXXXX), so the file reads the same whatever encoding a tool assumes.
fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' | '\u{7f}' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Shortest text that reads back as the same double. Non-finite values are written the way Python writes them
/// (`NaN`, `Infinity`), which is not strict JSON; no enum in the game has such a constant.
fn float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let s = format!("{f:?}");
    match s.split_once('e') {
        // Python writes the exponent with a sign and at least two digits
        Some((mantissa, exp)) => {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_matches_python_indent_1() {
        let mut o = J::obj();
        o.set("b", J::Int(1));
        o.set("a", J::Arr(vec![J::Int(-16), J::Str("x\"é".into())]));
        o.set("b", J::Null); // replaced in place, stays first
        o.set("e", J::obj());
        assert_eq!(o.to_text(), "{\n \"b\": null,\n \"a\": [\n  -16,\n  \"x\\\"\\u00e9\"\n ],\n \"e\": {}\n}");
        assert_eq!(o.to_value()["a"][0], -16);
    }

    #[test]
    fn floats_like_python_repr() {
        assert_eq!(float(0.5), "0.5");
        assert_eq!(float(1e-5), "1e-05");
        assert_eq!(float(1.5e20), "1.5e+20");
    }
}
