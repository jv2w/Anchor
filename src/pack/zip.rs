//! The zip archives resource packs come in: the central directory read with every bound checked
//! before allocating, and stored or DEFLATE entries (PKWARE APPNOTE 6.3, the subset packs use).
//!
//! Zip64, multi-disk, encrypted and other compression methods are refused: the client must be
//! able to unpack what it downloads, and Anchor reads only what it can check. Entry names that
//! could leave the pack folder when unpacked (absolute, a `..` part, a drive letter) are refused
//! too, to protect the players who unpack the pack. A backslash counts as a folder separator
//! there: Windows PowerShell's `Compress-Archive` writes names with backslashes.

use crate::crypto::crc32;
use crate::protocol::compression::{Compression, Decompressor};

/// End of central directory record.
const END_SIGNATURE: u32 = 0x0605_4b50;
const END_LEN: usize = 22;
/// The zip64 end of central directory locator, just before the end record of a zip64 archive.
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP64_LOCATOR_LEN: usize = 20;
/// Central directory file header.
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const CENTRAL_LEN: usize = 46;
/// Local file header.
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const LOCAL_LEN: usize = 30;
/// The longest archive comment: its length is a u16.
const MAX_COMMENT: usize = 0xffff;
/// General purpose flags: encrypted, strongly encrypted.
const FLAG_ENCRYPTED: u16 = 1;
const FLAG_STRONG_ENCRYPTION: u16 = 1 << 6;
/// Compression methods.
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
/// The most folders an entry name may nest; packs use a handful.
const MAX_DEPTH: usize = 64;

/// What is wrong with an archive.
pub(super) type Fault = &'static str;

/// One file or folder of an archive.
#[derive(Debug, Clone, Copy)]
pub(super) struct Entry<'a> {
    /// The name as stored: `/`-separated, folders end with `/`.
    pub(super) name: &'a [u8],
    method: u16,
    crc: u32,
    /// The stored bytes.
    data: &'a [u8],
    /// The unpacked size.
    pub(super) size: u32,
}

/// Reads the central directory of `bytes`: every entry, in directory order, after checking that
/// each local header and its data lie inside the archive before the directory.
pub(super) fn entries(bytes: &[u8]) -> Result<Vec<Entry<'_>>, Fault> {
    let end = find_end(bytes)?;
    if end
        .checked_sub(ZIP64_LOCATOR_LEN)
        .is_some_and(|at| u32_at(bytes, at) == Some(ZIP64_LOCATOR_SIGNATURE))
    {
        return Err("zip64 archive");
    }
    let field16 = |at: usize| {
        end.checked_add(at)
            .and_then(|p| u16_at(bytes, p))
            .ok_or("truncated end record")
    };
    let field32 = |at: usize| {
        end.checked_add(at)
            .and_then(|p| u32_at(bytes, p))
            .ok_or("truncated end record")
    };
    let (disk, directory_disk) = (field16(4)?, field16(6)?);
    let (disk_entries, count) = (field16(8)?, field16(10)?);
    let (directory_len, directory_offset) = (field32(12)?, field32(16)?);
    if count == u16::MAX || directory_len == u32::MAX || directory_offset == u32::MAX {
        return Err("zip64 archive");
    }
    if disk != 0 || directory_disk != 0 || disk_entries != count {
        return Err("multi-disk archive");
    }
    let start = usize::try_from(directory_offset).map_err(|_| "directory out of bounds")?;
    let len = usize::try_from(directory_len).map_err(|_| "directory out of bounds")?;
    let directory = start
        .checked_add(len)
        .filter(|&stop| stop <= end)
        .and_then(|stop| bytes.get(start..stop))
        .ok_or("directory out of bounds")?;
    let count = usize::from(count);
    if count.saturating_mul(CENTRAL_LEN) > directory.len() {
        return Err("more entries than the directory holds");
    }
    let mut out = Vec::with_capacity(count);
    let mut at = 0;
    for _ in 0..count {
        let (entry, next) = central(bytes, directory, at, start)?;
        out.push(entry);
        at = next;
    }
    if at != directory.len() {
        return Err("directory size differs from its entries");
    }
    let mut names: Vec<&[u8]> = out.iter().map(|e| e.name).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w.first() == w.get(1)) {
        return Err("duplicate entry name");
    }
    Ok(out)
}

/// The offset of the end record: the last signature whose comment ends exactly at the end.
fn find_end(bytes: &[u8]) -> Result<usize, Fault> {
    let last = bytes
        .len()
        .checked_sub(END_LEN)
        .ok_or("not a zip archive")?;
    let first = last.saturating_sub(MAX_COMMENT);
    (first..=last)
        .rev()
        .find(|&at| {
            u32_at(bytes, at) == Some(END_SIGNATURE)
                && at
                    .checked_add(20)
                    .and_then(|field| u16_at(bytes, field))
                    .and_then(|comment| at.checked_add(END_LEN)?.checked_add(usize::from(comment)))
                    .is_some_and(|stop| stop == bytes.len())
        })
        .ok_or("not a zip archive")
}

/// The entry whose central header starts at `at` in `directory`, and the offset after it.
/// `directory_start` is where the directory starts in `bytes`: entry data must end before it.
fn central<'a>(
    bytes: &'a [u8],
    directory: &'a [u8],
    at: usize,
    directory_start: usize,
) -> Result<(Entry<'a>, usize), Fault> {
    const TRUNCATED: Fault = "truncated directory entry";
    let field16 = |off: usize| {
        at.checked_add(off)
            .and_then(|p| u16_at(directory, p))
            .ok_or(TRUNCATED)
    };
    let field32 = |off: usize| {
        at.checked_add(off)
            .and_then(|p| u32_at(directory, p))
            .ok_or(TRUNCATED)
    };
    if field32(0)? != CENTRAL_SIGNATURE {
        return Err("bad directory entry signature");
    }
    let flags = field16(8)?;
    if flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0 {
        return Err("encrypted entry");
    }
    let method = field16(10)?;
    if method != METHOD_STORED && method != METHOD_DEFLATE {
        return Err("unsupported compression method");
    }
    let (crc, compressed, size) = (field32(16)?, field32(20)?, field32(24)?);
    if compressed == u32::MAX || size == u32::MAX {
        return Err("zip64 entry");
    }
    if method == METHOD_STORED && compressed != size {
        return Err("stored entry sizes differ");
    }
    let name_len = usize::from(field16(28)?);
    let extra_len = usize::from(field16(30)?);
    let comment_len = usize::from(field16(32)?);
    let local = usize::try_from(field32(42)?).map_err(|_| "local header out of bounds")?;
    let name_start = at.checked_add(CENTRAL_LEN).ok_or(TRUNCATED)?;
    let name_end = name_start.checked_add(name_len).ok_or(TRUNCATED)?;
    let name = directory.get(name_start..name_end).ok_or(TRUNCATED)?;
    check_name(name)?;
    let next = name_end
        .checked_add(extra_len)
        .and_then(|n| n.checked_add(comment_len))
        .filter(|&n| n <= directory.len())
        .ok_or(TRUNCATED)?;
    let data = local_data(bytes, local, name, compressed, directory_start)?;
    let entry = Entry {
        name,
        method,
        crc,
        data,
        size,
    };
    Ok((entry, next))
}

/// The stored bytes of the entry whose local header is at `offset`, checked against the central
/// name and kept before the directory.
fn local_data<'a>(
    bytes: &'a [u8],
    offset: usize,
    name: &[u8],
    compressed: u32,
    directory_start: usize,
) -> Result<&'a [u8], Fault> {
    const OUT: Fault = "local entry out of bounds";
    let before = bytes.get(..directory_start).ok_or(OUT)?;
    if u32_at(before, offset) != Some(LOCAL_SIGNATURE) {
        return Err("bad local header signature");
    }
    let field16 = |off: usize| {
        offset
            .checked_add(off)
            .and_then(|p| u16_at(before, p))
            .ok_or(OUT)
    };
    let name_len = usize::from(field16(26)?);
    let extra_len = usize::from(field16(28)?);
    let name_start = offset.checked_add(LOCAL_LEN).ok_or(OUT)?;
    let name_end = name_start.checked_add(name_len).ok_or(OUT)?;
    if before.get(name_start..name_end) != Some(name) {
        return Err("local name differs from the directory");
    }
    let start = name_end.checked_add(extra_len).ok_or(OUT)?;
    let len = usize::try_from(compressed).map_err(|_| OUT)?;
    start
        .checked_add(len)
        .and_then(|stop| before.get(start..stop))
        .ok_or(OUT)
}

/// Refuses names that could leave the folder a client unpacks into, or nest without bound.
fn check_name(name: &[u8]) -> Result<(), Fault> {
    let separator = |b: &u8| matches!(b, b'/' | b'\\');
    let unsafe_byte = |b: &u8| matches!(b, 0 | b':');
    if name.is_empty() || name.first().is_some_and(separator) || name.iter().any(unsafe_byte) {
        return Err("unsafe entry name");
    }
    let mut depth = 0;
    for part in name.split(separator) {
        if part == b".." {
            return Err("unsafe entry name");
        }
        depth += 1;
    }
    if depth > MAX_DEPTH + 1 {
        return Err("entry name nests too deep");
    }
    Ok(())
}

/// The unpacked bytes of `entry`, whose size the caller has bounded, checked against its size
/// and CRC-32.
pub(super) fn read(entry: &Entry<'_>) -> Result<Vec<u8>, Fault> {
    let size = usize::try_from(entry.size).map_err(|_| "entry too large")?;
    let out = if entry.method == METHOD_STORED {
        entry.data.to_vec()
    } else {
        let mut out = Vec::new();
        Decompressor::new(Compression::Zlib)
            .decompress(entry.data, size, &mut out)
            .map_err(|_| "invalid deflate data")?;
        out
    };
    if out.len() != size {
        return Err("entry size differs from the directory");
    }
    if crc32(&[&out]) != entry.crc {
        return Err("entry crc-32 mismatch");
    }
    Ok(out)
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes(b.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// One file to write into a test archive.
    pub(crate) struct File<'a> {
        pub(crate) name: &'a str,
        pub(crate) data: &'a [u8],
        pub(crate) deflate: bool,
    }

    /// Writes a zip archive of `files` the way common tools do (no data descriptors).
    pub(crate) fn archive(files: &[File<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for file in files {
            let (method, stored) = if file.deflate {
                let mut packed = Vec::new();
                crate::protocol::compression::Compressor::new(Compression::Zlib, 6)
                    .compress(file.data, &mut packed);
                (METHOD_DEFLATE, packed)
            } else {
                (METHOD_STORED, file.data.to_vec())
            };
            let crc = crc32(&[file.data]);
            let name = file.name.as_bytes();
            let offset = u32::try_from(out.len()).unwrap();
            let common = |h: &mut Vec<u8>| {
                h.extend_from_slice(&20u16.to_le_bytes()); // version needed
                h.extend_from_slice(&0u16.to_le_bytes()); // flags
                h.extend_from_slice(&method.to_le_bytes());
                h.extend_from_slice(&[0; 4]); // time, date
                h.extend_from_slice(&crc.to_le_bytes());
                h.extend_from_slice(&u32::try_from(stored.len()).unwrap().to_le_bytes());
                h.extend_from_slice(&u32::try_from(file.data.len()).unwrap().to_le_bytes());
                h.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes()); // extra
            };
            out.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
            common(&mut out);
            out.extend_from_slice(name);
            out.extend_from_slice(&stored);
            directory.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            directory.extend_from_slice(&20u16.to_le_bytes()); // version made by
            common(&mut directory);
            directory.extend_from_slice(&[0; 6]); // comment, disk, internal attributes
            directory.extend_from_slice(&[0; 4]); // external attributes
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(name);
        }
        let offset = u32::try_from(out.len()).unwrap();
        let count = u16::try_from(files.len()).unwrap();
        out.extend_from_slice(&directory);
        out.extend_from_slice(&END_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // disks
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&u32::try_from(directory.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out
    }

    fn file<'a>(name: &'a str, data: &'a [u8], deflate: bool) -> File<'a> {
        File {
            name,
            data,
            deflate,
        }
    }

    /// The offset of the only central header of a one-file archive named `name`.
    fn central_at(zip: &[u8]) -> usize {
        zip.windows(4)
            .position(|w| w == CENTRAL_SIGNATURE.to_le_bytes())
            .unwrap()
    }

    #[test]
    fn stored_and_deflated_entries_read_back() {
        let text = b"{\"a\": 1}".repeat(100);
        let zip = archive(&[
            file("manifest.json", &text, true),
            file("textures/", b"", false),
            file("textures/a.png", b"png", false),
        ]);
        let entries = entries(&zip).unwrap();
        let names: Vec<&[u8]> = entries.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [&b"manifest.json"[..], b"textures/", b"textures/a.png"]
        );
        assert_eq!(read(&entries[0]).unwrap(), text);
        assert_eq!(read(&entries[2]).unwrap(), b"png");
    }

    #[test]
    fn an_archive_comment_is_skipped() {
        let mut zip = archive(&[file("a", b"x", false)]);
        let comment = b"made by a tool";
        let at = zip.len() - 2;
        zip[at..].copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(comment);
        assert_eq!(entries(&zip).unwrap().len(), 1);
        // A comment length that does not reach the end is not an end record.
        zip.push(0);
        assert_eq!(entries(&zip).unwrap_err(), "not a zip archive");
    }

    #[test]
    fn offsets_at_the_end_of_the_address_space_are_errors_not_overflows() {
        let zip = archive(&[file("a", b"x", false)]);
        for at in [usize::MAX, usize::MAX - CENTRAL_LEN + 1, usize::MAX - 30] {
            assert!(central(&zip, &zip, at, zip.len()).is_err(), "at {at}");
            assert!(local_data(&zip, at, b"a", 1, zip.len()).is_err(), "at {at}");
        }
    }

    #[test]
    fn every_truncation_is_an_error() {
        let zip = archive(&[file("manifest.json", b"{}", true), file("b", b"bb", false)]);
        for len in 0..zip.len() {
            assert!(entries(&zip[..len]).is_err(), "length {len}");
        }
    }

    #[test]
    fn zip64_multi_disk_encrypted_and_other_methods_are_refused() {
        let zip = archive(&[file("a", b"x", false)]);
        let end = zip.len() - END_LEN;
        let central = central_at(&zip);

        let mut z = zip.clone();
        z[end + 10..end + 12].copy_from_slice(&[0xff, 0xff]);
        assert_eq!(entries(&z).unwrap_err(), "zip64 archive");

        let mut z = zip.clone();
        z[end + 4] = 1;
        assert_eq!(entries(&z).unwrap_err(), "multi-disk archive");

        let mut z = zip.clone();
        z[central + 8] = 1;
        assert_eq!(entries(&z).unwrap_err(), "encrypted entry");

        let mut z = zip.clone();
        z[central + 10] = 14; // LZMA
        assert_eq!(entries(&z).unwrap_err(), "unsupported compression method");

        let mut z = zip.clone();
        z[central + 24..central + 28].copy_from_slice(&[0xff; 4]);
        assert_eq!(entries(&z).unwrap_err(), "zip64 entry");

        let mut z = zip.clone();
        let mut locator = ZIP64_LOCATOR_SIGNATURE.to_le_bytes().to_vec();
        locator.resize(ZIP64_LOCATOR_LEN, 0);
        z.splice(end..end, locator);
        assert_eq!(entries(&z).unwrap_err(), "zip64 archive");
    }

    #[test]
    fn oversized_counts_and_offsets_are_refused_before_allocating() {
        let zip = archive(&[file("a", b"x", false)]);
        let end = zip.len() - END_LEN;
        let mut z = zip.clone();
        z[end + 8..end + 12].copy_from_slice(&[0xfe, 0xff, 0xfe, 0xff]);
        assert_eq!(
            entries(&z).unwrap_err(),
            "more entries than the directory holds"
        );
        let mut z = zip.clone();
        z[end + 16..end + 20].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        assert_eq!(entries(&z).unwrap_err(), "directory out of bounds");
        let mut z = zip.clone();
        let central = central_at(&z);
        z[central + 42..central + 46].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(entries(&z).is_err());
        let mut z = zip;
        z[central + 20..central + 24].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        z[central + 24..central + 28].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        assert_eq!(entries(&z).unwrap_err(), "local entry out of bounds");
    }

    #[test]
    fn names_that_leave_the_pack_folder_are_refused() {
        for name in [
            "../manifest.json",
            "a/../../b",
            "/etc/passwd",
            "a\\..\\..\\b",
            "\\x",
            "C:x",
            "a\0b",
            "",
        ] {
            let zip = archive(&[file(name, b"x", false)]);
            assert_eq!(entries(&zip).unwrap_err(), "unsafe entry name", "{name:?}");
        }
        let deep = "a/".repeat(MAX_DEPTH + 1);
        let zip = archive(&[file(&deep, b"", false)]);
        assert_eq!(entries(&zip).unwrap_err(), "entry name nests too deep");
        let ok = "a/".repeat(MAX_DEPTH);
        let zip = archive(&[
            file(&ok, b"", false),
            file("a..b/.x", b"", false),
            file("textures\\a.png", b"", false),
        ]);
        assert_eq!(entries(&zip).unwrap().len(), 3);
    }

    #[test]
    fn duplicate_names_and_mismatched_local_names_are_refused() {
        let zip = archive(&[file("a", b"1", false), file("a", b"2", false)]);
        assert_eq!(entries(&zip).unwrap_err(), "duplicate entry name");
        let mut zip = archive(&[file("ab", b"1", false)]);
        zip[LOCAL_LEN] = b'x';
        assert_eq!(
            entries(&zip).unwrap_err(),
            "local name differs from the directory"
        );
    }

    #[test]
    fn corrupt_data_fails_its_size_or_crc_check() {
        let zip = archive(&[file("a", b"hello", false)]);
        let at = zip.windows(5).position(|w| w == b"hello").unwrap();
        let mut z = zip.clone();
        z[at] = b'j';
        let e = entries(&z).unwrap();
        assert_eq!(read(&e[0]).unwrap_err(), "entry crc-32 mismatch");

        let text = b"hello hello hello".repeat(10);
        let zip = archive(&[file("a", &text, true)]);
        let mut z = zip.clone();
        let central = central_at(&z);
        let size = u32::try_from(text.len() - 1).unwrap();
        z[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
        let e = entries(&z).unwrap();
        assert!(read(&e[0]).is_err());
    }

    #[test]
    fn mutated_archives_never_panic() {
        let base = archive(&[
            file("manifest.json", br#"{"format_version": 2}"#, true),
            file("textures/", b"", false),
            file("textures/a.png", &[7; 300], true),
        ]);
        let read_all = |bytes: &[u8]| {
            let list = entries(bytes).map_err(|_| crate::protocol::Error::Size)?;
            for entry in list.iter().filter(|e| e.size < 1 << 20) {
                read(entry).map_err(|_| crate::protocol::Error::Size)?;
            }
            Ok(())
        };
        let decoded = crate::protocol::fuzz(&base, 20_000, read_all);
        assert!(decoded > 0);
    }
}
