//! ZIP container of a .docx file: parse the archive, read entries under a
//! decompression budget, and write it back with every untouched entry's
//! content preserved.
//!
//! Everything is read through the central directory, which is
//! authoritative for names, methods, CRC-32s and sizes. Local headers are
//! only used to find each entry's compressed data. On write, rawzip
//! regenerates every header. Untouched entries keep their content, CRC-32
//! and size without being decompressed: stored ones are deflated (see
//! [`Package::write`] for why), and all others are copied verbatim,
//! compressed bytes included.

use std::{
    cmp::Ordering,
    io::{self, Write},
    iter, str,
};

use bytes::Bytes;
use miniz_oxide::{
    deflate::compress_to_vec,
    inflate::{TINFLStatus, decompress_slice_iter_to_slice},
};
use rawzip::{
    CompressionMethod, DataDescriptorOutput, ZipArchive, ZipArchiveWriter,
    path::{EntryPath, ZipFilePath},
    time::{UtcDateTime, ZipDateTimeKind},
};
use thiserror::Error;

// ── Limits ───────────────────────────────────────────────────────────────────

/// Number of bytes the reads that open one document may return in total:
/// the budget callers pass to [`Package::read`].
///
/// DEFLATE expands data by up to about 1032:1, so without a cap a few
/// kilobytes of input could claim gigabytes of memory (a zip bomb). 64 MiB
/// is several times the XML of a heavily formatted thousand-page document.
/// Only XML parts are read; the trees parsed from them are bounded
/// separately, by their number of nodes (see
/// [`NODE_BUDGET`](crate::xml::NODE_BUDGET)).
pub(crate) const DECOMPRESSION_BUDGET: usize = 64 * 1024 * 1024;

/// Longest entry name a ZIP archive can hold: its length is a 16-bit field
/// (APPNOTE §4.4.10).
pub(crate) const MAX_ENTRY_NAME_LEN: usize = 65_535;

/// DEFLATE level for replaced and appended entries (zlib's default).
const DEFLATE_LEVEL: u8 = 6;

/// 1980-01-01T00:00:00Z, the earliest MS-DOS timestamp, in seconds since
/// the Unix epoch: the modification time of appended entries. A fixed value
/// keeps the output independent of the clock.
const DOS_EPOCH: i64 = 315_532_800;

/// Implode's dictionary size and tree count options (APPNOTE §4.4.4,
/// bits 1 and 2 for method 6).
const FLAG_IMPLODE_OPTIONS: u16 = 0b0110;

/// "Compressed patched data" (APPNOTE §4.4.4, bit 5).
const FLAG_PATCHED_DATA: u16 = 1 << 5;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Error returned when a file is not a ZIP archive that can be safely read
/// and written back, or when one of its entries cannot be read.
///
/// Its message says what is wrong; a malformed archive also carries the
/// cause as its [`source`](std::error::Error::source).
#[derive(Debug, Error)]
#[error(transparent)]
#[non_exhaustive]
pub struct InvalidZip(#[from] Reason);

/// Why an archive or one of its entries was rejected.
///
/// Entry names are untrusted: messages print them with `{:?}` so control
/// characters come out escaped.
#[derive(Debug, Error)]
enum Reason {
    /// rawzip rejected the end record, the central directory or a local
    /// header, for example because the archive is truncated.
    #[error("malformed ZIP structure")]
    Zip(#[source] rawzip::Error),
    /// The central directory does not hold as many entries as the end
    /// record declares.
    #[error("central directory has {found} entries, the end record declares {declared}")]
    EntryCount { found: u64, declared: u64 },
    /// An entry name is not valid UTF-8 (shown lossily converted).
    #[error("entry name {0:?} is not valid UTF-8")]
    NonUtf8Name(String),
    /// The ZIP writer would not reproduce the name (see [`is_canonical`]).
    #[error("entry name {0:?} is not in canonical form")]
    NonCanonicalName(String),
    /// Two entry names are equal ignoring ASCII case.
    #[error("duplicate entry name {0:?}")]
    DuplicateName(String),
    /// The entry is encrypted, or the archive hides local header values.
    #[error("entry {0:?} is encrypted")]
    Encrypted(String),
    /// The entry relies on flags the ZIP writer cannot reproduce (see
    /// [`flags_survive_copy`]).
    #[error("entry {0:?} uses flags that cannot be written back")]
    UnsupportedFlags(String),
    /// A directory entry declares content.
    #[error("directory entry {0:?} has content")]
    DirectoryWithContent(String),
    /// The entry's bytes in the archive overlap another entry's.
    #[error("entry {0:?} overlaps another entry")]
    Overlap(String),
    /// The entry is neither stored nor deflated.
    #[error("entry {name:?} uses unsupported compression method {method}")]
    UnsupportedMethod { name: String, method: u16 },
    /// The entry's declared size exceeds what is left of the budget.
    #[error("entry {0:?} exceeds the decompression budget")]
    BudgetExceeded(String),
    /// The entry's DEFLATE stream is malformed or truncated.
    #[error("entry {0:?} has corrupt compressed data")]
    CorruptData(String),
    /// The entry's content is shorter or longer than declared.
    #[error("entry {0:?} does not match its declared size")]
    SizeMismatch(String),
    /// The entry's content does not match its declared CRC-32.
    #[error("entry {0:?} fails its CRC-32 check")]
    CrcMismatch(String),
}

// ── Package ──────────────────────────────────────────────────────────────────

/// A ZIP archive whose entries can be read and written back.
///
/// Entry names are kept exactly as stored in the archive and looked up
/// ignoring ASCII case, the way OPC compares part names (ECMA-376 Part 2,
/// M1.12). Clones share the original bytes.
#[derive(Clone)]
pub(crate) struct Package {
    /// Entries in central directory order.
    entries: Vec<Entry>,
    /// Indices into `entries`, sorted by name ignoring ASCII case.
    by_name: Vec<usize>,
}

/// One entry (file or directory) of a [`Package`].
#[derive(Clone)]
struct Entry {
    /// Name exactly as stored in the archive: valid UTF-8, canonical (see
    /// [`is_canonical`]), ending with `/` for directories.
    name: Box<str>,
    /// Compression method from the central directory.
    method: CompressionMethod,
    /// CRC-32 of the content, from the central directory.
    crc32: u32,
    /// Size of the content, from the central directory.
    uncompressed_size: u64,
    /// The compressed data, which shares the original archive.
    compressed: Bytes,
    /// Modification time to write back (see [`modification_time`]).
    modified: UtcDateTime,
}

impl Entry {
    /// Return whether this is a directory entry (its name ends with `/`).
    fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

impl Package {
    /// Parse a ZIP archive.
    ///
    /// Only the central directory and the local headers are inspected:
    /// nothing is decompressed until [`read`](Self::read).
    ///
    /// # Errors
    ///
    /// Returns [`InvalidZip`] when the archive is malformed or truncated,
    /// when an entry name is not valid UTF-8, is not canonical (see
    /// [`is_canonical`]) or equals another name ignoring ASCII case, when an
    /// entry is encrypted or relies on flags that cannot be written back,
    /// when a directory entry declares content, or when two entries share
    /// bytes of the archive.
    pub(crate) fn parse(data: impl Into<Bytes>) -> Result<Self, InvalidZip> {
        let entries = parse_entries(&data.into())?;
        let by_name = index_by_name(&entries)?;
        Ok(Self { entries, by_name })
    }

    /// Return whether an entry is named `name`, ignoring ASCII case.
    #[must_use]
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Iterate over the entry names (directories included) in archive
    /// order, spelled as stored in the archive.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| &*entry.name)
    }

    /// Decompress the entry named `name`, ignoring ASCII case, or return
    /// `None` if there is none.
    ///
    /// The content is checked against the CRC-32 and the size recorded in
    /// the central directory. The entry's declared size is deducted from
    /// `budget` before anything is decompressed, whether or not the read
    /// then succeeds: callers share one budget, normally
    /// [`DECOMPRESSION_BUDGET`], between all their reads. Nothing is
    /// deducted when there is no such entry. The content of a stored entry
    /// shares the original archive rather than copying it.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidZip`] when the entry is neither stored nor deflated,
    /// when its declared size exceeds `budget`, or when its content is
    /// corrupt or does not match its declared size and CRC-32.
    pub(crate) fn read(&self, name: &str, budget: &mut usize) -> Result<Option<Bytes>, InvalidZip> {
        let Some(index) = self.find(name) else {
            return Ok(None);
        };
        let entry = &self.entries[index];

        let method = entry.method;
        if method != CompressionMethod::STORE && method != CompressionMethod::DEFLATE {
            return Err(Reason::UnsupportedMethod {
                name: entry.name.to_string(),
                method: method.as_u16(),
            }
            .into());
        }

        // The declared size is untrusted: it is only used to size the
        // output buffer once it fits the budget, and the content must then
        // match it exactly.
        let size = usize::try_from(entry.uncompressed_size)
            .ok()
            .filter(|&size| size <= *budget)
            .ok_or_else(|| Reason::BudgetExceeded(entry.name.to_string()))?;
        // Charging before decompressing bounds the work of failing reads too.
        *budget -= size;

        let content = if method == CompressionMethod::STORE {
            if entry.compressed.len() != size {
                return Err(Reason::SizeMismatch(entry.name.to_string()).into());
            }
            entry.compressed.clone()
        } else {
            Bytes::from(inflate(&entry.name, &entry.compressed, size)?)
        };

        if rawzip::crc32(&content) != entry.crc32 {
            return Err(Reason::CrcMismatch(entry.name.to_string()).into());
        }
        Ok(Some(content))
    }

    /// Write the package to `out` as a ZIP archive and return `out`.
    ///
    /// Entries keep their order. An entry named in `replace` (ignoring
    /// ASCII case; the archive's spelling is kept) gets the new content,
    /// deflated, and names that match no entry are appended in the order
    /// given. Every other entry keeps its content, CRC-32 and size, and is
    /// never decompressed:
    ///
    /// - deflated entries, and entries using a method this crate cannot
    ///   read, are copied verbatim, compressed bytes and method included;
    /// - stored entries are deflated: rawzip sets the data descriptor flag
    ///   (APPNOTE §4.4.4, bit 3) on every file entry, and LibreOffice 25.2
    ///   refuses to load a .docx whose stored entries have one ("source
    ///   file could not be loaded"), although the same entries load without
    ///   the flag; Word may be as strict. A DEFLATE stream marks its own
    ///   end, so descriptors are harmless there. Stored bytes are the
    ///   content itself, so deflating them involves no decompression, only
    ///   work proportional to the input;
    /// - directory entries are written stored without data, which only
    ///   changes the encoding of an empty directory deflated by its
    ///   original writer.
    ///
    /// rawzip regenerates the headers: file entries always get a data
    /// descriptor, the UTF-8 flag is set when a name needs it, and extra
    /// fields, comments and attributes are not carried over. Modification
    /// times are kept (see [`modification_time`]); appended entries get a
    /// fixed one, so the output only depends on the package and `replace`.
    ///
    /// `out` receives many small writes: buffer it if they are expensive.
    ///
    /// # Errors
    ///
    /// Returns the error of `out` when writing to it fails, and an error of
    /// kind [`io::ErrorKind::InvalidInput`] when `replace` names a directory
    /// entry, names the same entry twice, or appends a name that is not
    /// canonical (see [`is_canonical`]), ends with `/` or is longer than
    /// [`MAX_ENTRY_NAME_LEN`] bytes. These are checked before anything is
    /// written. Nothing else can fail: writing to a `Vec<u8>` with a valid
    /// `replace` always succeeds.
    pub(crate) fn write<W: Write>(&self, out: W, replace: &[(String, Bytes)]) -> io::Result<W> {
        let plan = self.plan(replace)?;
        let mut archive = ZipArchiveWriter::builder()
            .with_capacity(self.entries.len() + plan.appended.len())
            .build(out);

        for (entry, replacement) in self.entries.iter().zip(plan.replaced) {
            let name = EntryPath::conformant(&*entry.name);
            if entry.is_dir() {
                archive
                    .new_dir(name)
                    .last_modified(entry.modified)
                    .create()
                    .map_err(io_error)?;
            } else if let Some(content) = replacement {
                write_deflated(
                    &mut archive,
                    name,
                    entry.modified,
                    content,
                    describe(content),
                )?;
            } else {
                // The central directory's CRC-32 and size are kept even when
                // they are wrong, so a damaged entry stays exactly as damaged.
                let descriptor = DataDescriptorOutput::new(entry.crc32, entry.uncompressed_size);
                if entry.method == CompressionMethod::STORE {
                    // Deflated rather than copied: see the method docs.
                    write_deflated(
                        &mut archive,
                        name,
                        entry.modified,
                        &entry.compressed,
                        descriptor,
                    )?;
                } else {
                    write_compressed(
                        &mut archive,
                        name,
                        entry.method,
                        entry.modified,
                        &entry.compressed,
                        descriptor,
                    )?;
                }
            }
        }

        let appended_time = UtcDateTime::from_unix(DOS_EPOCH);
        for (name, content) in plan.appended {
            write_deflated(
                &mut archive,
                EntryPath::conformant(name),
                appended_time,
                content,
                describe(content),
            )?;
        }
        archive.finish().map_err(io_error)
    }

    /// Return the index of the entry named `name`, ignoring ASCII case.
    fn find(&self, name: &str) -> Option<usize> {
        let position = self
            .by_name
            .binary_search_by(|&index| cmp_ignore_ascii_case(&self.entries[index].name, name))
            .ok()?;
        self.by_name.get(position).copied()
    }

    /// Match `replace` against the entries and validate it for
    /// [`write`](Self::write).
    fn plan<'a>(&self, replace: &'a [(String, Bytes)]) -> io::Result<Plan<'a>> {
        let mut replaced = vec![None; self.entries.len()];
        let mut appended = Vec::new();
        for (name, content) in replace {
            if let Some(index) = self.find(name) {
                if self.entries[index].is_dir() {
                    return Err(invalid_input(format!(
                        "cannot replace directory entry {name:?}"
                    )));
                }
                if replaced[index].replace(&content[..]).is_some() {
                    return Err(invalid_input(format!("entry {name:?} replaced twice")));
                }
            } else if name.len() > MAX_ENTRY_NAME_LEN {
                return Err(invalid_input(format!(
                    "entry name of {} bytes is too long",
                    name.len()
                )));
            } else if is_canonical(name) && !name.ends_with('/') {
                appended.push((name.as_str(), &content[..]));
            } else {
                return Err(invalid_input(format!("invalid entry name {name:?}")));
            }
        }

        let mut new_names: Vec<&str> = appended.iter().map(|&(name, _)| name).collect();
        new_names.sort_by(|a, b| cmp_ignore_ascii_case(a, b));
        for pair in new_names.windows(2) {
            if let [a, b] = *pair
                && a.eq_ignore_ascii_case(b)
            {
                return Err(invalid_input(format!("entry {b:?} appended twice")));
            }
        }

        Ok(Plan { replaced, appended })
    }
}

/// Read the central directory of `data` and validate every entry.
fn parse_entries(data: &Bytes) -> Result<Vec<Entry>, Reason> {
    let archive = ZipArchive::from_slice(&data[..]).map_err(Reason::Zip)?;
    let mut entries = Vec::new();
    // Bytes of the archive each entry occupies, from its local header to the
    // end of its compressed data, and the entry's index.
    let mut spans = Vec::new();

    let mut records = archive.entries();
    while let Some(record) = records.next_entry().map_err(Reason::Zip)? {
        let raw_name = record.file_path().as_bytes();
        let name = str::from_utf8(raw_name)
            .map_err(|_| Reason::NonUtf8Name(String::from_utf8_lossy(raw_name).into_owned()))?;
        if !is_canonical(name) {
            return Err(Reason::NonCanonicalName(name.to_owned()));
        }

        let flags = record.flags();
        if flags.is_encrypted() || flags.has_strong_encryption() || flags.is_masked() {
            return Err(Reason::Encrypted(name.to_owned()));
        }
        let method = record.compression_method();
        if !flags_survive_copy(method, flags.bits()) {
            return Err(Reason::UnsupportedFlags(name.to_owned()));
        }
        if record.is_dir() && record.uncompressed_size_hint() != 0 {
            return Err(Reason::DirectoryWithContent(name.to_owned()));
        }

        // Validates the local header and that all the compressed data is
        // present.
        let local = archive.get_entry(record.wayfinder()).map_err(Reason::Zip)?;
        let (_, data_end) = local.compressed_data_range();
        spans.push((record.local_header_offset(), data_end, entries.len()));
        entries.push(Entry {
            name: name.into(),
            method,
            crc32: record.crc32(),
            uncompressed_size: record.uncompressed_size_hint(),
            // rawzip reads the archive in place: the entry's data is a
            // subslice of `data`.
            compressed: data.slice_ref(local.data()),
            modified: modification_time(&record.last_modified()),
        });
    }

    // rawzip leaves checking the entry count to its callers.
    let found = entries.len() as u64;
    let declared = archive.entries_hint();
    if found != declared {
        return Err(Reason::EntryCount { found, declared });
    }
    check_disjoint(spans, &entries)?;
    Ok(entries)
}

/// Reject entries whose bytes overlap.
///
/// A crafted central directory can point any number of entries at the
/// same data; copying each of them would make the output arbitrarily
/// larger than the input. With disjoint entries, the output stays
/// proportional to the input.
fn check_disjoint(mut spans: Vec<(u64, u64, usize)>, entries: &[Entry]) -> Result<(), Reason> {
    spans.sort_unstable();
    let mut previous_end = 0;
    for (start, end, index) in spans {
        if start < previous_end {
            return Err(Reason::Overlap(entries[index].name.to_string()));
        }
        previous_end = end;
    }
    Ok(())
}

/// Sort entry indices by name ignoring ASCII case, rejecting names that are
/// equal ignoring ASCII case (ECMA-376 Part 2, M1.12).
fn index_by_name(entries: &[Entry]) -> Result<Vec<usize>, Reason> {
    let mut by_name: Vec<usize> = (0..entries.len()).collect();
    by_name.sort_by(|&a, &b| cmp_ignore_ascii_case(&entries[a].name, &entries[b].name));
    for pair in by_name.windows(2) {
        if let [a, b] = *pair
            && entries[a].name.eq_ignore_ascii_case(&entries[b].name)
        {
            return Err(Reason::DuplicateName(entries[b].name.to_string()));
        }
    }
    Ok(by_name)
}

/// Inflate raw DEFLATE data that must expand to exactly `size` bytes.
///
/// The output buffer is allocated once, at the declared size the caller
/// charged to the budget, and a stream that keeps going simply runs out of
/// room. `decompress_to_vec_with_limit` would not do: its buffer grows by
/// doubling and can reserve almost twice its limit.
fn inflate(name: &str, compressed: &[u8], size: usize) -> Result<Vec<u8>, Reason> {
    let mut content = vec![0; size];
    match decompress_slice_iter_to_slice(&mut content, iter::once(compressed), false, true) {
        Ok(written) if written == size => Ok(content),
        Ok(_) | Err(TINFLStatus::HasMoreOutput) => Err(Reason::SizeMismatch(name.to_owned())),
        Err(_) => Err(Reason::CorruptData(name.to_owned())),
    }
}

/// Return the modification time to write back for an entry.
///
/// rawzip's writer takes a UTC time, from which it fills both the MS-DOS
/// date and time fields (APPNOTE §4.4.6) and an extended timestamp extra
/// field (0x5455). A time read from an extended timestamp or NTFS extra
/// field is UTC already. A bare MS-DOS time is local time in an unknown
/// zone: as in rawzip's own example for copying modification times, its
/// fields are taken as UTC, so the MS-DOS fields come out unchanged (out of
/// range values are clamped) and the added extended timestamp assumes the
/// original zone was UTC. Either way the result only depends on the input.
///
/// The time is clamped to the range of the extended timestamp (1970 ..
/// 2106-02-07): rawzip clamps that field but not the MS-DOS fields, so a
/// later time would be written as two different instants, and rewriting
/// the output would not reproduce it.
fn modification_time(time: &ZipDateTimeKind) -> UtcDateTime {
    let time = match time {
        ZipDateTimeKind::Utc(time) => *time,
        ZipDateTimeKind::Local(time) => UtcDateTime::from_components(
            time.year(),
            time.month(),
            time.day(),
            time.hour(),
            time.minute(),
            time.second(),
            time.nanosecond(),
        )
        .unwrap_or_else(|| UtcDateTime::from_unix(DOS_EPOCH)),
    };
    UtcDateTime::from_unix(time.to_unix().clamp(0, i64::from(u32::MAX)))
}

/// Return whether an entry keeps its meaning when copied by rawzip's
/// writer, which only sets the data descriptor and UTF-8 flags.
///
/// Every other general purpose flag is dropped (APPNOTE §4.4.4). Bits 1
/// and 2 only hint at the compression level for DEFLATE, and LZMA's
/// end-of-stream bit is not needed when sizes are known, but Implode
/// needs bits 1 and 2 to decode, and "compressed patched data" changes
/// what the bytes mean whatever the method.
fn flags_survive_copy(method: CompressionMethod, flags: u16) -> bool {
    flags & FLAG_PATCHED_DATA == 0
        && (method != CompressionMethod::IMPLODED || flags & FLAG_IMPLODE_OPTIONS == 0)
}

// ── Writing ──────────────────────────────────────────────────────────────────

/// What [`Package::write`] does with each entry.
struct Plan<'a> {
    /// New content for each entry of the package, by index; `None` keeps
    /// the entry's content.
    replaced: Vec<Option<&'a [u8]>>,
    /// Entries added after the existing ones, in order.
    appended: Vec<(&'a str, &'a [u8])>,
}

/// Write a file entry whose compressed bytes are already known.
fn write_compressed<W: Write>(
    archive: &mut ZipArchiveWriter<W>,
    name: EntryPath<'_>,
    method: CompressionMethod,
    modified: UtcDateTime,
    compressed: &[u8],
    descriptor: DataDescriptorOutput,
) -> io::Result<()> {
    let (mut entry, _config) = archive
        .new_file(name)
        .compression_method(method)
        .last_modified(modified)
        .start()
        .map_err(io_error)?;
    entry.write_all(compressed)?;
    entry.finish(descriptor).map_err(io_error)?;
    Ok(())
}

/// Write a file entry holding `content`, deflated, and declaring the
/// CRC-32 and size in `descriptor`.
fn write_deflated<W: Write>(
    archive: &mut ZipArchiveWriter<W>,
    name: EntryPath<'_>,
    modified: UtcDateTime,
    content: &[u8],
    descriptor: DataDescriptorOutput,
) -> io::Result<()> {
    let compressed = compress_to_vec(content, DEFLATE_LEVEL);
    write_compressed(
        archive,
        name,
        CompressionMethod::DEFLATE,
        modified,
        &compressed,
        descriptor,
    )
}

/// Return the CRC-32 and size of new content.
fn describe(content: &[u8]) -> DataDescriptorOutput {
    DataDescriptorOutput::new(rawzip::crc32(content), content.len() as u64)
}

/// Convert a rawzip error to an [`io::Error`], returning errors of the
/// underlying writer unchanged.
fn io_error(err: rawzip::Error) -> io::Error {
    match err.into_kind() {
        rawzip::ErrorKind::IO(err) => err,
        kind => io::Error::new(io::ErrorKind::InvalidInput, rawzip::Error::from(kind)),
    }
}

/// Build an [`io::ErrorKind::InvalidInput`] error.
fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

// ── Names ────────────────────────────────────────────────────────────────────

/// Return whether rawzip's writer reproduces `name` byte for byte.
///
/// Names are written with [`EntryPath::conformant`], the only mode that
/// sets the UTF-8 flag (APPNOTE §4.4.4, bit 11) when a name needs it:
/// [`EntryPath::verbatim`] keeps the bytes but never sets the flag, which
/// would turn `word/media/immagine-è.png` into CP437 mojibake for readers
/// that honour it. `conformant` also normalizes the path, though:
/// backslashes become slashes, empty, `.` and `..` segments and leading
/// slashes are dropped, and so is everything up to a `:`. Rather than
/// mixing the two modes, names that normalization would change are
/// rejected: apart from names containing a `:`, which OPC allows but which
/// are unheard of in practice, none of them is a valid OPC part name or
/// item name. Asking rawzip itself keeps this check in step with the
/// writer.
fn is_canonical(name: &str) -> bool {
    ZipFilePath::from_str(name).as_str() == name
}

/// Compare names ignoring ASCII case, as OPC compares part names.
fn cmp_ignore_ascii_case(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase()))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[expect(clippy::cast_possible_truncation, reason = "test archives are small")]
mod tests {
    use std::{
        error::Error as _,
        io::{Cursor, Read},
    };

    use miniz_oxide::inflate::decompress_to_vec;

    use super::*;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Pack an MS-DOS date (APPNOTE §4.4.6).
    const fn dos_date(year: u16, month: u16, day: u16) -> u16 {
        ((year - 1980) << 9) | (month << 5) | day
    }

    /// Pack an MS-DOS time (APPNOTE §4.4.6), which has 2-second precision.
    const fn dos_time(hour: u16, minute: u16, second: u16) -> u16 {
        (hour << 11) | (minute << 5) | (second / 2)
    }

    /// MS-DOS date and time of every fixture entry.
    const DOS_DATE: u16 = dos_date(2024, 5, 17);
    const DOS_TIME: u16 = dos_time(10, 30, 44);

    const FLAG_ENCRYPTED: u16 = 1;
    const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
    const FLAG_UTF8: u16 = 1 << 11;

    const METHOD_STORE: u16 = 0;
    const METHOD_IMPLODE: u16 = 6;
    const METHOD_DEFLATE: u16 = 8;
    const METHOD_BZIP2: u16 = 12;

    const CONTENT_TYPES: &[u8] = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\r\n",
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
        r#"<Default Extension="xml" ContentType="application/xml"/></Types>"#,
    )
    .as_bytes();
    const DOCUMENT: &[u8] = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\r\n",
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        "<w:body><w:p><w:r><w:t>Hello {{name}}</w:t></w:r></w:p></w:body></w:document>",
    )
    .as_bytes();
    const RELS: &[u8] = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\r\n",
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#,
    )
    .as_bytes();
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR not really an image";

    /// One entry, laid out by [`build_zip`] exactly as described.
    #[derive(Clone)]
    struct Fixture {
        name: Vec<u8>,
        method: u16,
        flags: u16,
        crc32: u32,
        compressed: Vec<u8>,
        uncompressed_size: u32,
        /// What the entry should read back as.
        content: Vec<u8>,
    }

    impl Fixture {
        fn stored(name: &str, content: &[u8]) -> Self {
            Self {
                name: name.as_bytes().to_vec(),
                method: METHOD_STORE,
                flags: if name.is_ascii() { 0 } else { FLAG_UTF8 },
                crc32: rawzip::crc32(content),
                compressed: content.to_vec(),
                uncompressed_size: content.len() as u32,
                content: content.to_vec(),
            }
        }

        fn deflated(name: &str, content: &[u8]) -> Self {
            Self {
                method: METHOD_DEFLATE,
                compressed: compress_to_vec(content, 9),
                ..Self::stored(name, content)
            }
        }

        fn directory(name: &str) -> Self {
            Self::stored(name, b"")
        }

        fn with_flags(mut self, flags: u16) -> Self {
            self.flags |= flags;
            self
        }

        fn name(&self) -> &str {
            str::from_utf8(&self.name).unwrap()
        }
    }

    fn put_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn put_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn u16_at(zip: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(zip[offset..offset + 2].try_into().unwrap())
    }

    fn u32_at(zip: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(zip[offset..offset + 4].try_into().unwrap())
    }

    /// Lay out `entries` as a ZIP archive (APPNOTE §4.3): local headers,
    /// each followed by a data descriptor when flag bit 3 is set, then the
    /// central directory and the end record.
    fn build_zip(entries: &[Fixture]) -> Vec<u8> {
        let mut zip = Vec::new();
        let mut offsets = Vec::new();
        for entry in entries {
            offsets.push(zip.len() as u32);
            let descriptor = entry.flags & FLAG_DATA_DESCRIPTOR != 0;
            put_u32(&mut zip, 0x0403_4b50);
            put_u16(&mut zip, 20); // version needed to extract
            put_u16(&mut zip, entry.flags);
            put_u16(&mut zip, entry.method);
            put_u16(&mut zip, DOS_TIME);
            put_u16(&mut zip, DOS_DATE);
            if descriptor {
                // Deferred to the data descriptor (APPNOTE §4.4.4).
                zip.extend_from_slice(&[0; 12]);
            } else {
                put_u32(&mut zip, entry.crc32);
                put_u32(&mut zip, entry.compressed.len() as u32);
                put_u32(&mut zip, entry.uncompressed_size);
            }
            put_u16(&mut zip, entry.name.len() as u16);
            put_u16(&mut zip, 0); // extra field length
            zip.extend_from_slice(&entry.name);
            zip.extend_from_slice(&entry.compressed);
            if descriptor {
                put_u32(&mut zip, 0x0807_4b50);
                put_u32(&mut zip, entry.crc32);
                put_u32(&mut zip, entry.compressed.len() as u32);
                put_u32(&mut zip, entry.uncompressed_size);
            }
        }

        let central_directory = zip.len();
        for (entry, offset) in entries.iter().zip(offsets) {
            put_u32(&mut zip, 0x0201_4b50);
            put_u16(&mut zip, 20); // version made by: MS-DOS, 2.0
            put_u16(&mut zip, 20); // version needed to extract
            put_u16(&mut zip, entry.flags);
            put_u16(&mut zip, entry.method);
            put_u16(&mut zip, DOS_TIME);
            put_u16(&mut zip, DOS_DATE);
            put_u32(&mut zip, entry.crc32);
            put_u32(&mut zip, entry.compressed.len() as u32);
            put_u32(&mut zip, entry.uncompressed_size);
            put_u16(&mut zip, entry.name.len() as u16);
            put_u16(&mut zip, 0); // extra field length
            put_u16(&mut zip, 0); // file comment length
            put_u16(&mut zip, 0); // disk number start
            put_u16(&mut zip, 0); // internal file attributes
            let directory = entry.name.ends_with(b"/");
            put_u32(&mut zip, if directory { 0x10 } else { 0 }); // MS-DOS attributes
            put_u32(&mut zip, offset);
            zip.extend_from_slice(&entry.name);
        }

        let central_directory_size = zip.len() - central_directory;
        put_u32(&mut zip, 0x0605_4b50);
        put_u16(&mut zip, 0); // number of this disk
        put_u16(&mut zip, 0); // disk where the central directory starts
        put_u16(&mut zip, entries.len() as u16); // entries on this disk
        put_u16(&mut zip, entries.len() as u16); // total entries
        put_u32(&mut zip, central_directory_size as u32);
        put_u32(&mut zip, central_directory as u32);
        put_u16(&mut zip, 0); // comment length
        zip
    }

    /// A .docx-shaped archive mixing stored, deflated and directory
    /// entries, a non-ASCII name and entries with data descriptors.
    fn sample_entries() -> Vec<Fixture> {
        vec![
            Fixture::deflated("[Content_Types].xml", CONTENT_TYPES),
            Fixture::deflated("_rels/.rels", RELS),
            Fixture::directory("word/"),
            Fixture::deflated("word/document.xml", DOCUMENT).with_flags(FLAG_DATA_DESCRIPTOR),
            Fixture::stored("word/_rels/document.xml.rels", RELS),
            Fixture::directory("word/media/"),
            Fixture::stored("word/media/immagine-è.png", PNG),
            Fixture::stored("docProps/custom.xml", b"").with_flags(FLAG_DATA_DESCRIPTOR),
        ]
    }

    fn sample() -> Vec<u8> {
        build_zip(&sample_entries())
    }

    fn parse(zip: Vec<u8>) -> Package {
        Package::parse(zip).unwrap()
    }

    fn rejection(zip: Vec<u8>) -> Reason {
        match Package::parse(zip) {
            Ok(_) => panic!("archive accepted"),
            Err(err) => err.0,
        }
    }

    /// Read the entry `name`, which must exist, with a budget of its own.
    fn read(package: &Package, name: &str) -> Result<Bytes, InvalidZip> {
        let mut budget = DECOMPRESSION_BUDGET;
        let content = package.read(name, &mut budget)?;
        Ok(content.unwrap_or_else(|| panic!("no entry named {name:?}")))
    }

    fn write(package: &Package, replace: &[(String, Bytes)]) -> Vec<u8> {
        package.write(Vec::new(), replace).unwrap()
    }

    /// One entry as rawzip reads it back.
    #[derive(Debug, PartialEq, Eq)]
    struct RawEntry {
        name: Vec<u8>,
        method: u16,
        crc32: u32,
        uncompressed_size: u64,
        compressed: Vec<u8>,
    }

    fn raw_entries(zip: &[u8]) -> Vec<RawEntry> {
        let archive = ZipArchive::from_slice(zip).unwrap();
        let mut records = archive.entries();
        let mut entries = Vec::new();
        while let Some(record) = records.next_entry().unwrap() {
            let local = archive.get_entry(record.wayfinder()).unwrap();
            entries.push(RawEntry {
                name: record.file_path().as_bytes().to_vec(),
                method: record.compression_method().as_u16(),
                crc32: record.crc32(),
                uncompressed_size: record.uncompressed_size_hint(),
                compressed: local.data().to_vec(),
            });
        }
        entries
    }

    /// Name and content of every entry, read with the independent `zip`
    /// crate (which checks each CRC-32 as it reads).
    fn read_with_zip_crate(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut archive = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        (0..archive.len())
            .map(|index| {
                let mut file = archive.by_index(index).unwrap();
                let name = file.name().unwrap().into_owned();
                let mut content = Vec::new();
                file.read_to_end(&mut content).unwrap();
                (name, content)
            })
            .collect()
    }

    fn expected_files(entries: &[Fixture]) -> Vec<(String, Vec<u8>)> {
        entries
            .iter()
            .map(|entry| (entry.name().to_owned(), entry.content.clone()))
            .collect()
    }

    /// Assert that `after` is `before` written back untouched: byte for
    /// byte, except that a stored file comes out deflated with the same
    /// content, CRC-32 and size.
    fn assert_kept(before: &RawEntry, after: &RawEntry) {
        if before.method == METHOD_STORE && !before.name.ends_with(b"/") {
            let name = String::from_utf8_lossy(&before.name);
            assert_eq!(after.name, before.name);
            assert_eq!(after.method, METHOD_DEFLATE, "{name}");
            assert_eq!(after.crc32, before.crc32, "{name}");
            assert_eq!(after.uncompressed_size, before.uncompressed_size, "{name}");
            assert_eq!(
                decompress_to_vec(&after.compressed).unwrap(),
                before.compressed,
                "{name}"
            );
        } else {
            assert_eq!(after, before);
        }
    }

    /// Assert that `output` holds the entries of `input`, written back
    /// untouched (see [`assert_kept`]), in the same order.
    fn assert_all_kept(input: &[u8], output: &[u8]) {
        let before = raw_entries(input);
        let after = raw_entries(output);
        assert_eq!(after.len(), before.len());
        for (before, after) in before.iter().zip(&after) {
            assert_kept(before, after);
        }
    }

    /// Assert that no entry of `zip` is stored with a data descriptor
    /// (flag bit 3), neither in its central directory record nor in its
    /// local header, which is read from the raw bytes.
    fn assert_no_stored_entry_with_descriptor(zip: &[u8]) {
        let archive = ZipArchive::from_slice(zip).unwrap();
        let mut records = archive.entries();
        while let Some(record) = records.next_entry().unwrap() {
            let name = String::from_utf8_lossy(record.file_path().as_bytes()).into_owned();
            let central = (record.compression_method().as_u16(), record.flags().bits());
            let local_header = record.local_header_offset() as usize;
            assert_eq!(u32_at(zip, local_header), 0x0403_4b50, "{name}");
            let local = (u16_at(zip, local_header + 8), u16_at(zip, local_header + 6));
            for (method, flags) in [central, local] {
                assert!(
                    method != METHOD_STORE || flags & FLAG_DATA_DESCRIPTOR == 0,
                    "{name}: stored with a data descriptor"
                );
            }
        }
    }

    // ── Round trips ──────────────────────────────────────────────────────────

    #[test]
    fn writing_without_replacements_keeps_every_entry() {
        let input = sample();
        let output = write(&parse(input.clone()), &[]);

        // Same names, order, CRC-32s and sizes. Deflated entries keep their
        // compressed bytes; stored files come out deflated.
        assert_all_kept(&input, &output);
        assert_eq!(
            read_with_zip_crate(&output),
            expected_files(&sample_entries())
        );
        let package = parse(output);
        for entry in sample_entries() {
            assert_eq!(read(&package, entry.name()).unwrap(), entry.content);
        }
    }

    #[test]
    fn no_entry_is_written_stored_with_a_data_descriptor() {
        // All stored, as the `zip` crate writes them, with and without a
        // data descriptor, plus an empty file and a directory.
        let entries = [
            Fixture::stored("[Content_Types].xml", CONTENT_TYPES),
            Fixture::directory("word/"),
            Fixture::stored("word/document.xml", DOCUMENT),
            Fixture::stored("word/media/image1.png", PNG).with_flags(FLAG_DATA_DESCRIPTOR),
            Fixture::stored("docProps/custom.xml", b""),
        ];
        let input = build_zip(&entries);
        let package = parse(input.clone());
        let replace = [
            (
                "word/document.xml".to_owned(),
                Bytes::from_static(b"<w:document/>"),
            ),
            (
                "customXml/item1.xml".to_owned(),
                Bytes::from_static(b"<item/>"),
            ),
        ];
        for replace in [&[][..], &replace] {
            assert_no_stored_entry_with_descriptor(&write(&package, replace));
        }
        assert_all_kept(&input, &write(&package, &[]));
    }

    #[test]
    fn damaged_stored_entries_stay_exactly_as_damaged() {
        // The central directory's CRC-32 and size are carried over to the
        // deflated copy, so readers reject it just as they did the original.
        let bad_crc = Fixture {
            crc32: rawzip::crc32(b"hello") ^ 1,
            ..Fixture::stored("bad-crc.xml", b"hello")
        };
        let too_short = Fixture {
            uncompressed_size: 4,
            ..Fixture::stored("too-short.xml", b"hello")
        };
        let too_long = Fixture {
            uncompressed_size: 6,
            ..Fixture::stored("too-long.xml", b"hello")
        };
        let input = build_zip(&[bad_crc, too_short, too_long]);
        let output = write(&parse(input.clone()), &[]);
        assert_all_kept(&input, &output);

        let package = parse(output);
        let err = read(&package, "bad-crc.xml").unwrap_err();
        assert!(matches!(err.0, Reason::CrcMismatch(_)));
        for name in ["too-short.xml", "too-long.xml"] {
            let err = read(&package, name).unwrap_err();
            assert!(matches!(err.0, Reason::SizeMismatch(_)), "{name}");
        }
    }

    #[test]
    fn names_come_out_byte_identical_with_a_correct_utf8_flag() {
        let entries = [
            Fixture::stored("[Content_Types].xml", CONTENT_TYPES),
            Fixture::stored("word/_rels/document.xml.rels", RELS),
            Fixture::directory("word/media/"),
            // Some writers store UTF-8 names without setting the flag.
            Fixture {
                flags: 0,
                ..Fixture::stored("word/media/immagine-è.png", PNG)
            },
            Fixture::directory("word/media/グラフ/"),
        ];
        let output = write(&parse(build_zip(&entries)), &[]);

        let archive = ZipArchive::from_slice(&output).unwrap();
        let mut records = archive.entries();
        for entry in &entries {
            let record = records.next_entry().unwrap().unwrap();
            assert_eq!(record.file_path().as_bytes(), entry.name);
            assert_eq!(record.flags().is_utf8(), !entry.name.is_ascii());
            assert_eq!(record.is_dir(), entry.name.ends_with(b"/"));
        }
        assert!(records.next_entry().unwrap().is_none());
        assert_eq!(read_with_zip_crate(&output), expected_files(&entries));
    }

    #[test]
    fn replaced_entries_get_new_content_and_new_names_are_appended() {
        let input = sample();
        let new_document = Bytes::from_static(b"<w:document>replaced</w:document>");
        let replace = [
            // Lookups ignore ASCII case; the archive's spelling is kept.
            ("WORD/Document.XML".to_owned(), new_document.clone()),
            ("word/media/image1.png".to_owned(), Bytes::from_static(PNG)),
            (
                "customXml/item1.xml".to_owned(),
                Bytes::from_static(b"<item/>"),
            ),
        ];
        let output = write(&parse(input.clone()), &replace);

        let mut expected = expected_files(&sample_entries());
        expected[3].1 = new_document.to_vec();
        expected.push(("word/media/image1.png".to_owned(), PNG.to_vec()));
        expected.push(("customXml/item1.xml".to_owned(), b"<item/>".to_vec()));
        assert_eq!(read_with_zip_crate(&output), expected);

        let before = raw_entries(&input);
        let after = raw_entries(&output);
        assert_eq!(after.len(), before.len() + 2);
        for (before, after) in before.iter().zip(&after) {
            if before.name == b"word/document.xml" {
                assert_eq!(after.name, before.name);
                assert_eq!(after.method, METHOD_DEFLATE);
            } else {
                assert_kept(before, after);
            }
        }
        assert!(
            after[8..]
                .iter()
                .all(|entry| entry.method == METHOD_DEFLATE)
        );
    }

    #[test]
    fn modification_times_are_kept_and_appended_entries_get_a_fixed_one() {
        let replace = [
            (
                "word/document.xml".to_owned(),
                Bytes::from_static(b"<w:document/>"),
            ),
            ("new.xml".to_owned(), Bytes::from_static(b"<new/>")),
        ];
        let output = write(&parse(sample()), &replace);

        let archive = ZipArchive::from_slice(&output).unwrap();
        let mut records = archive.entries();
        let mut times = Vec::new();
        while let Some(record) = records.next_entry().unwrap() {
            let dos = record.last_modified_dos();
            times.push((dos.packed_date(), dos.packed_time()));
        }
        let (appended, kept) = times.split_last().unwrap();
        assert!(kept.iter().all(|&time| time == (DOS_DATE, DOS_TIME)));
        assert_eq!(*appended, (dos_date(1980, 1, 1), dos_time(0, 0, 0)));
    }

    #[test]
    fn output_is_deterministic() {
        let package = parse(sample());
        let replace = [
            (
                "word/document.xml".to_owned(),
                Bytes::from_static(b"<w:document/>"),
            ),
            ("new.xml".to_owned(), Bytes::from_static(b"<new/>")),
        ];
        let output = write(&package, &replace);
        assert_eq!(write(&package, &replace), output);
        assert_eq!(write(&package.clone(), &replace), output);

        // Rewriting the output reproduces it byte for byte: modification
        // times survive through the extended timestamp field.
        assert_eq!(write(&parse(output.clone()), &[]), output);
    }

    #[test]
    fn times_past_the_extended_timestamp_range_are_rewritten_identically() {
        // The MS-DOS fields reach 2107-12-31, the extended timestamp only
        // 2106-02-07: the first write must already settle on one instant.
        let mut input = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let time = zip::DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58).unwrap();
        let options = zip::write::SimpleFileOptions::default().last_modified_time(time);
        input.start_file("word/document.xml", options).unwrap();
        input.write_all(DOCUMENT).unwrap();
        let input = input.finish().unwrap().into_inner();

        let output = write(&parse(input), &[]);
        assert_eq!(write(&parse(output.clone()), &[]), output);
    }

    #[test]
    fn data_before_the_archive_is_dropped() {
        // rawzip shifts every offset when the central directory is found
        // further than declared, as with self-extracting archives.
        let mut input = b"#!/bin/sh\nexit 0\n".to_vec();
        input.extend_from_slice(&sample());
        assert_eq!(write(&parse(input), &[]), write(&parse(sample()), &[]));
    }

    #[test]
    fn offsets_that_overflow_when_shifted_are_rejected() {
        // A ZIP64 extra field (APPNOTE §4.5.3) puts the local header at
        // offset u64::MAX, and data before the archive makes rawzip shift
        // that offset further. rawzip 0.5.0 and 0.5.1 panicked on the
        // overflow; 0.5.2 reports a truncated archive.
        let mut zip = Vec::new();
        put_u32(&mut zip, 0x0201_4b50);
        put_u16(&mut zip, 45); // version made by: MS-DOS, 4.5
        put_u16(&mut zip, 45); // version needed to extract: ZIP64
        put_u16(&mut zip, 0); // flags
        put_u16(&mut zip, METHOD_STORE);
        put_u16(&mut zip, DOS_TIME);
        put_u16(&mut zip, DOS_DATE);
        put_u32(&mut zip, rawzip::crc32(b""));
        put_u32(&mut zip, 0); // compressed size
        put_u32(&mut zip, 0); // uncompressed size
        put_u16(&mut zip, 1); // file name length
        put_u16(&mut zip, 12); // extra field length
        put_u16(&mut zip, 0); // file comment length
        put_u16(&mut zip, 0); // disk number start
        put_u16(&mut zip, 0); // internal file attributes
        put_u32(&mut zip, 0); // MS-DOS attributes
        put_u32(&mut zip, u32::MAX); // local header offset: in the extra field
        zip.push(b'a');
        put_u16(&mut zip, 0x0001); // ZIP64 extended information
        put_u16(&mut zip, 8); // its size
        zip.extend_from_slice(&u64::MAX.to_le_bytes()); // local header offset
        let central_directory_size = zip.len() as u32;
        put_u32(&mut zip, 0x0605_4b50);
        put_u16(&mut zip, 0); // number of this disk
        put_u16(&mut zip, 0); // disk where the central directory starts
        put_u16(&mut zip, 1); // entries on this disk
        put_u16(&mut zip, 1); // total entries
        put_u32(&mut zip, central_directory_size);
        put_u32(&mut zip, 0); // offset of the central directory
        put_u16(&mut zip, 0); // comment length

        let mut input = b"prefix".to_vec();
        input.extend_from_slice(&zip);
        let reason = rejection(input);
        assert!(matches!(reason, Reason::Zip(_)), "{reason}");
    }

    #[test]
    fn other_methods_are_copied_verbatim_but_cannot_be_read() {
        let bzip2 = Fixture {
            method: METHOD_BZIP2,
            compressed: b"BZh91AY&SY not really bzip2".to_vec(),
            ..Fixture::stored("word/media/image1.emf", b"EMF")
        };
        let input = build_zip(&[Fixture::deflated("word/document.xml", DOCUMENT), bzip2]);
        let package = parse(input.clone());

        let mut budget = DECOMPRESSION_BUDGET;
        let err = package
            .read("word/media/image1.emf", &mut budget)
            .unwrap_err();
        assert!(matches!(
            err.0,
            Reason::UnsupportedMethod { method: 12, .. }
        ));
        assert_eq!(budget, DECOMPRESSION_BUDGET, "nothing charged");
        assert_eq!(raw_entries(&write(&package, &[])), raw_entries(&input));
    }

    #[test]
    fn deflated_directory_entries_come_out_stored() {
        // Java's ZipOutputStream deflates directory entries into a
        // two-byte empty stream.
        let directory = Fixture::deflated("word/", b"");
        assert_eq!(directory.compressed.len(), 2);
        let output = write(&parse(build_zip(&[directory])), &[]);

        let entries = raw_entries(&output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, b"word/");
        assert_eq!(entries[0].method, METHOD_STORE);
        assert_eq!(entries[0].compressed, b"");
        let mut archive = zip::ZipArchive::new(Cursor::new(output)).unwrap();
        assert!(archive.by_index(0).unwrap().is_dir());
    }

    #[test]
    fn deflate_compression_level_hints_are_harmless() {
        let entry = Fixture::deflated("word/document.xml", DOCUMENT).with_flags(0b0010);
        let package = parse(build_zip(&[entry]));
        assert_eq!(read(&package, "word/document.xml").unwrap(), DOCUMENT);
    }

    // ── Reading ──────────────────────────────────────────────────────────────

    #[test]
    fn every_kind_of_entry_reads_back() {
        let package = parse(sample());
        for entry in sample_entries() {
            assert_eq!(read(&package, entry.name()).unwrap(), entry.content);
        }
    }

    #[test]
    fn lookups_ignore_ascii_case_only() {
        let package = parse(sample());
        assert!(package.contains("WORD/DOCUMENT.XML"));
        assert!(package.contains("[content_types].XML"));
        assert!(package.contains("Word/Media/IMMAGINE-è.PNG"));
        assert!(!package.contains("word/media/immagine-È.png"));
        assert!(!package.contains("word/document.xm"));
        assert!(!package.contains(""));
        assert_eq!(read(&package, "Word/Document.Xml").unwrap(), DOCUMENT);
    }

    #[test]
    fn names_are_listed_in_archive_order() {
        let package = parse(sample());
        let expected: Vec<String> = sample_entries()
            .iter()
            .map(|entry| entry.name().to_owned())
            .collect();
        assert_eq!(package.names().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn reading_a_missing_entry_gives_nothing() {
        let package = parse(sample());
        let mut budget = DECOMPRESSION_BUDGET;
        let content = package.read("word/missing.xml", &mut budget).unwrap();
        assert!(content.is_none());
        assert_eq!(budget, DECOMPRESSION_BUDGET, "nothing charged");
    }

    /// Four MiB of zeros, which DEFLATE squeezes into about 4 KiB.
    fn zeros() -> Vec<u8> {
        vec![0; 4 << 20]
    }

    #[test]
    fn zip_bomb_is_rejected_before_inflating() {
        let bomb = Fixture::deflated("word/document.xml", &zeros());
        assert!(bomb.compressed.len() < 8 << 10);
        let package = parse(build_zip(&[bomb]));

        let mut budget = 1 << 20;
        let err = package.read("word/document.xml", &mut budget).unwrap_err();
        assert!(matches!(err.0, Reason::BudgetExceeded(_)));
        assert_eq!(budget, 1 << 20, "nothing charged");
    }

    #[test]
    fn zip_bomb_lying_about_its_size_stops_at_the_declared_size() {
        // The central directory claims 1 KiB: that is all that gets
        // allocated and charged before the stream runs out of room.
        let bomb = Fixture {
            uncompressed_size: 1024,
            ..Fixture::deflated("word/document.xml", &zeros())
        };
        let package = parse(build_zip(&[bomb]));

        let mut budget = 1 << 20;
        let err = package.read("word/document.xml", &mut budget).unwrap_err();
        assert!(matches!(err.0, Reason::SizeMismatch(_)));
        assert_eq!(budget, (1 << 20) - 1024);
    }

    #[test]
    fn budget_is_shared_by_the_reads_given_it() {
        let content = vec![b'x'; 600 << 10];
        let zip = build_zip(&[
            Fixture::deflated("a.xml", &content),
            Fixture::stored("b.xml", &content),
        ]);
        let package = parse(zip);

        let mut budget = 1 << 20;
        assert_eq!(
            package.read("a.xml", &mut budget).unwrap().unwrap(),
            content
        );
        let err = package.read("b.xml", &mut budget).unwrap_err();
        assert!(matches!(err.0, Reason::BudgetExceeded(_)));
        // Even a failed read of the same entry is charged again.
        assert!(package.read("a.xml", &mut budget).is_err());

        // Another budget is untouched.
        assert_eq!(read(&package, "b.xml").unwrap(), content);
    }

    #[test]
    fn crc_mismatch_is_rejected() {
        for entry in [
            Fixture::stored("a.xml", b"hello"),
            Fixture::deflated("a.xml", b"hello"),
        ] {
            let entry = Fixture {
                crc32: entry.crc32 ^ 1,
                ..entry
            };
            let package = parse(build_zip(&[entry]));
            let err = read(&package, "a.xml").unwrap_err();
            assert!(matches!(err.0, Reason::CrcMismatch(_)));
        }
    }

    #[test]
    fn content_not_matching_its_declared_size_is_rejected() {
        for entry in [
            Fixture::stored("a.xml", b"hello"),
            Fixture::deflated("a.xml", b"hello"),
        ] {
            for uncompressed_size in [4, 6] {
                let entry = Fixture {
                    uncompressed_size,
                    ..entry.clone()
                };
                let package = parse(build_zip(&[entry]));
                let err = read(&package, "a.xml").unwrap_err();
                assert!(matches!(err.0, Reason::SizeMismatch(_)));
            }
        }
    }

    #[test]
    fn corrupt_deflate_data_is_rejected() {
        let valid = Fixture::deflated("a.xml", DOCUMENT);
        let truncated = Fixture {
            compressed: valid.compressed[..valid.compressed.len() / 2].to_vec(),
            ..valid.clone()
        };
        // Block type 3 is reserved (RFC 1951 §3.2.3).
        let invalid = Fixture {
            compressed: vec![0b111; 16],
            ..valid
        };
        for entry in [truncated, invalid] {
            let package = parse(build_zip(&[entry]));
            let err = read(&package, "a.xml").unwrap_err();
            assert!(matches!(err.0, Reason::CorruptData(_)), "{err}");
        }
    }

    // ── Rejected archives ────────────────────────────────────────────────────

    #[test]
    fn non_zip_and_truncated_archives_are_rejected() {
        let zip = sample();
        let central_directory = u32_at(&zip, zip.len() - 6) as usize;
        for data in [
            &b"not a zip"[..],
            &[],
            &zip[..zip.len() - 1],          // inside the end record
            &zip[..zip.len() - 22],         // without the end record
            &zip[..central_directory + 10], // inside the central directory
            &zip[..central_directory - 10], // inside the last entry
        ] {
            let reason = rejection(data.to_vec());
            assert!(matches!(reason, Reason::Zip(_)), "{reason}");
        }

        let Err(err) = Package::parse(b"not a zip".to_vec()) else {
            panic!("archive accepted");
        };
        assert_eq!(err.to_string(), "malformed ZIP structure");
        assert!(err.source().is_some());
    }

    #[test]
    fn names_equal_ignoring_ascii_case_are_rejected() {
        let zip = build_zip(&[
            Fixture::stored("word/document.xml", DOCUMENT),
            Fixture::stored("Word/Document.xml", DOCUMENT),
        ]);
        let reason = rejection(zip);
        assert!(matches!(&reason, Reason::DuplicateName(name) if name == "Word/Document.xml"));

        let zip = build_zip(&[
            Fixture::stored("a.xml", b"1"),
            Fixture::stored("a.xml", b"2"),
        ]);
        assert!(matches!(rejection(zip), Reason::DuplicateName(_)));
    }

    #[test]
    fn encrypted_entries_are_rejected() {
        for flags in [FLAG_ENCRYPTED, 1 << 6, 1 << 13] {
            let entry = Fixture::stored("word/document.xml", DOCUMENT).with_flags(flags);
            assert!(matches!(
                rejection(build_zip(&[entry])),
                Reason::Encrypted(_)
            ));
        }
    }

    #[test]
    fn non_utf8_names_are_rejected() {
        // "média" in Windows-1252.
        let entry = Fixture {
            name: b"word/m\xe9dia.xml".to_vec(),
            ..Fixture::stored("x", b"")
        };
        let reason = rejection(build_zip(&[entry]));
        assert!(matches!(&reason, Reason::NonUtf8Name(name) if name == "word/m\u{fffd}dia.xml"));
    }

    #[test]
    fn names_the_writer_would_rewrite_are_rejected() {
        for name in [
            "word\\document.xml",
            "/word/document.xml",
            "./word/document.xml",
            "../word/document.xml",
            "word//document.xml",
            "word/./document.xml",
            "word/../document.xml",
            "C:word/document.xml",
            "/",
        ] {
            let reason = rejection(build_zip(&[Fixture::stored(name, b"")]));
            assert!(matches!(reason, Reason::NonCanonicalName(_)), "{name}");
        }

        // Dots and odd characters that normalization leaves alone are fine.
        for entry in [
            Fixture::stored("_rels/.rels", RELS),
            Fixture::stored("word/a..b.xml", b"x"),
            Fixture::stored("~$owner.docx", b"x"),
            Fixture::directory("word/.hidden/"),
        ] {
            let input = build_zip(&[entry]);
            assert_all_kept(&input, &write(&parse(input.clone()), &[]));
        }
    }

    #[test]
    fn entries_sharing_bytes_are_rejected() {
        // Point the second central directory record at the first local
        // header, as overlapping zip bombs do.
        let mut zip = build_zip(&[
            Fixture::stored("a.xml", b"hello"),
            Fixture::stored("b.xml", b"hello"),
        ]);
        let second_record = u32_at(&zip, zip.len() - 6) as usize + 46 + "a.xml".len();
        zip[second_record + 42..second_record + 46].fill(0);
        assert!(matches!(rejection(zip), Reason::Overlap(_)));
    }

    #[test]
    fn directories_with_content_are_rejected() {
        let zip = build_zip(&[Fixture::stored("word/", b"not empty")]);
        assert!(matches!(rejection(zip), Reason::DirectoryWithContent(_)));
    }

    #[test]
    fn entry_count_mismatch_is_rejected() {
        let mut zip = sample();
        let end_record = zip.len() - 22;
        zip[end_record + 8] -= 1; // entries on this disk
        zip[end_record + 10] -= 1; // total entries
        let reason = rejection(zip);
        assert!(matches!(
            reason,
            Reason::EntryCount {
                found: 8,
                declared: 7
            }
        ));
    }

    #[test]
    fn flags_the_writer_would_drop_are_rejected() {
        let imploded = Fixture {
            method: METHOD_IMPLODE,
            ..Fixture::stored("a.xml", b"x")
        };
        let patched = Fixture::stored("a.xml", b"x").with_flags(1 << 5);
        for entry in [
            imploded.clone().with_flags(0b0010),
            imploded.with_flags(0b0100),
            patched,
        ] {
            assert!(matches!(
                rejection(build_zip(&[entry])),
                Reason::UnsupportedFlags(_)
            ));
        }
    }

    // ── Hostile input ────────────────────────────────────────────────────────

    /// Parse `zip` with a small budget, read every entry and write it back.
    ///
    /// Whatever parses must write to a `Vec` without error, with no entry
    /// stored with a data descriptor, and the output must parse again.
    fn exercise(zip: Vec<u8>) {
        let Ok(package) = Package::parse(zip) else {
            return;
        };
        let mut budget = 64 << 10;
        for name in package.names() {
            let _ = package.read(name, &mut budget);
        }
        let output = write(&package, &[]);
        assert_no_stored_entry_with_descriptor(&output);
        Package::parse(output).unwrap();
    }

    #[test]
    fn every_truncation_of_a_valid_archive_is_handled() {
        let zip = sample();
        for len in 0..zip.len() {
            exercise(zip[..len].to_vec());
        }
    }

    #[test]
    fn byte_flips_anywhere_are_handled() {
        let zip = sample();
        for offset in 0..zip.len() {
            for mask in [0x01, 0x80, 0xff] {
                let mut flipped = zip.clone();
                flipped[offset] ^= mask;
                exercise(flipped);
            }
        }
    }

    // ── Writing ──────────────────────────────────────────────────────────────

    #[test]
    fn invalid_replacements_are_rejected() {
        let package = parse(sample());
        for replace in [
            vec![("word/".to_owned(), Bytes::new())],
            vec![
                ("word/document.xml".to_owned(), Bytes::new()),
                ("WORD/document.xml".to_owned(), Bytes::new()),
            ],
            vec![
                ("new.xml".to_owned(), Bytes::new()),
                ("NEW.xml".to_owned(), Bytes::new()),
            ],
            vec![("word\\new.xml".to_owned(), Bytes::new())],
            vec![("word/new/".to_owned(), Bytes::new())],
            vec![("a".repeat(70_000), Bytes::new())],
        ] {
            let mut out = Vec::new();
            let err = package.write(&mut out, &replace).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
            assert!(out.is_empty(), "{err}: rejected before writing");
        }
    }

    /// A writer that accepts `remaining` bytes, then fails.
    #[derive(Debug)]
    struct FailingWriter {
        remaining: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
            }
            let written = buf.len().min(self.remaining);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_errors_are_returned_unchanged() {
        let package = parse(sample());
        let len = write(&package, &[]).len();
        for remaining in [0, 10, 100, len / 2, len - 1] {
            let err = package.write(FailingWriter { remaining }, &[]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
            assert_eq!(err.to_string(), "pipe closed");
        }
        assert!(package.write(FailingWriter { remaining: len }, &[]).is_ok());
    }
}
