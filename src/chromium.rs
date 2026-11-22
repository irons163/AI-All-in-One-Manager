//! Just enough of Chromium's on-disk formats to *read* an IndexedDB folder:
//! LevelDB log files and tables, Snappy compression, and Blink's compressed
//! IndexedDB values. Used only to find a cached account email for display;
//! nothing here ever writes, and any data it cannot make sense of is skipped.

use std::fs;
use std::path::Path;

/// Files larger than this are not inspected.
const MAX_FILE: u64 = 64 << 20;
/// A decompressed value larger than this is treated as corrupt.
const MAX_OUTPUT: usize = 64 << 20;
/// Blink marks an IndexedDB value it compressed with Snappy by these bytes
/// (V8 version tag, the "requires processing" pseudo version 17, and 2 for
/// "compressed with Snappy"), followed by raw Snappy data.
const COMPRESSED_VALUE: &[u8] = b"\xff\x11\x02";

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn varint_usize(bytes: &[u8], at: &mut usize) -> Option<usize> {
    usize::try_from(varint(bytes, at)?).ok()
}

/// Decode raw (unframed) Snappy data starting at the beginning of `input`.
/// `input` may run on past the end of the compressed data; decoding stops
/// once the declared length has been produced. Returns `None` for anything
/// malformed.
pub fn snappy_decompress(input: &[u8]) -> Option<Vec<u8>> {
    let mut at = 0;
    let len = varint_usize(input, &mut at)?;
    if len > MAX_OUTPUT {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(len.min(1 << 20));
    while out.len() < len {
        let tag = *input.get(at)?;
        at += 1;
        let (copy_len, offset) = match tag & 3 {
            0 => {
                let mut lit = usize::from(tag >> 2);
                if lit >= 60 {
                    let width = lit - 59;
                    let raw = input.get(at..at + width)?;
                    at += width;
                    lit = raw
                        .iter()
                        .rev()
                        .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
                }
                let lit = lit + 1;
                out.extend_from_slice(input.get(at..at.checked_add(lit)?)?);
                at += lit;
                continue;
            }
            1 => {
                let len = usize::from((tag >> 2) & 7) + 4;
                let off = (usize::from(tag >> 5) << 8) | usize::from(*input.get(at)?);
                at += 1;
                (len, off)
            }
            2 => {
                let raw = input.get(at..at + 2)?;
                at += 2;
                (usize::from(tag >> 2) + 1, usize::from(u16::from_le_bytes([raw[0], raw[1]])))
            }
            _ => {
                let raw = input.get(at..at + 4)?;
                at += 4;
                let off = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                (usize::from(tag >> 2) + 1, usize::try_from(off).ok()?)
            }
        };
        if offset == 0 || offset > out.len() || out.len() + copy_len > len {
            return None;
        }
        let start = out.len() - offset;
        // Overlapping copies are allowed (offset < len repeats a pattern).
        for i in 0..copy_len {
            let byte = out[start + i];
            out.push(byte);
        }
    }
    (out.len() == len).then_some(out)
}

/// The record data of a LevelDB log file with the 7-byte block and fragment
/// headers taken out, so values that span a 32 KiB block boundary become
/// contiguous again.
pub fn log_payload(bytes: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 32 * 1024;
    const HEADER: usize = 7;
    let mut out = Vec::with_capacity(bytes.len());
    let mut pos = 0;
    while pos + HEADER <= bytes.len() {
        let left = BLOCK - pos % BLOCK;
        if left < HEADER {
            pos += left; // trailer padding
            continue;
        }
        let len = usize::from(u16::from_le_bytes([bytes[pos + 4], bytes[pos + 5]]));
        let kind = bytes[pos + 6];
        if kind == 0 && len == 0 {
            pos += left; // preallocated zeroes
            continue;
        }
        let start = pos + HEADER;
        let end = start + len;
        if len > left - HEADER || end > bytes.len() {
            break; // torn tail
        }
        out.extend_from_slice(&bytes[start..end]);
        pos = end;
    }
    out
}

/// Read one table block (handle = offset, size), undoing Snappy if the block
/// is compressed.
fn table_block(bytes: &[u8], offset: usize, size: usize) -> Option<Vec<u8>> {
    let data = bytes.get(offset..offset.checked_add(size)?)?;
    match *bytes.get(offset + size)? {
        0 => Some(data.to_vec()),
        1 => snappy_decompress(data),
        _ => None,
    }
}

/// The (key, value) entries of a decoded table block.
fn block_entries(block: &[u8]) -> Vec<(Vec<u8>, &[u8])> {
    let mut entries = Vec::new();
    let Some(count_at) = block.len().checked_sub(4) else {
        return entries;
    };
    let restarts = u32::from_le_bytes(block[count_at..].try_into().unwrap()) as usize;
    let Some(limit) = restarts
        .checked_mul(4)
        .and_then(|r| count_at.checked_sub(r))
    else {
        return entries;
    };
    let mut at = 0;
    let mut key: Vec<u8> = Vec::new();
    while at < limit {
        let (Some(shared), Some(fresh), Some(vlen)) = (
            varint_usize(block, &mut at),
            varint_usize(block, &mut at),
            varint_usize(block, &mut at),
        ) else {
            break;
        };
        let Some(delta) = block.get(at..at + fresh) else { break };
        at += fresh;
        let Some(value) = block.get(at..at + vlen) else { break };
        at += vlen;
        if shared > key.len() {
            break;
        }
        key.truncate(shared);
        key.extend_from_slice(delta);
        entries.push((key.clone(), value));
    }
    entries
}

/// All data blocks of a LevelDB table (`.ldb`), decompressed and
/// concatenated. `None` if it is not a table this can read.
pub fn table_payload(bytes: &[u8]) -> Option<Vec<u8>> {
    const FOOTER: usize = 48;
    const MAGIC: [u8; 8] = 0xdb47_7524_8b80_fb57u64.to_le_bytes();
    let footer_at = bytes.len().checked_sub(FOOTER)?;
    if bytes[bytes.len() - 8..] != MAGIC {
        return None;
    }
    let mut at = footer_at;
    let _meta_offset = varint_usize(bytes, &mut at)?;
    let _meta_size = varint_usize(bytes, &mut at)?;
    let index_offset = varint_usize(bytes, &mut at)?;
    let index_size = varint_usize(bytes, &mut at)?;
    let index = table_block(bytes, index_offset, index_size)?;
    let mut out = Vec::new();
    for (_, handle) in block_entries(&index) {
        let mut h = 0;
        let (Some(offset), Some(size)) = (varint_usize(handle, &mut h), varint_usize(handle, &mut h))
        else {
            continue;
        };
        if let Some(block) = table_block(bytes, offset, size) {
            out.extend_from_slice(&block);
        }
    }
    Some(out)
}

/// Every readable form of the data in one IndexedDB file: its LevelDB
/// payload (or the raw bytes for other files, such as values that Blink
/// moved into blob files), plus each compressed IndexedDB value in it,
/// decompressed.
pub fn readable_parts(path: &Path) -> Vec<Vec<u8>> {
    let Ok(meta) = fs::metadata(path) else { return Vec::new() };
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Vec::new();
    }
    let Ok(bytes) = fs::read(path) else { return Vec::new() };
    readable_parts_of(&path.to_string_lossy(), bytes)
}

/// `readable_parts` for content already in memory; `name` only decides how
/// the bytes are read (by its extension).
pub fn readable_parts_of(name: &str, bytes: Vec<u8>) -> Vec<Vec<u8>> {
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    let bases = match extension {
        // A log being written may end in a torn record that the reassembly
        // drops, so the raw bytes are searched too.
        Some("log") => vec![log_payload(&bytes), bytes],
        Some("ldb") | Some("sst") => vec![table_payload(&bytes).unwrap_or(bytes)],
        _ => vec![bytes],
    };
    let mut parts = Vec::new();
    for base in bases {
        let mut from = 0;
        while let Some(pos) = find_bytes(&base[from..], COMPRESSED_VALUE) {
            let start = from + pos + COMPRESSED_VALUE.len();
            if let Some(value) = snappy_decompress(&base[start..]) {
                parts.push(value);
            }
            from = start;
        }
        parts.push(base);
    }
    parts
}

pub fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Snappy-encode with literals only, plus optional copies given as
    /// (position in output, length, offset): enough to build test inputs.
    pub fn snappy_encode(data: &[u8], copies: &[(usize, usize, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut n = data.len() as u64;
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        let literal = |out: &mut Vec<u8>, chunk: &[u8]| {
            for piece in chunk.chunks(256) {
                let l = piece.len() - 1;
                if l < 60 {
                    out.push((l as u8) << 2);
                } else {
                    out.push(60 << 2);
                    out.push(l as u8);
                }
                out.extend_from_slice(piece);
            }
        };
        let mut pos = 0;
        for &(at, len, offset) in copies {
            literal(&mut out, &data[pos..at]);
            assert!((4..=11).contains(&len) && offset < 2048, "copy-1 form only");
            out.push(((offset >> 8) as u8) << 5 | ((len - 4) as u8) << 2 | 1);
            out.push((offset & 0xff) as u8);
            pos = at + len;
        }
        literal(&mut out, &data[pos..]);
        out
    }

    #[test]
    fn snappy_round_trips_literals_and_copies() {
        let text = b"abcdabcdabcd-the email_address is me@gmail.com";
        let mail = text.windows(4).position(|w| w == b"mail").unwrap();
        let gmail = text.windows(5).position(|w| w == b"gmail").unwrap() + 1;
        let enc = snappy_encode(text, &[(4, 8, 4), (gmail, 4, gmail - mail)]);
        assert!(enc.len() < text.len() + 4);
        assert_eq!(snappy_decompress(&enc).unwrap(), text);

        let mut trailing = enc.clone();
        trailing.extend_from_slice(b"whatever follows");
        assert_eq!(snappy_decompress(&trailing).unwrap(), text, "stops at the declared length");

        assert_eq!(snappy_decompress(&[3, 0x01, 0x05]), None, "copy before any output");
        assert_eq!(snappy_decompress(&enc[..enc.len() - 3]), None, "truncated");
        assert_eq!(snappy_decompress(&[0xff, 0xff, 0xff, 0xff, 0x7f]), None, "absurd length");

        let long = vec![b'x'; 1000];
        assert_eq!(snappy_decompress(&snappy_encode(&long, &[])).unwrap(), long);
    }

    pub fn leveldb_log(records: &[&[u8]]) -> Vec<u8> {
        const BLOCK: usize = 32 * 1024;
        let mut out: Vec<u8> = Vec::new();
        for record in records {
            let mut rest: &[u8] = record;
            let mut first = true;
            loop {
                let left = BLOCK - out.len() % BLOCK;
                if left < 7 {
                    out.extend(std::iter::repeat(0).take(left));
                    continue;
                }
                let take = rest.len().min(left - 7);
                let last = take == rest.len();
                let kind = match (first, last) {
                    (true, true) => 1,
                    (true, false) => 2,
                    (false, false) => 3,
                    (false, true) => 4,
                };
                out.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
                out.extend_from_slice(&(take as u16).to_le_bytes());
                out.push(kind);
                out.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                first = false;
                if last {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn log_payload_joins_records_split_across_blocks() {
        let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let log = leveldb_log(&[b"small one", &big, b"tail"]);
        assert!(log.len() > 2 * 32 * 1024);
        let payload = log_payload(&log);
        let mut expected = b"small one".to_vec();
        expected.extend_from_slice(&big);
        expected.extend_from_slice(b"tail");
        assert_eq!(payload, expected);
        assert_eq!(log_payload(&log[..log.len() - 2]), expected[..expected.len() - 4], "torn tail dropped");
    }

    fn put_varint(out: &mut Vec<u8>, mut n: u64) {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn block(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, value) in entries {
            put_varint(&mut out, 0);
            put_varint(&mut out, key.len() as u64);
            put_varint(&mut out, value.len() as u64);
            out.extend_from_slice(key);
            out.extend_from_slice(value);
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out
    }

    /// A minimal LevelDB table holding `value` in one data block.
    pub fn leveldb_table(value: &[u8], compress: bool) -> Vec<u8> {
        let mut file = Vec::new();
        let data = block(&[(b"key", value)]);
        let stored = if compress { snappy_encode(&data, &[]) } else { data };
        let data_handle = (file.len(), stored.len());
        file.extend_from_slice(&stored);
        file.push(u8::from(compress));
        file.extend_from_slice(&[0; 4]);

        let meta = block(&[]);
        let meta_handle = (file.len(), meta.len());
        file.extend_from_slice(&meta);
        file.extend_from_slice(&[0; 5]);

        let mut handle = Vec::new();
        put_varint(&mut handle, data_handle.0 as u64);
        put_varint(&mut handle, data_handle.1 as u64);
        let index = block(&[(b"kez", &handle)]);
        let index_handle = (file.len(), index.len());
        file.extend_from_slice(&index);
        file.extend_from_slice(&[0; 5]);

        let mut footer = Vec::new();
        for n in [meta_handle.0, meta_handle.1, index_handle.0, index_handle.1] {
            put_varint(&mut footer, n as u64);
        }
        footer.resize(40, 0);
        footer.extend_from_slice(&0xdb47_7524_8b80_fb57u64.to_le_bytes());
        file.extend_from_slice(&footer);
        file
    }

    #[test]
    fn table_payload_reads_plain_and_compressed_blocks() {
        for compress in [false, true] {
            let table = leveldb_table(b"the cached VALUE", compress);
            let payload = table_payload(&table).unwrap();
            assert!(find_bytes(&payload, b"the cached VALUE").is_some(), "compress={compress}");
        }
        assert_eq!(table_payload(b"not a table at all, far too short"), None);
    }
}
