//! Camera RAW containers, via the preview image they carry.
//!
//! A RAW file is a sensor mosaic, not a picture: demosaicing one is a decode
//! this crate has no business attempting, and the `tiff` crate's attempt is
//! worse than nothing — on a NEF it decodes IFD0, which is a 160x120 postage
//! stamp beside a 12 MP photograph, and hands it back for a 512 px request.
//! Every camera writes a real JPEG of the shot alongside the mosaic, though,
//! so the whole job here is *finding* that JPEG. Once found it is handed to
//! the ordinary JPEG decoder over a [`SubSource`] window, which means the
//! IDCT-scaled decode, the peak estimate and the budget check are the same
//! ones every other format gets — nothing about RAW is special downstream.
//!
//! Where the preview hides, verified against 80 pinned samples:
//!
//! | Container | Where |
//! |---|---|
//! | CR2 | IFD0's single strip (a full-size JPEG) |
//! | NEF, ARW, DNG, SRW | a SubIFD's `JPEGInterchangeFormat`, or its strip |
//! | PEF | a chained IFD's `JPEGInterchangeFormat` |
//! | RW2 | Panasonic's private tag `0x002E`, a bare JPEG blob |
//! | ORF | inside the Olympus maker note (see [`walk_olympus`]) |
//!
//! Some Nikon and DNG files carry no JPEG at all, only a small uncompressed
//! RGB strip; that is picked up too, since a 320x213 preview still beats no
//! thumbnail.
//!
//! Everything here reads attacker-controlled offsets. Directory recursion is
//! depth- and count-capped, every offset and length is checked against the
//! source length before a read, and a candidate is only believed once its
//! bytes have been looked at — a header claiming "4000x3000 JPEG" that does
//! not start with an SOI is discarded, not trusted.

use crate::exif::{Endian, u16_at, u32_at};
use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SmallImage, SubSource, ThumbError,
	ThumbSpec,
};

/// Directories visited per file, across the IFD chain, SubIFDs and maker
/// notes. Real files use a handful; this is the forged-graph backstop.
const MAX_IFDS: usize = 48;
/// Entries one directory may declare. The largest seen in the pinned set is
/// 142 (an Olympus sub-directory).
const MAX_ENTRIES: u16 = 512;
/// How deep SubIFD / maker-note nesting may go.
const MAX_DEPTH: u8 = 4;
/// Preview candidates collected before the walk stops looking.
const MAX_CANDIDATES: usize = 24;
/// Bytes read at a candidate's offset to decide whether it really is a JPEG
/// and how big. Most previews put their SOF within a few hundred bytes, so
/// that is what is read first...
const SNIFF_BYTES: u64 = 4 * 1024;
/// ...but Panasonic prefixes its preview with a 31 KB EXIF block carrying a
/// thumbnail of its own, so a candidate that begins with an SOI and has not
/// reached its SOF yet earns one larger read before it is given up on.
const DEEP_SNIFF_BYTES: u64 = 64 * 1024;
/// Anything smaller cannot be a JPEG worth decoding, and rejecting it early
/// keeps degenerate 4-byte "previews" out of the ranking.
const MIN_PREVIEW_BYTES: u64 = 128;
/// Ceiling on an UNCOMPRESSED preview, which is materialised whole rather
/// than scaled during decode. The largest in the pinned set is 320x213; this
/// is ~15x that, and the 4 MB of RGBA it implies still fits the default
/// budget with room to spare.
const MAX_UNCOMPRESSED_PREVIEW_PIXELS: u64 = 1 << 20;

/// TIFF-layout RAW containers whose magic is NOT plain TIFF's `42`.
///
/// Olympus writes `IIRO` / `IIRS` (and `MMOR` / `MMSR` big-endian) and
/// Panasonic writes `IIU\0`; the byte order mark and the directory layout are
/// TIFF's in every case, only the version word differs.
pub(super) fn detect_vendor_tiff(prefix: &[u8]) -> bool {
	matches!(
		prefix.get(0..4),
		Some(b"IIRO" | b"IIRS" | b"MMOR" | b"MMSR" | b"II\x55\x00")
	)
}

/// What the container walk found.
pub(super) struct Index {
	/// EXIF orientation from the first directory that declares one.
	pub orientation: u8,
	/// The largest believable preview, or `None` when the file hides nothing
	/// usable.
	pub preview: Option<Preview>,
}

pub(super) enum Preview {
	/// A JPEG byte range, with the dimensions its own SOF declares.
	Jpeg { off: u64, len: u64, w: u32, h: u32 },
	/// A single uncompressed 8-bit RGB strip.
	Rgb { off: u64, w: u32, h: u32 },
}

impl Preview {
	fn area(&self) -> u64 {
		let (w, h) = match *self {
			Preview::Jpeg { w, h, .. } | Preview::Rgb { w, h, .. } => (w, h),
		};
		u64::from(w) * u64::from(h)
	}

	fn dims(&self) -> (u32, u32) {
		match *self {
			Preview::Jpeg { w, h, .. } | Preview::Rgb { w, h, .. } => (w, h),
		}
	}
}

/// Reads exactly `len` bytes, or `None` — short reads, overflowing ranges and
/// anything past the end of the source are all the same answer here.
fn read_exact_at(src: &mut dyn ByteSource, offset: u64, len: u64) -> Option<Vec<u8>> {
	let end = offset.checked_add(len)?;
	if len == 0 || end > src.len() {
		return None;
	}
	let mut buf = vec![0u8; usize::try_from(len).ok()?];
	let mut done = 0usize;
	while done < buf.len() {
		match src.read_at(offset + done as u64, &mut buf[done..]) {
			Ok(0) | Err(_) => return None,
			Ok(n) => done += n,
		}
	}
	Some(buf)
}

/// One directory entry, with its value either inline (the common case for the
/// scalars walked here) or at a resolved absolute offset.
struct Entry {
	tag: u16,
	typ: u16,
	count: u32,
	/// The 4 value bytes as stored, for inline values.
	bytes: [u8; 4],
	/// Absolute offset of the value when it does not fit inline.
	value_at: u64,
}

impl Entry {
	fn type_size(&self) -> Option<u64> {
		Some(match self.typ {
			1 | 2 | 6 | 7 => 1,
			3 | 8 => 2,
			4 | 9 | 11 | 13 => 4,
			5 | 10 | 12 => 8,
			_ => return None,
		})
	}

	fn total(&self) -> Option<u64> {
		self.type_size()?.checked_mul(u64::from(self.count))
	}

	/// A single inline unsigned value. Deliberately narrow: every tag this
	/// walker cares about is a count-1 SHORT or LONG, so nothing here has to
	/// chase a value offset, and a tag that does not fit that shape is simply
	/// not read.
	fn scalar(&self, endian: Endian) -> Option<u64> {
		if self.count != 1 || self.total()? > 4 {
			return None;
		}
		match self.typ {
			3 | 8 => u16_at(&self.bytes, 0, endian).map(u64::from),
			4 | 9 | 13 => u32_at(&self.bytes, 0, endian).map(u64::from),
			1 | 6 | 7 => Some(u64::from(self.bytes[0])),
			_ => None,
		}
	}
}

struct Directory {
	entries: Vec<Entry>,
	next: u64,
}

impl Directory {
	fn get(&self, tag: u16) -> Option<&Entry> {
		self.entries.iter().find(|e| e.tag == tag)
	}

	fn scalar(&self, tag: u16, endian: Endian) -> Option<u64> {
		self.get(tag)?.scalar(endian)
	}
}

/// Reads one directory table. `base` is what value offsets inside it are
/// relative to — 0 for the file's own IFDs, the maker-note start for a maker
/// note that rebases its offsets.
fn read_directory(
	src: &mut dyn ByteSource,
	endian: Endian,
	base: u64,
	at: u64,
) -> Option<Directory> {
	let head = read_exact_at(src, at, 2)?;
	let count = u16_at(&head, 0, endian)?;
	if count == 0 || count > MAX_ENTRIES {
		return None;
	}
	let table_len = u64::from(count) * 12;
	// The trailing next-IFD pointer is missing on the sub-directories some
	// maker notes inline, so its absence is not a reason to drop the table.
	let (table, next) = match read_exact_at(src, at + 2, table_len + 4) {
		Some(table) => {
			let next = u32_at(&table, table_len as usize, endian).unwrap_or(0);
			(table, u64::from(next))
		}
		None => (read_exact_at(src, at + 2, table_len)?, 0),
	};

	let mut entries = Vec::with_capacity(count as usize);
	for i in 0..count as usize {
		let at_entry = i * 12;
		let (Some(tag), Some(typ), Some(n)) = (
			u16_at(&table, at_entry, endian),
			u16_at(&table, at_entry + 2, endian),
			u32_at(&table, at_entry + 4, endian),
		) else {
			continue;
		};
		let bytes: [u8; 4] = match table.get(at_entry + 8..at_entry + 12) {
			Some(slice) => slice.try_into().ok()?,
			None => continue,
		};
		let mut entry = Entry {
			tag,
			typ,
			count: n,
			bytes,
			value_at: at + 2 + at_entry as u64 + 8,
		};
		if entry.total().is_some_and(|total| total > 4)
			&& let Some(raw) = u32_at(&bytes, 0, endian)
		{
			entry.value_at = base.saturating_add(u64::from(raw));
		}
		entries.push(entry);
	}
	Some(Directory { entries, next })
}

/// A candidate before its bytes have been looked at.
struct Candidate {
	off: u64,
	len: u64,
	/// Dimensions the directory declares, used only for the uncompressed
	/// case — a JPEG's real size comes from its own SOF, because the RAW
	/// containers that matter most (NEF, ARW) declare nothing at all for
	/// their big preview, and the ones that do sometimes declare the sensor's
	/// size rather than the preview's.
	w: u32,
	h: u32,
	/// The directory's own tags say this is a small reduced-resolution
	/// uncompressed RGB image whose byte count matches its geometry exactly.
	uncompressed: bool,
}

struct Walk<'a> {
	src: &'a mut dyn ByteSource,
	visited: Vec<u64>,
	candidates: Vec<Candidate>,
	orientation: u8,
	/// Maker note payload (offset, length), parsed after the main walk so its
	/// own base is known.
	maker_note: Option<(u64, u64)>,
}

/// Which tag vocabulary a directory speaks. Maker notes reuse low tag numbers
/// for entirely different things — `0x0100` is `ImageWidth` in a TIFF IFD and
/// a JPEG thumbnail blob in an Olympus maker note — so the two must never be
/// read with the same rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vocab {
	Tiff,
	Maker,
}

impl Walk<'_> {
	fn walk(&mut self, endian: Endian, base: u64, at: u64, depth: u8, vocab: Vocab) {
		if depth > MAX_DEPTH
			|| self.visited.len() >= MAX_IFDS
			|| self.candidates.len() >= MAX_CANDIDATES
			|| self.visited.contains(&at)
		{
			return;
		}
		self.visited.push(at);
		let Some(dir) = read_directory(self.src, endian, base, at) else {
			return;
		};

		match vocab {
			Vocab::Tiff => self.collect_tiff(&dir, endian),
			Vocab::Maker => self.collect_maker(&dir, endian, base),
		}

		// SubIFDs: an array of directory offsets. Nikon, Sony, Samsung and
		// Adobe all park the real preview in one.
		if let Some(entry) = dir.get(0x014A) {
			for off in self.offset_array(entry, endian, base) {
				self.walk(endian, base, off, depth + 1, vocab);
			}
		}
		// The Exif IFD is only interesting as the road to the maker note.
		if vocab == Vocab::Tiff
			&& let Some(exif_at) = dir.scalar(0x8769, endian)
		{
			self.walk(endian, base, base.saturating_add(exif_at), depth + 1, vocab);
		}
		if vocab == Vocab::Tiff
			&& self.maker_note.is_none()
			&& let Some(entry) = dir.get(0x927C)
			&& let Some(total) = entry.total()
			&& total > 4
		{
			self.maker_note = Some((entry.value_at, total));
		}
		// Olympus CameraSettings, which holds the offset of the LARGE preview
		// (the maker note's own 0x0100 blob is only the 160x120 stamp). Typed
		// as an IFD pointer by newer bodies and as an inline blob by older
		// ones; both land on a directory table.
		if vocab == Vocab::Maker
			&& let Some(entry) = dir.get(0x2020)
		{
			let target = match entry.scalar(endian) {
				Some(rel) => base.saturating_add(rel),
				None => entry.value_at,
			};
			self.walk(endian, base, target, depth + 1, vocab);
		}

		if dir.next != 0 {
			// A chained directory is a sibling, not a child: PEF puts its
			// full-size preview in IFD2.
			self.walk(endian, base, base.saturating_add(dir.next), depth, vocab);
		}
	}

	/// The LONG array a SubIFDs tag points at, bounded and bounds-checked.
	fn offset_array(&mut self, entry: &Entry, endian: Endian, base: u64) -> Vec<u64> {
		if entry.typ != 4 && entry.typ != 13 {
			return Vec::new();
		}
		let count = entry.count.min(MAX_IFDS as u32);
		if count == 1 {
			return entry
				.scalar(endian)
				.map(|off| vec![base.saturating_add(off)])
				.unwrap_or_default();
		}
		let Some(bytes) = read_exact_at(self.src, entry.value_at, u64::from(count) * 4) else {
			return Vec::new();
		};
		(0..count as usize)
			.filter_map(|i| u32_at(&bytes, i * 4, endian))
			.map(|off| base.saturating_add(u64::from(off)))
			.collect()
	}

	fn push(&mut self, off: u64, len: u64, w: u32, h: u32, uncompressed: bool) {
		if len >= MIN_PREVIEW_BYTES && self.candidates.len() < MAX_CANDIDATES {
			self.candidates.push(Candidate {
				off,
				len,
				w,
				h,
				uncompressed,
			});
		}
	}

	fn collect_tiff(&mut self, dir: &Directory, endian: Endian) {
		if self.orientation == 1
			&& let Some(o @ 1..=8) = dir.scalar(0x0112, endian)
		{
			self.orientation = o as u8;
		}
		let width = dir.scalar(0x0100, endian).unwrap_or(0);
		let height = dir.scalar(0x0101, endian).unwrap_or(0);
		let photometric = dir.scalar(0x0106, endian);
		// PhotometricInterpretation 32803 (CFA) and 34892 (LinearRaw) mark the
		// sensor mosaic. Some DNGs store that mosaic as a lossy JPEG, which
		// would otherwise sail through every other check and thumbnail as a
		// green grid.
		let displayable = photometric.is_none_or(|p| p == 2 || p == 6);
		let (w, h) = (
			width.min(u32::MAX.into()) as u32,
			height.min(u32::MAX.into()) as u32,
		);

		if displayable
			&& let (Some(off), Some(len)) = (dir.scalar(0x0201, endian), dir.scalar(0x0202, endian))
		{
			self.push(off, len, w, h, false);
		}
		// A single strip is either a whole JPEG (Canon's CR2 preview, Adobe's
		// DNG preview) or a small uncompressed RGB image (Nikon's, Adobe's
		// thumbnails). Multi-strip images are real pictures for the streaming
		// TIFF decoder, not previews.
		if displayable
			&& dir.get(0x0111).is_some_and(|e| e.count == 1)
			&& let (Some(off), Some(len)) = (dir.scalar(0x0111, endian), dir.scalar(0x0117, endian))
		{
			let pixels = u64::from(w) * u64::from(h);
			// Trusting Compression here would be a mistake: Canon tags an
			// uncompressed RGB sub-image as JPEG. The byte count matching the
			// geometry exactly is the check that cannot be talked out of.
			let uncompressed = photometric == Some(2)
				&& dir.scalar(0x00FE, endian) == Some(1)
				&& pixels.saturating_mul(3) == len
				&& pixels <= MAX_UNCOMPRESSED_PREVIEW_PIXELS;
			self.push(off, len, w, h, uncompressed);
		}
		// Panasonic keeps a full JPEG in a private tag, with no directory of
		// its own to describe it.
		if let Some(entry) = dir.get(0x002E)
			&& entry.typ == 7
			&& entry.total().is_some_and(|total| total > 4)
		{
			self.push(entry.value_at, u64::from(entry.count), 0, 0, false);
		}
	}

	fn collect_maker(&mut self, dir: &Directory, endian: Endian, base: u64) {
		// The maker note's own thumbnail blob.
		if let Some(entry) = dir.get(0x0100)
			&& entry.typ == 7
			&& entry.total().is_some_and(|total| total > 4)
		{
			self.push(entry.value_at, u64::from(entry.count), 0, 0, false);
		}
		// Olympus CameraSettings: PreviewImageStart / PreviewImageLength.
		if let (Some(off), Some(len)) = (dir.scalar(0x0101, endian), dir.scalar(0x0102, endian)) {
			self.push(base.saturating_add(off), len, 0, 0, false);
		}
	}

	/// Olympus is the one vendor in the pinned set that hides its large
	/// preview behind a maker note. Two header layouts, differing in where
	/// the directory starts and — the part that bites — what its offsets are
	/// relative to.
	fn walk_olympus(&mut self, file_endian: Endian, at: u64, len: u64) {
		let Some(head) = read_exact_at(self.src, at, 12.min(len)) else {
			return;
		};
		if head.starts_with(b"OLYMPUS\0") {
			let endian = match head.get(8..10) {
				Some(b"II") => Endian::Little,
				Some(b"MM") => Endian::Big,
				_ => return,
			};
			// Offsets rebased to the maker note itself.
			self.walk(endian, at, at + 12, 1, Vocab::Maker);
		} else if head.starts_with(b"OLYMP\0") {
			// The older layout keeps file-relative offsets.
			self.walk(file_endian, 0, at + 8, 1, Vocab::Maker);
		}
	}
}

/// Walks the container and returns the best preview it can prove is really
/// there. Reads a few KB per directory and 16 KB per candidate; nothing here
/// pulls a preview's payload, only enough of its head to believe in it.
pub(super) fn scan(src: &mut dyn ByteSource) -> Index {
	let Some(header) = read_exact_at(src, 0, 8) else {
		return Index {
			orientation: 1,
			preview: None,
		};
	};
	let endian = match &header[0..2] {
		b"II" => Endian::Little,
		b"MM" => Endian::Big,
		_ => {
			return Index {
				orientation: 1,
				preview: None,
			};
		}
	};
	let Some(ifd0) = u32_at(&header, 4, endian) else {
		return Index {
			orientation: 1,
			preview: None,
		};
	};

	let mut walk = Walk {
		src,
		visited: Vec::new(),
		candidates: Vec::new(),
		orientation: 1,
		maker_note: None,
	};
	walk.walk(endian, 0, u64::from(ifd0), 0, Vocab::Tiff);
	if let Some((at, len)) = walk.maker_note {
		walk.walk_olympus(endian, at, len);
	}

	let orientation = walk.orientation;
	let src = walk.src;
	let preview = walk
		.candidates
		.iter()
		.filter_map(|c| believe(src, c))
		.max_by_key(Preview::area);
	Index {
		orientation,
		preview,
	}
}

/// Turns a claimed candidate into one we have seen the bytes of, or nothing.
fn believe(src: &mut dyn ByteSource, candidate: &Candidate) -> Option<Preview> {
	let end = candidate.off.checked_add(candidate.len)?;
	if candidate.len < MIN_PREVIEW_BYTES || end > src.len() {
		return None;
	}
	let head = read_exact_at(src, candidate.off, candidate.len.min(SNIFF_BYTES))?;
	if head.starts_with(&[0xFF, 0xD8]) {
		let dims = jpeg_sof(&head).or_else(|| {
			let deeper = candidate.len.min(DEEP_SNIFF_BYTES);
			(deeper > head.len() as u64)
				.then(|| jpeg_sof(&read_exact_at(src, candidate.off, deeper)?))
				.flatten()
		});
		let (w, h) = dims?;
		return Some(Preview::Jpeg {
			off: candidate.off,
			len: candidate.len,
			w,
			h,
		});
	}
	candidate.uncompressed.then_some(Preview::Rgb {
		off: candidate.off,
		w: candidate.w,
		h: candidate.h,
	})
}

/// The frame dimensions from a JPEG's SOF, over a bounded prefix.
///
/// Only the three SOFs a general-purpose decoder handles are accepted, and
/// that is a correctness requirement rather than caution: a RAW file's sensor
/// data is itself frequently stored as a *lossless* JPEG (SOF3), often the
/// largest JPEG-shaped thing in the file. Ranking by size without this check
/// picks the mosaic every time.
fn jpeg_sof(data: &[u8]) -> Option<(u32, u32)> {
	let mut at = 2usize;
	while at + 4 <= data.len() {
		if data[at] != 0xFF {
			return None;
		}
		let marker = data[at + 1];
		// Fill bytes, TEM, RSTn and a stray SOI carry no payload.
		if marker == 0xFF {
			at += 1;
			continue;
		}
		if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
			at += 2;
			continue;
		}
		// Entropy-coded data or end of image: the SOF should have come first.
		if marker == 0xD9 || marker == 0xDA {
			return None;
		}
		let len = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
		if len < 2 {
			return None;
		}
		if matches!(marker, 0xC0..=0xC2) {
			let h = u16::from_be_bytes([*data.get(at + 5)?, *data.get(at + 6)?]);
			let w = u16::from_be_bytes([*data.get(at + 7)?, *data.get(at + 8)?]);
			return (w > 0 && h > 0).then_some((u32::from(w), u32::from(h)));
		}
		at = at.checked_add(2 + len)?;
	}
	None
}

/// Wraps the found preview as a prepared decode.
///
/// The RAW itself is never decoded, so `peak_estimate` is unaffordable by
/// construction and [`embedded_preview`](PreparedDecode::embedded_preview) is
/// the only path that produces anything — which also means a preview-only
/// caller (a large remote file) gets the same thumbnail a local one does, for
/// the cost of the preview's byte range.
pub(super) fn prepare(
	src: Box<dyn ByteSource>,
	preview: Preview,
	orientation: u8,
	spec: &ThumbSpec,
) -> Result<Box<dyn PreparedDecode>, ThumbError> {
	let dims = preview.dims();
	match preview {
		Preview::Jpeg { off, len, .. } => {
			let inner = super::JPEG.open(Box::new(SubSource::new(src, off, len)), spec)?;
			// The RAW's own orientation tag wins when it has one; a bare
			// preview stream usually carries no EXIF of its own. Taking one or
			// the other — never both — is what keeps a 90-degree rotation from
			// being applied twice.
			let orientation = if orientation > 1 {
				orientation
			} else {
				inner.orientation()
			};
			Ok(Box::new(PreparedRawJpeg {
				inner: Some(inner),
				spec: *spec,
				dims,
				orientation,
			}))
		}
		Preview::Rgb { off, w, h } => Ok(Box::new(PreparedRawRgb {
			src: Some(src),
			off,
			dims: (w, h),
			orientation,
		})),
	}
}

struct PreparedRawJpeg {
	inner: Option<Box<dyn PreparedDecode>>,
	spec: ThumbSpec,
	dims: (u32, u32),
	orientation: u8,
}

impl PreparedDecode for PreparedRawJpeg {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self) -> Result<Option<SmallImage>, ThumbError> {
		let Some(inner) = self.inner.take() else {
			return Ok(None);
		};
		// Priced by the same rules as any other decode; over budget answers
		// None rather than failing, and the orchestrator reports no thumbnail.
		crate::decode_bounded(inner, &self.spec)
	}

	fn peak_estimate(&self) -> usize {
		usize::MAX
	}

	fn decode_into(self: Box<Self>, _sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		Err(ThumbError::Decode(
			"raw: the sensor mosaic is not decodable here".into(),
		))
	}
}

struct PreparedRawRgb {
	src: Option<Box<dyn ByteSource>>,
	off: u64,
	dims: (u32, u32),
	orientation: u8,
}

impl PreparedDecode for PreparedRawRgb {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self) -> Result<Option<SmallImage>, ThumbError> {
		let (Some(mut src), (w, h)) = (self.src.take(), self.dims) else {
			return Ok(None);
		};
		let row_bytes = w as usize * 3;
		let mut rgba = vec![0u8; w as usize * h as usize * 4];
		// Read in bands rather than row by row: a remote source charges per
		// request, and 64 KB at a time keeps both the request count and the
		// scratch buffer small.
		let rows_per_band = (65536 / row_bytes.max(1)).max(1);
		let mut band = vec![0u8; rows_per_band * row_bytes];
		let mut y = 0usize;
		while y < h as usize {
			let rows = rows_per_band.min(h as usize - y);
			let want = &mut band[..rows * row_bytes];
			let mut done = 0usize;
			while done < want.len() {
				match src.read_at(self.off + (y * row_bytes + done) as u64, &mut want[done..])? {
					0 => return Ok(None),
					n => done += n,
				}
			}
			for r in 0..rows {
				let src_row = &want[r * row_bytes..][..row_bytes];
				let dst_row = &mut rgba[(y + r) * w as usize * 4..][..w as usize * 4];
				for (dst, px) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(3)) {
					dst.copy_from_slice(&[px[0], px[1], px[2], 255]);
				}
			}
			y += rows;
		}
		Ok(Some(SmallImage {
			width: w,
			height: h,
			rgba,
		}))
	}

	fn peak_estimate(&self) -> usize {
		usize::MAX
	}

	fn decode_into(self: Box<Self>, _sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		Err(ThumbError::Decode(
			"raw: the sensor mosaic is not decodable here".into(),
		))
	}
}

#[cfg(test)]
mod tests {
	use super::{Vocab, Walk, jpeg_sof, scan};
	use crate::{ByteSource, MemSource, exif::Endian};

	/// A little-endian TIFF header pointing at `ifd0`.
	fn header(ifd0: u32) -> Vec<u8> {
		let mut out = b"II\x2a\x00".to_vec();
		out.extend_from_slice(&ifd0.to_le_bytes());
		out
	}

	fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
		let mut out = tag.to_le_bytes().to_vec();
		out.extend_from_slice(&typ.to_le_bytes());
		out.extend_from_slice(&count.to_le_bytes());
		out.extend_from_slice(&value.to_le_bytes());
		out
	}

	/// A minimal JPEG head: SOI, an APP0 to skip over, then a baseline SOF.
	fn jpeg_head(w: u16, h: u16) -> Vec<u8> {
		let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
		out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
		out.extend_from_slice(&h.to_be_bytes());
		out.extend_from_slice(&w.to_be_bytes());
		out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
		out
	}

	#[test]
	fn sof_is_read_and_lossless_jpeg_is_rejected() {
		assert_eq!(jpeg_sof(&jpeg_head(1600, 1200)), Some((1600, 1200)));
		// SOF3 is a RAW's own sensor data. Picking it is the failure mode this
		// check exists to prevent.
		let mut lossless = jpeg_head(4000, 3000);
		lossless[9] = 0xC3;
		assert_eq!(jpeg_sof(&lossless), None);
		// Truncated, empty and non-JPEG input answer None, never panic.
		assert_eq!(jpeg_sof(&[]), None);
		assert_eq!(jpeg_sof(&[0xFF, 0xD8, 0xFF]), None);
		assert_eq!(jpeg_sof(&jpeg_head(1600, 1200)[..8]), None);
		assert_eq!(jpeg_sof(b"not a jpeg at all"), None);
		// A zero-length marker segment must not loop forever.
		assert_eq!(
			jpeg_sof(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x00, 0xFF, 0xD9]),
			None
		);
	}

	/// The shape every TIFF-family RAW reduces to: a SubIFD carrying a JPEG
	/// the top-level directory says nothing about.
	#[test]
	fn a_subifd_jpeg_is_found_and_measured_by_its_own_sof() {
		let jpeg = jpeg_head(1600, 1200);
		let mut file = header(8);
		// IFD0: orientation 6, SubIFDs -> 60.
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, 6));
		file.extend_from_slice(&entry(0x014A, 4, 1, 60));
		file.extend_from_slice(&0u32.to_le_bytes());
		assert_eq!(file.len(), 38);
		file.resize(60, 0);
		// SubIFD: JPEGInterchangeFormat at 200, length.
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, 200));
		file.extend_from_slice(&entry(0x0202, 4, 1, 400));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(&jpeg);
		file.resize(600, 0);

		let mut src = MemSource(file);
		let index = scan(&mut src);
		assert_eq!(index.orientation, 6);
		let preview = index.preview.expect("the SubIFD JPEG must be found");
		assert_eq!(preview.dims(), (1600, 1200));
	}

	/// Hostile directory graphs: a self-referential SubIFD, a directory
	/// claiming millions of entries, and offsets past the end of the file.
	/// None of these may hang, panic or read out of bounds.
	#[test]
	fn forged_directories_are_refused_without_panicking() {
		// A SubIFD pointing back at IFD0, and IFD0 chaining to itself.
		let mut file = header(8);
		file.extend_from_slice(&1u16.to_le_bytes());
		file.extend_from_slice(&entry(0x014A, 4, 1, 8));
		file.extend_from_slice(&8u32.to_le_bytes());
		file.resize(128, 0);
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// An absurd entry count.
		let mut file = header(8);
		file.extend_from_slice(&60000u16.to_le_bytes());
		file.resize(128, 0);
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// Every offset past the end.
		let mut file = header(8);
		file.extend_from_slice(&3u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, u32::MAX));
		file.extend_from_slice(&entry(0x0202, 4, 1, u32::MAX));
		file.extend_from_slice(&entry(0x014A, 4, 1, u32::MAX - 1));
		file.extend_from_slice(&u32::MAX.to_le_bytes());
		file.resize(128, 0);
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// Truncated mid-directory.
		let mut file = header(8);
		file.extend_from_slice(&4u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, 8));
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// Not a TIFF at all, and empty.
		assert!(scan(&mut MemSource(b"nonsense".to_vec())).preview.is_none());
		assert!(scan(&mut MemSource(Vec::new())).preview.is_none());
	}

	/// A directory graph wide enough to exhaust the caps must terminate, and
	/// must not have read the whole file to decide that.
	#[test]
	fn a_wide_forged_graph_terminates() {
		// 256 SubIFDs, each pointing at the next, 4 KB apart.
		let mut file = header(8);
		file.extend_from_slice(&1u16.to_le_bytes());
		file.extend_from_slice(&entry(0x014A, 4, 1, 4096));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(4096, 0);
		for i in 1..256u32 {
			let mut dir = 1u16.to_le_bytes().to_vec();
			dir.extend_from_slice(&entry(0x014A, 4, 1, (i + 1) * 4096));
			dir.extend_from_slice(&0u32.to_le_bytes());
			file.extend_from_slice(&dir);
			file.resize(((i + 1) * 4096) as usize, 0);
		}
		let mut src = MemSource(file);
		assert!(scan(&mut src).preview.is_none());
	}

	/// The uncompressed path only fires when the byte count matches the
	/// geometry exactly — the check that stops a full-size uncompressed TIFF
	/// from being mistaken for a thumbnail.
	#[test]
	fn an_uncompressed_strip_is_believed_only_when_its_size_adds_up() {
		let build = |byte_count: u32| {
			let mut file = header(8);
			file.extend_from_slice(&6u16.to_le_bytes());
			file.extend_from_slice(&entry(0x00FE, 4, 1, 1));
			file.extend_from_slice(&entry(0x0100, 4, 1, 32));
			file.extend_from_slice(&entry(0x0101, 4, 1, 8));
			file.extend_from_slice(&entry(0x0106, 3, 1, 2));
			file.extend_from_slice(&entry(0x0111, 4, 1, 200));
			file.extend_from_slice(&entry(0x0117, 4, 1, byte_count));
			file.extend_from_slice(&0u32.to_le_bytes());
			file.resize(200, 0);
			file.resize(1200, 0x40);
			MemSource(file)
		};
		let found = scan(&mut build(768)).preview.expect("32x8x3 = 768 adds up");
		assert_eq!(found.dims(), (32, 8));
		assert!(scan(&mut build(900)).preview.is_none());
	}

	/// Maker-note tags must never be read with TIFF's vocabulary: `0x0100` is
	/// `ImageWidth` in one and a JPEG blob in the other.
	#[test]
	fn maker_note_vocabulary_is_separate() {
		let jpeg = jpeg_head(640, 480);
		let mut file = header(8);
		file.extend_from_slice(&1u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0100, 7, 300, 200));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(&jpeg);
		file.resize(600, 0);
		// Read as TIFF, 0x0100 is a width and there is no candidate at all.
		assert!(scan(&mut MemSource(file.clone())).preview.is_none());
		// Read as a maker note, it is the thumbnail.
		let mut src = MemSource(file);
		let mut walk = Walk {
			src: &mut src,
			visited: Vec::new(),
			candidates: Vec::new(),
			orientation: 1,
			maker_note: None,
		};
		walk.walk(Endian::Little, 0, 8, 0, Vocab::Maker);
		assert_eq!(walk.candidates.len(), 1);
	}

	/// Finding nothing must stay cheap: a big file whose directories promise
	/// no preview is rejected after a handful of small reads.
	#[test]
	fn a_fruitless_scan_reads_only_a_prefix() {
		struct Counting(MemSource, u64);
		impl ByteSource for Counting {
			fn len(&self) -> u64 {
				self.0.len()
			}
			fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
				let n = self.0.read_at(offset, buf)?;
				self.1 += n as u64;
				Ok(n)
			}
		}
		let mut file = header(8);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, 1));
		file.extend_from_slice(&entry(0x0106, 3, 1, 32803));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(8 * 1024 * 1024, 0);
		let mut src = Counting(MemSource(file), 0);
		assert!(scan(&mut src).preview.is_none());
		assert!(src.1 < 4096, "read {} bytes to find nothing", src.1);
	}
}
