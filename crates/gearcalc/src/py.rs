//! Number handling of the Python reference, reproduced operation by operation so that results are the same
//! doubles and not merely close: Python's rounding functions, its compensated `sum`, floor division, the argument
//! order of `max` / `min`, and the reference's round trip through a C# `float`.

/// `zmods._round_even`: the game's `Mathf.RoundToInt` (halves go to the even neighbour). `None` for a non-finite
/// value, where the reference raises.
pub(crate) fn round_even(x: f64) -> Option<i64> {
    if !x.is_finite() {
        return None;
    }
    let floor = x.floor();
    let rounded = if x - floor == 0.5 {
        if floor % 2.0 == 0.0 { floor } else { floor + 1.0 }
    } else {
        (x + 0.5).floor()
    };
    Some(rounded as i64)
}

/// `int(round(x))`. `None` for a non-finite value, where Python raises.
pub(crate) fn round_int(x: f64) -> Option<i64> {
    x.is_finite().then(|| x.round_ties_even() as i64)
}

/// `round(x, digits)`. Python rounds the exact decimal expansion of the double (ties to even) and reads the
/// result back; formatting with a precision does the same here.
pub(crate) fn round_to(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// `zar_data.f32`: the value a C# `float` field holds, as a double. `None` where packing into a float overflows
/// (Python raises).
pub(crate) fn f32_of(x: f64) -> Option<f64> {
    let single = x as f32;
    (!single.is_infinite() || x.is_infinite()).then_some(f64::from(single))
}

/// `a // b` for integers. `None` for a zero divisor.
pub(crate) fn floor_div(a: i64, b: i64) -> Option<i64> {
    let quotient = a.checked_div(b)?;
    Some(if a % b != 0 && (a < 0) != (b < 0) { quotient - 1 } else { quotient })
}

/// The built-in `sum` over floats. Since Python 3.12 it carries a Neumaier compensation term, so a plain running
/// total can differ from it in the last bit.
pub(crate) fn sum(values: impl Iterator<Item = f64>) -> f64 {
    let (mut total, mut compensation) = (0.0_f64, 0.0_f64);
    for x in values {
        let t = total + x;
        compensation += if total.abs() >= x.abs() { (total - t) + x } else { (x - t) + total };
        total = t;
    }
    if compensation != 0.0 && compensation.is_finite() {
        total += compensation;
    }
    total
}

/// `max(a, b)`: Python keeps the first argument unless the second is greater, which matters for NaN.
pub(crate) fn max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `min(a, b)`, with the same rule.
pub(crate) fn min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}
