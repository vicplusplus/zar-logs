//! UnityFS asset bundles (`*.bundle`): an archive of a few named files stored as one compressed stream.
//!
//! Layout, all integers big endian:
//!
//! ```text
//! "UnityFS\0"  u32 format (8 here)  "5.x.x\0"  "6000.5.10f1\0"
//! i64 file size   u32 compressed size of the directory   u32 its uncompressed size   u32 flags
//! (format >= 7: padding to a multiple of 16)
//! directory, compressed as flags & 0x3f says; at the end of the file instead when flags & 0x80
//! (flags & 0x200: padding to a multiple of 16)
//! the data blocks, back to back
//! ```
//!
//! The directory, once decompressed:
//!
//! ```text
//! 16 bytes hash
//! i32 block count,  then per block:  u32 uncompressed size, u32 compressed size, u16 flags (& 0x3f = compression)
//! i32 entry count,  then per entry:  i64 offset, i64 size, u32 flags (4 = a SerializedFile), name\0
//! ```
//!
//! Concatenating the decompressed blocks gives one long stream; an entry is a byte range of that stream. The game's
//! bundles hold a `CAB-<hash>` SerializedFile (the objects, see [`crate::serialized`]) and often a `CAB-<hash>.resS`
//! next to it with the raw texture pixels the objects point into.
//!
//! Compression is 0 (stored), 1 (LZMA), 2 (LZ4) or 3 (LZ4HC, same decoder as LZ4). Every block of this game is 0 or
//! 3, so LZMA is refused instead of carried as a dependency. Blocks are at most 128 KiB of data each and independent,
//! which is what makes this reader cheap: [`Bundle::read`] decompresses only the blocks a byte range touches, so
//! pulling a few objects out of a 240 MB bundle does not inflate the other 400 MB.

use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::bytes::Cursor;

const COMPRESSION_MASK: u32 = 0x3f;
const DIRECTORY_AT_END: u32 = 0x80;
const PADDING_BEFORE_BLOCKS: u32 = 0x200;
const SERIALIZED_FILE: u32 = 4;

/// One file inside the archive.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// Position in the decompressed stream.
    pub offset: u64,
    pub size: u64,
    pub flags: u32,
}

impl Entry {
    pub fn is_serialized_file(&self) -> bool {
        self.flags & SERIALIZED_FILE != 0
    }
}

struct Block {
    file_offset: u64,
    stream_offset: u64,
    compressed: u32,
    size: u32,
    compression: u32,
}

pub struct Bundle {
    file: File,
    /// File name of the bundle, for error messages and the `_bundle` field of the data dump.
    pub name: String,
    blocks: Vec<Block>,
    entries: Vec<Entry>,
    stream_len: u64,
    /// The block decompressed last. Small objects sit next to each other, so consecutive reads mostly land in the
    /// same block; one slot is enough and keeps memory flat.
    last: Mutex<Option<(usize, Arc<Vec<u8>>)>>,
}

impl Bundle {
    pub fn open(path: &Path) -> Result<Bundle, String> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let fail = |what: String| format!("{}: {}", name, what);
        let file = File::open(path).map_err(|e| fail(e.to_string()))?;
        let file_len = file.metadata().map_err(|e| fail(e.to_string()))?.len();

        let mut head = vec![0u8; file_len.min(256) as usize];
        read_at(&file, &mut head, 0).map_err(|e| fail(e.to_string()))?;
        let mut r = Cursor::new(&head, true);
        let signature = r.cstr().map_err(&fail)?;
        if signature != "UnityFS" {
            return Err(fail(format!("not a UnityFS bundle (signature {:?})", signature)));
        }
        let format = r.u32().map_err(&fail)?;
        r.cstr().map_err(&fail)?; // player version, "5.x.x"
        r.cstr().map_err(&fail)?; // engine version
        r.i64().map_err(&fail)?; // file size
        let dir_compressed = r.u32().map_err(&fail)? as usize;
        let dir_size = r.u32().map_err(&fail)? as usize;
        let flags = r.u32().map_err(&fail)?;
        if format >= 7 {
            r.align(16);
        }
        let after_header = r.pos as u64;
        let (dir_offset, mut data_start) = if flags & DIRECTORY_AT_END != 0 {
            (file_len.saturating_sub(dir_compressed as u64), after_header)
        } else {
            (after_header, after_header + dir_compressed as u64)
        };
        if flags & PADDING_BEFORE_BLOCKS != 0 {
            data_start = (data_start + 15) & !15;
        }

        let mut packed = vec![0u8; dir_compressed];
        read_at(&file, &mut packed, dir_offset).map_err(|e| fail(e.to_string()))?;
        let mut directory = vec![0u8; dir_size];
        decompress(flags & COMPRESSION_MASK, &packed, &mut directory).map_err(&fail)?;

        let mut r = Cursor::new(&directory, true);
        r.bytes(16).map_err(&fail)?;
        let count = r.i32().map_err(&fail)?.max(0) as usize;
        let mut blocks = Vec::with_capacity(count);
        let (mut file_offset, mut stream_offset) = (data_start, 0u64);
        for _ in 0..count {
            let size = r.u32().map_err(&fail)?;
            let compressed = r.u32().map_err(&fail)?;
            let compression = r.u16().map_err(&fail)? as u32 & COMPRESSION_MASK;
            blocks.push(Block { file_offset, stream_offset, compressed, size, compression });
            file_offset += compressed as u64;
            stream_offset += size as u64;
        }
        let count = r.i32().map_err(&fail)?.max(0) as usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let offset = r.i64().map_err(&fail)? as u64;
            let size = r.i64().map_err(&fail)? as u64;
            let flags = r.u32().map_err(&fail)?;
            let name = r.cstr().map_err(&fail)?;
            entries.push(Entry { name, offset, size, flags });
        }
        Ok(Bundle { file, name, blocks, entries, stream_len: stream_offset, last: Mutex::new(None) })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry with this file name. Objects refer to their resource file by a path such as
    /// `archive:/CAB-xx/CAB-xx.resS`; only the last component names the entry.
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// `len` bytes of the decompressed stream starting at `offset`.
    pub fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; len];
        self.read_into(offset, &mut out)?;
        Ok(out)
    }

    pub fn read_into(&self, offset: u64, out: &mut [u8]) -> Result<(), String> {
        let end = offset + out.len() as u64;
        if end > self.stream_len {
            return Err(format!("{}: read of {}..{} is past the end ({})", self.name, offset, end, self.stream_len));
        }
        let mut index = self.blocks.partition_point(|b| b.stream_offset + b.size as u64 <= offset);
        let mut pos = offset;
        while pos < end {
            let block = &self.blocks[index];
            let skip = (pos - block.stream_offset) as usize;
            let n = (block.size as usize - skip).min((end - pos) as usize);
            let target = &mut out[(pos - offset) as usize..][..n];
            if n == 0 {
                // an empty block holds nothing to copy
            } else if n == block.size as usize {
                // A block needed whole goes straight into the caller's buffer: texture pixels span dozens of blocks
                // and would otherwise be copied twice.
                self.load(index, target)?;
            } else {
                target.copy_from_slice(&self.cached(index)?[skip..skip + n]);
            }
            pos += n as u64;
            index += 1;
        }
        Ok(())
    }

    fn cached(&self, index: usize) -> Result<Arc<Vec<u8>>, String> {
        if let Some((i, data)) = self.last.lock().unwrap().as_ref() {
            if *i == index {
                return Ok(data.clone());
            }
        }
        // Decompressed outside the lock, so that threads reading different textures do not queue behind each other.
        let mut data = vec![0u8; self.blocks[index].size as usize];
        self.load(index, &mut data)?;
        let data = Arc::new(data);
        *self.last.lock().unwrap() = Some((index, data.clone()));
        Ok(data)
    }

    fn load(&self, index: usize, out: &mut [u8]) -> Result<(), String> {
        let block = &self.blocks[index];
        let fail = |what: String| format!("{}: block {}: {}", self.name, index, what);
        if block.compression == 0 {
            return read_at(&self.file, out, block.file_offset).map_err(|e| fail(e.to_string()));
        }
        let mut packed = vec![0u8; block.compressed as usize];
        read_at(&self.file, &mut packed, block.file_offset).map_err(|e| fail(e.to_string()))?;
        decompress(block.compression, &packed, out).map_err(fail)
    }
}

fn decompress(compression: u32, packed: &[u8], out: &mut [u8]) -> Result<(), String> {
    match compression {
        0 if packed.len() == out.len() => {
            out.copy_from_slice(packed);
            Ok(())
        }
        2 | 3 => match lz4_flex::block::decompress_into(packed, out) {
            Ok(n) if n == out.len() => Ok(()),
            Ok(n) => Err(format!("LZ4 block gave {} bytes, {} expected", n, out.len())),
            Err(e) => Err(format!("LZ4: {}", e)),
        },
        1 => Err("LZMA compression is not supported (the game's bundles are LZ4)".into()),
        other => Err(format!("unknown compression {}", other)),
    }
}

/// Positional read, so that one open file can serve several threads without a shared seek position.
#[cfg(windows)]
fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}
