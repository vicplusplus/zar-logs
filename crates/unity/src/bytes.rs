//! A bounds-checked cursor over a byte slice.
//!
//! Every Unity container mixes byte orders (the bundle header and the first bytes of a SerializedFile are big
//! endian, everything after is whatever the file declares, little endian on every current platform), so the order is
//! a field of the cursor rather than a type parameter. A read past the end is an error, never a panic: the input is
//! a file on the user's disk that a game update can change under us.

pub struct Cursor<'a> {
    pub data: &'a [u8],
    pub pos: usize,
    pub big_endian: bool,
}

macro_rules! number {
    ($name:ident, $t:ty) => {
        pub fn $name(&mut self) -> Result<$t, String> {
            const N: usize = std::mem::size_of::<$t>();
            let mut raw = [0u8; N];
            raw.copy_from_slice(self.bytes(N)?);
            Ok(if self.big_endian { <$t>::from_be_bytes(raw) } else { <$t>::from_le_bytes(raw) })
        }
    };
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8], big_endian: bool) -> Self {
        Cursor { data, pos: 0, big_endian }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.remaining() {
            return Err(format!("unexpected end of data: {} bytes wanted at {}, {} left", n, self.pos, self.remaining()));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    number!(i16, i16);
    number!(u16, u16);
    number!(i32, i32);
    number!(u32, u32);
    number!(i64, i64);
    number!(u64, u64);
    number!(f32, f32);
    number!(f64, f64);

    /// A NUL-terminated string. Names in these files are ASCII; anything else is replaced rather than refused.
    pub fn cstr(&mut self) -> Result<String, String> {
        let rest = &self.data[self.pos.min(self.data.len())..];
        let len = rest.iter().position(|&b| b == 0).ok_or("unterminated string")?;
        self.pos += len + 1;
        Ok(String::from_utf8_lossy(&rest[..len]).into_owned())
    }

    /// Skips the padding up to the next multiple of `n`, counted from the start of the slice.
    pub fn align(&mut self, n: usize) {
        self.pos += (n - self.pos % n) % n;
    }
}
