//! Numbers and texts the way the class-skills table prints them. The table was first written by a Python script
//! and its cells are compared text for text, so these follow Python's formatting rules (`round`, `%.4f`, `%g`,
//! `repr` of a float), not Rust's defaults: `12.0` not `12`, `nan` not `NaN`, `1e-05` not `1e-5`.

use serde_json::Value;

/// White space as Python's `str` methods and `\s` see it: Unicode white space plus the four ASCII separators
/// U+001C..U+001F, which Rust does not count.
pub(crate) fn is_space(c: char) -> bool { c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c) }

/// `round(x, digits)`: to the nearest decimal, exact halves to the even digit.
pub(crate) fn round(x: f64, digits: usize) -> f64 {
    if x.is_finite() { format!("{x:.digits$}").parse().unwrap_or(x) } else { x }
}

/// `"%.<decimals>f" % x`
pub(crate) fn fixed(x: f64, decimals: usize) -> String {
    if x.is_nan() {
        "nan".into()
    } else if x.is_infinite() {
        if x > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        format!("{x:.decimals$}")
    }
}

/// The table's usual number: at most four decimals, no trailing zeros.
pub(crate) fn num(x: f64) -> String {
    let s = fixed(x, 4);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".into() } else { s.into() }
}

/// `repr(x)`: the shortest text that reads back as `x`, always marked as a float.
pub(crate) fn repr(x: f64) -> String {
    if !x.is_finite() {
        return fixed(x, 0);
    }
    let s = format!("{x:?}");
    match s.split_once('e') {
        Some((mantissa, exponent)) => {
            let exponent: i32 = exponent.parse().unwrap_or(0);
            format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
        }
        None => s,
    }
}

/// `"%+g" % x`: six significant digits, trailing zeros dropped, always with a sign.
pub(crate) fn signed_general(x: f64) -> String {
    let sign = if x.is_sign_negative() { '-' } else { '+' };
    if !x.is_finite() {
        return format!("{sign}{}", fixed(x.abs(), 0));
    }
    let scientific = format!("{:.5e}", x.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trimmed = |digits: &str| match digits.contains('.') {
        true => digits.trim_end_matches('0').trim_end_matches('.').to_string(),
        false => digits.to_string(),
    };
    if x == 0.0 {
        format!("{sign}0")
    } else if !(-4..6).contains(&exponent) {
        format!("{sign}{}e{}{:02}", trimmed(mantissa), if exponent < 0 { '-' } else { '+' }, exponent.abs())
    } else {
        format!("{sign}{}", trimmed(&format!("{:.*}", (5 - exponent) as usize, x.abs())))
    }
}

/// `str(v)` of a scalar read from the dump: whole numbers plain, floats as `repr`.
pub(crate) fn text(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_f64() => repr(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `"%d" % v`: a float loses its fraction.
pub(crate) fn whole(v: &Value) -> String {
    match v {
        Value::Number(n) if n.is_f64() => (n.as_f64().unwrap_or(0.0).trunc() as i64).to_string(),
        Value::Bool(b) => u8::from(*b).to_string(),
        other => text(other),
    }
}

/// The four XML entities the localisation texts use, decoded in the reference's order (so `&amp;lt;` stays `&lt;`).
pub(crate) fn decode(s: &str) -> String {
    s.replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&").replace("&quot;", "\"")
}

/// A localisation text as one line: entities decoded, every run of white space one space, none at the ends.
pub(crate) fn clean(s: &str) -> String {
    decode(s).split(is_space).filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_numbers() {
        assert_eq!([num(1.0), num(0.25), num(1.23456), num(-0.00001), num(f64::NAN)], ["1", "0.25", "1.2346", "-0", "nan"]);
        assert_eq!([round(0.125, 2), round(2.675, 2), round(-0.000001, 5)], [0.12, 2.67, -0.0]);
        assert_eq!([repr(15.0), repr(17.5), repr(1e16), repr(0.00001), repr(1.5e-7)], ["15.0", "17.5", "1e+16", "1e-05", "1.5e-07"]);
        assert_eq!([fixed(2.5, 0), fixed(0.125, 2), fixed(f64::NAN, 1)], ["2", "0.12", "nan"]);
    }

    #[test]
    fn general_format() {
        let g = signed_general;
        assert_eq!([g(75.0), g(-0.5), g(0.05), g(0.0), g(1234567.0), g(0.00001234), g(123456.7), g(999999.5)],
                   ["+75", "-0.5", "+0.05", "+0", "+1.23457e+06", "+1.234e-05", "+123457", "+1e+06"]);
    }

    #[test]
    fn texts() {
        assert_eq!(clean("  a &amp;gt; b\r\n\t&lt;c&gt; &quot;d&quot; "), "a &gt; b <c> \"d\"");
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!([text(&v("10000")), text(&v("2.50")), whole(&v("3.9")), whole(&v("20"))], ["10000", "2.5", "3", "20"]);
    }
}
