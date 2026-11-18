//! The game's English texts, from the localisation XML (`loc_en.xml`: the English TextAsset of the game's
//! localisation bundle). Every text is one `<T id="KEY">text</T>` element; definitions refer to texts by KEY
//! (an ability's `title` field holds such a key).
//!
//! Two tables come out of one pass, because the reference tooling read the file in two places with two slightly
//! different rules and the pack has to match both:
//!
//! * `display` is the pack's `loc` section. Its reader tolerates white space inside the tag (the file has a few
//!   `<T id ="...">`), decodes the five XML entities and trims the text.
//! * `raw` is what the class-skills table takes its names, descriptions and tooltips from. Its reader accepts
//!   only the exact form `<T id="...">` and keeps the text as written: entities stay encoded and nothing is
//!   trimmed (the table decodes and tidies descriptions itself, and leaves names alone).
//!
//! The file is scanned as text, not parsed as XML: comments and the grouping elements around the texts are
//! ignored, which is also what the reference does.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::fmt::is_space;

pub(crate) struct Texts {
    pub display: Map<String, Value>,
    pub raw: HashMap<String, String>,
}

/// Matches `<T\s+id\s*=\s*"([^"]+)"\s*>` at the start of `s`: (key, length of the opening tag).
fn opening_tag(s: &str) -> Option<(&str, usize)> {
    let rest = s.strip_prefix("<T")?;
    let after_space = rest.trim_start_matches(is_space);
    if after_space.len() == rest.len() {
        return None;
    }
    let rest = after_space.strip_prefix("id")?.trim_start_matches(is_space);
    let rest = rest.strip_prefix('=')?.trim_start_matches(is_space).strip_prefix('"')?;
    let key = &rest[..rest.find('"')?];
    if key.is_empty() {
        return None;
    }
    let rest = rest[key.len() + 1..].trim_start_matches(is_space).strip_prefix('>')?;
    Some((key, s.len() - rest.len()))
}

pub(crate) fn read(xml: &str) -> Texts {
    let mut texts = Texts { display: Map::new(), raw: HashMap::new() };
    let mut pos = 0;
    while let Some(found) = xml[pos..].find("<T") {
        let start = pos + found;
        let Some((key, tag_len)) = opening_tag(&xml[start..]) else {
            pos = start + 2;
            continue;
        };
        let body_start = start + tag_len;
        // without a closing tag here there is none further on either
        let Some(body_len) = xml[body_start..].find("</T>") else { break };
        let body = &xml[body_start..body_start + body_len];
        // "&amp;" first, as the reference does, so "&amp;lt;" ends up as "<"
        let decoded = body.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&apos;", "'")
            .replace("&quot;", "\"");
        // a key that occurs twice keeps its first position and takes the later text, in both tables
        texts.display.insert(key.to_string(), Value::String(decoded.trim_matches(is_space).to_string()));
        if tag_len == "<T id=\"\">".len() + key.len() && xml[start..].starts_with("<T id=\"") {
            texts.raw.insert(key.to_string(), body.to_string());
        }
        pos = body_start + body_len + "</T>".len();
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_readers() {
        let t = read("<!-- <Tx> --><Team><T id=\"a\"> A &amp;amp; &lt;b&gt; </T>\n<T id =\"b\">B</T><T  id=\"c\"\n>C\nc</T><T id=\"\">x</T>\
                      <T id=\"a\">again</T></Team>");
        let keys: Vec<&String> = t.display.keys().collect();
        assert_eq!(keys, ["a", "b", "c"]);
        assert_eq!(t.display["a"], "again");
        assert_eq!(t.display["c"], "C\nc");
        assert_eq!(t.raw.get("a").map(String::as_str), Some("again"));
        assert!(!t.raw.contains_key("b") && !t.raw.contains_key("c"));
        let t = read("<T id=\"a\"> A &amp;lt; &apos;b&apos; </T>");
        assert_eq!(t.display["a"], "A < 'b'");
        assert_eq!(t.raw["a"], " A &amp;lt; &apos;b&apos; ");
    }
}
