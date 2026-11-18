//! Bounds-checked little-endian reads shared by the two file parsers.
//!
//! Both input files come from a game that updates on its own schedule, so a read that falls outside the file must
//! become an error message ("this build is laid out differently"), never a panic inside the app.

pub fn get<const N: usize>(d: &[u8], off: usize) -> Result<[u8; N], String> {
    off.checked_add(N)
        .and_then(|end| d.get(off..end))
        .map(|s| s.try_into().unwrap())
        .ok_or_else(|| format!("read of {N} bytes at offset {off:#x} is outside the file ({:#x} bytes)", d.len()))
}

pub fn u8_at(d: &[u8], off: usize) -> Result<u8, String> {
    Ok(get::<1>(d, off)?[0])
}

pub fn u16_at(d: &[u8], off: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(get(d, off)?))
}

pub fn u32_at(d: &[u8], off: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(get(d, off)?))
}

pub fn i32_at(d: &[u8], off: usize) -> Result<i32, String> {
    Ok(i32::from_le_bytes(get(d, off)?))
}

pub fn u64_at(d: &[u8], off: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(get(d, off)?))
}

/// The NUL-terminated string at `off`, with invalid UTF-8 replaced rather than rejected (names are only compared
/// and exported, and one odd name must not stop the whole layout).
pub fn cstr(d: &[u8], off: usize) -> Result<String, String> {
    let tail = d.get(off..).ok_or_else(|| format!("string at offset {off:#x} is outside the file"))?;
    let len = memchr::memchr(0, tail).ok_or_else(|| format!("unterminated string at offset {off:#x}"))?;
    Ok(String::from_utf8_lossy(&tail[..len]).into_owned())
}
