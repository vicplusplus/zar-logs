//! Reading one class file of the ScriptableObject dump (`<Class>.json`: `{path id: {field: value, ...}}`).
//!
//! The dump is written by Python, whose JSON writes the non-finite floats as the bare words `Infinity`,
//! `-Infinity` and `NaN` (a weapon that fires once has `"timeBetweenShots": Infinity`). JSON proper has no such
//! numbers and `serde_json` rejects the file, so they are replaced before parsing: an infinity by the largest
//! finite number of its sign, which keeps it a set, non-zero value; `NaN` by `null`.

use serde_json::{Map, Value};

/// The text with the non-finite number words outside of strings replaced.
fn finite(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    let mut in_string = false;
    while let Some(at) = rest.find(['"', '\\', 'I', 'N']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let (replacement, taken) = if in_string {
            // a backslash takes the next character with it, so an escaped quote does not end the string
            let escaped = rest.starts_with('\\');
            in_string = escaped || !rest.starts_with('"');
            (None, if escaped { 1 + rest[1..].chars().next().map_or(0, char::len_utf8) } else { 1 })
        } else if rest.starts_with("Infinity") {
            (Some("1.7976931348623157e308"), "Infinity".len())
        } else if rest.starts_with("NaN") {
            (Some("null"), "NaN".len())
        } else {
            in_string = rest.starts_with('"');
            (None, 1)
        };
        out.push_str(replacement.unwrap_or(&rest[..taken]));
        rest = &rest[taken..];
    }
    out.push_str(rest);
    out
}

/// Parses the content of a class file into the form [`crate::build`] takes, with the objects and their fields in
/// file order.
pub fn parse_dump(text: &str) -> Result<Map<String, Value>, String> {
    let parsed = if text.contains("Infinity") || text.contains("NaN") {
        serde_json::from_str(&finite(text))
    } else {
        serde_json::from_str(text)
    };
    parsed.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_numbers() {
        let o = parse_dump(r#"{"2":{"a":Infinity,"b":[-Infinity,NaN],"NaN \" Infinity\\":"Infinity","c":1}, "1":{}}"#).unwrap();
        assert_eq!(o.keys().collect::<Vec<_>>(), ["2", "1"]);
        let two = o["2"].as_object().unwrap();
        assert_eq!(two["a"].as_f64(), Some(f64::MAX));
        assert_eq!(two["b"][0].as_f64(), Some(f64::MIN));
        assert!(two["b"][1].is_null());
        assert_eq!(two["NaN \" Infinity\\"], "Infinity");
        assert_eq!(two.keys().last().map(String::as_str), Some("c"));
    }
}
