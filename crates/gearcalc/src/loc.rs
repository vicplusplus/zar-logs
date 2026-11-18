//! Display names from the English localisation file (`loc_en.xml`): one `<T id="KEY">text</T>` element per text,
//! and a definition's `title` field holds the KEY.
//!
//! The Python tools read the file in two places with two different patterns, and names have to match each tool:
//! weapons are named by tools/dps.py (`loc_table`), armour pieces by tools/zmods.py (`loc`). Both scan the file as
//! text and accept only the exact opening `<T id="KEY">` (the file has a few `<T id ="KEY">`, which neither sees),
//! and neither decodes XML entities.

use std::collections::HashMap;

/// White space as Python's `str.strip` sees it: Unicode white space plus U+001C..U+001F, which Rust leaves out.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// The reference reads the file in text mode, which turns every line ending into "\n".
fn unix_newlines(text: &str) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text.to_string() }
}

/// Calls `found(key, rest)` for each `<T id="KEY">` in the file, `rest` being the file after the tag. `found`
/// returns how much of `rest` its element takes up, or `None` when no element starts here after all.
fn scan<'a>(xml: &'a str, mut found: impl FnMut(&'a str, &'a str) -> Option<usize>) {
    const OPEN: &str = "<T id=\"";
    let mut pos = 0;
    while let Some(offset) = xml[pos..].find(OPEN) {
        let key_start = pos + offset + OPEN.len();
        pos = key_start;
        let Some(key_len) = xml[key_start..].find('"') else { break };
        let after_key = key_start + key_len;
        if key_len == 0 || !xml[after_key..].starts_with("\">") {
            continue;
        }
        let body_start = after_key + 2;
        if let Some(taken) = found(&xml[key_start..after_key], &xml[body_start..]) {
            pos = body_start + taken;
        }
    }
}

/// tools/dps.py `loc_table`: `<T id="([^"]+)">([^<]*)</T>`. The text must not contain a tag, is kept as written,
/// and the last element with a key wins.
pub(crate) fn weapon_names(xml: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    scan(xml, |key, rest| {
        let end = rest.find('<').unwrap_or(rest.len());
        if !rest[end..].starts_with("</T>") {
            return None;
        }
        out.insert(unix_newlines(key), unix_newlines(&rest[..end]));
        Some(end + 4)
    });
    out
}

/// tools/zmods.py `loc`: `<T id="([^"]+)">(.*?)</T>` across lines. The text runs to the next `</T>`, is trimmed,
/// and the first element with a key wins.
pub(crate) fn armour_names(xml: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    scan(xml, |key, rest| {
        let end = rest.find("</T>")?;
        out.entry(unix_newlines(key)).or_insert_with(|| unix_newlines(&rest[..end]).trim_matches(is_space).to_string());
        Some(end + 4)
    });
    out
}
