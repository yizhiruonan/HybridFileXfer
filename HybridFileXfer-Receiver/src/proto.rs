//! Wire format of the HybridFileXfer protocol.
//!
//! Everything is big endian, matching Java's `DataOutput`. Strings use Java's
//! "modified UTF-8" (UTF-16 code units, surrogate pairs encoded as two 3-byte
//! sequences, NUL as `C0 80`), prefixed by an unsigned 16 bit byte length.

use std::io::{self, Read, Write};

pub const CLIENT_HEADER: &[u8; 4] = b"HFXC";
pub const VERSION_CODE: i32 = 300;

pub const BLOCK_SIZE: u64 = 1024 * 1024;

pub mod controller {
    pub const SHUTDOWN: i16 = 0;
    pub const LIST_FILES: i16 = 1;
    pub const DELETE_FILE: i16 = 2;
    pub const MKDIR: i16 = 3;
    pub const REQUEST_RECEIVE: i16 = 10;
    pub const REQUEST_SEND: i16 = 11;
    /// Not part of the original protocol: the phone sends this short while the
    /// connection is idle so that a dead link is detected. Unknown ids are
    /// ignored by every client implementation, so this is backwards compatible.
    pub const PING: i16 = 20;
}

pub mod transfer {
    pub const FILE: i16 = 0;
    pub const FOLDER: i16 = 1;
    #[allow(dead_code)]
    pub const FILE_SLICE: i16 = 2;
    pub const EOF: i16 = 3;
    pub const END_OF_INTERRUPTED: i16 = 4;
    pub const END_OF_READ_ERROR: i16 = 5;
    pub const END_OF_WRITE_ERROR: i16 = 6;
}

pub mod fs_kind {
    pub const UNIX: i32 = 0;
    pub const WINDOWS: i32 = 1;
}

pub fn current_file_system() -> i32 {
    if std::path::MAIN_SEPARATOR == '\\' {
        fs_kind::WINDOWS
    } else {
        fs_kind::UNIX
    }
}

pub fn write_u8<W: Write>(w: &mut W, v: u8) -> io::Result<()> {
    w.write_all(&[v])
}

pub fn write_bool<W: Write>(w: &mut W, v: bool) -> io::Result<()> {
    write_u8(w, if v { 1 } else { 0 })
}

pub fn write_i16<W: Write>(w: &mut W, v: i16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_i32<W: Write>(w: &mut W, v: i32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_i64<W: Write>(w: &mut W, v: i64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_utf<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    let bytes = encode_modified_utf8(s);
    if bytes.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "string too long for writeUTF",
        ));
    }
    write_i16(w, bytes.len() as i16)?;
    w.write_all(&bytes)
}

pub fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

pub fn read_bool<R: Read>(r: &mut R) -> io::Result<bool> {
    Ok(read_u8(r)? != 0)
}

pub fn read_i16<R: Read>(r: &mut R) -> io::Result<i16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(i16::from_be_bytes(b))
}

pub fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

pub fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_be_bytes(b))
}

pub fn read_utf<R: Read>(r: &mut R) -> io::Result<String> {
    let len = read_i16(r)?;
    if len < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "negative UTF length",
        ));
    }
    let mut bytes = vec![0u8; len as usize];
    r.read_exact(&mut bytes)?;
    Ok(decode_modified_utf8(&bytes))
}

/// Encode a Rust string exactly like Java's `DataOutput::writeUTF`.
pub fn encode_modified_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 2);
    for unit in s.encode_utf16() {
        let cp = unit as u32;
        match cp {
            0 => {
                out.push(0xC0);
                out.push(0x80);
            }
            1..=0x7F => out.push(cp as u8),
            0x80..=0x7FF => {
                out.push(0xC0 | (cp >> 6) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
            _ => {
                // BMP code point, including lone surrogates (CESU-8 style).
                out.push(0xE0 | (cp >> 12) as u8);
                out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
        }
    }
    out
}

/// Decode modified UTF-8 (CESU-8) produced by Java, also tolerating standard
/// UTF-8 4-byte sequences.
pub fn decode_modified_utf8(bytes: &[u8]) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x00..=0x7F => {
                units.push(b as u16);
                i += 1;
            }
            0xC0..=0xDF => {
                if i + 1 >= bytes.len() {
                    units.push(0xFFFD);
                    break;
                }
                let b2 = bytes[i + 1];
                let cp = (((b & 0x1F) as u32) << 6) | (b2 & 0x3F) as u32;
                units.push(cp as u16);
                i += 2;
            }
            0xE0..=0xEF => {
                if i + 2 >= bytes.len() {
                    units.push(0xFFFD);
                    break;
                }
                let b2 = bytes[i + 1];
                let b3 = bytes[i + 2];
                let cp = (((b & 0x0F) as u32) << 12)
                    | (((b2 & 0x3F) as u32) << 6)
                    | (b3 & 0x3F) as u32;
                units.push(cp as u16);
                i += 3;
            }
            0xF0..=0xF7 => {
                if i + 3 >= bytes.len() {
                    units.push(0xFFFD);
                    break;
                }
                let b2 = bytes[i + 1];
                let b3 = bytes[i + 2];
                let b4 = bytes[i + 3];
                let cp = (((b & 0x07) as u32) << 18)
                    | (((b2 & 0x3F) as u32) << 12)
                    | (((b3 & 0x3F) as u32) << 6)
                    | (b4 & 0x3F) as u32;
                if (0x10000..=0x10FFFF).contains(&cp) {
                    let mut buf = [0u16; 2];
                    for u in char::from_u32(cp).unwrap_or('\u{FFFD}').encode_utf16(&mut buf) {
                        units.push(*u);
                    }
                } else {
                    units.push(0xFFFD);
                }
                i += 4;
            }
            _ => {
                units.push(0xFFFD);
                i += 1;
            }
        }
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_utf8_matches_java() {
        assert_eq!(encode_modified_utf8("abc"), vec![0x61, 0x62, 0x63]);
        assert_eq!(
            encode_modified_utf8("测试"),
            vec![0xE6, 0xB5, 0x8B, 0xE8, 0xAF, 0x95]
        );
        // Java encodes U+1F600 as the surrogate pair D83D DE00 -> two 3 byte groups.
        assert_eq!(
            encode_modified_utf8("😀"),
            vec![0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]
        );
        assert_eq!(encode_modified_utf8("a\0b"), vec![0x61, 0xC0, 0x80, 0x62]);
    }

    #[test]
    fn modified_utf8_round_trip() {
        for s in ["", "hello", "中文/路径", "😀 emoji", "a\0b", "ümlaut"] {
            let bytes = encode_modified_utf8(s);
            assert_eq!(decode_modified_utf8(&bytes), s, "round trip for {:?}", s);
        }
        // Cross-check a Java style UTF length prefix.
        let mut buf = Vec::new();
        write_utf(&mut buf, "文件").unwrap();
        assert_eq!(&buf[..2], &[0, 6]);
        assert_eq!(read_utf(&mut buf.as_slice()).unwrap(), "文件");
    }

    #[test]
    fn integers_are_big_endian() {
        let mut buf = Vec::new();
        write_i32(&mut buf, 0x0102_0304).unwrap();
        assert_eq!(buf, vec![1, 2, 3, 4]);
        write_i64(&mut buf, -1).unwrap();
        assert_eq!(&buf[4..], &[0xFF; 8]);
        write_i16(&mut buf, 0x0102).unwrap();
        assert_eq!(&buf[12..], &[1, 2]);
    }
}