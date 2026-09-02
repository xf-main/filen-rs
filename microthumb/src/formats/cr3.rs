//! Canon CR3, via the preview image in its ISO-BMFF box tree.
//!
//! CR3 broke with the TIFF lineage every other Canon RAW follows: it is an
//! MP4-shaped container, so none of the directory machinery in [`super::raw`]
//! applies. What survives is the principle — find the JPEG Canon stored
//! alongside the mosaic and decode that — so all this module does is walk to
//! the box holding it.
//!
//! Two boxes carry one, both nested inside `uuid` boxes Canon defines: `PRVW`
//! (1620x1080 across every pinned body, from the EOS R to the PowerShot V1)
//! and `THMB` (160x120). The larger one wins when both are present. Both hold
//! a bare JPEG — SOI straight into the quantisation tables, no APP1 — so the
//! shot's orientation is not in the stream. It is in `CMT1`, the box beside
//! them that carries the file's EXIF IFD0 as a plain TIFF.
//!
//! The walk is its own bounded parser rather than a media library: box sizes
//! are attacker-controlled, so every one is checked against its parent's
//! extent, recursion is depth-capped, the total number of boxes visited is
//! capped, and no read is ever sized from a header without that check.

use crate::{
	ByteSource, FormatDecoder, LocatedPreview, PreparedDecode, ThumbError, ThumbSpec, exif,
};

use super::raw::{self, Index, Preview};

pub struct Cr3;

/// Boxes visited across the whole tree.
const MAX_BOXES: usize = 256;
/// How deep the containers nest before the walk stops. `moov` -> `uuid` ->
/// `THMB` is the deepest real path.
const MAX_DEPTH: u8 = 3;
/// A box header is 8 bytes, or 16 when the size word is the `1` escape.
const BOX_HEADER: u64 = 8;
/// Bytes of a preview box's payload searched for the SOI. Both box types put
/// their JPEG at offset 16, behind a small fixed descriptor.
const PREVIEW_PROBE: u64 = 32;
/// Most of `CMT1` read for the orientation tag. The box is IFD0 alone — make,
/// model, timestamps, a few hundred bytes on every pinned body — so this caps
/// a forged box size rather than budgeting a real one.
const CMT1_MAX_BYTES: u64 = 64 * 1024;

/// The largest preview box, and the file's orientation.
///
/// The preview JPEGs carry no EXIF of their own, so a portrait shot would come
/// out sideways without the file-level tag. `CMT1` is a TIFF, which is exactly
/// the payload the EXIF walker takes.
fn index(src: &mut dyn ByteSource) -> Index {
	let mut walk = Walk {
		boxes: 0,
		previews: Vec::new(),
		cmt1: None,
	};
	let end = src.len();
	walk.boxes_in(src, 0, end, 0);
	let preview = walk
		.previews
		.into_iter()
		.filter_map(|(off, len)| raw::jpeg_window(src, off, len))
		.max_by_key(Preview::area);
	let orientation = walk
		.cmt1
		.and_then(|(body, body_end)| {
			raw::read_exact_at(src, body, (body_end - body).min(CMT1_MAX_BYTES))
		})
		.map_or(1, |tiff| exif::orientation(&tiff));
	Index {
		orientation,
		preview,
		jpeg: preview,
	}
}

impl FormatDecoder for Cr3 {
	fn detect(&self, prefix: &[u8]) -> bool {
		// `ftyp` with Canon's `crx ` brand. Checking the brand as well as the
		// box keeps ordinary MP4s — which nothing here decodes — from being
		// claimed and then refused.
		prefix.get(4..12) == Some(b"ftypcrx ")
	}

	fn locate_preview(
		&self,
		src: &mut dyn ByteSource,
	) -> Result<Option<LocatedPreview>, ThumbError> {
		Ok(raw::locate(index(src)))
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let Index {
			orientation,
			preview: Some(preview),
			..
		} = index(&mut *src)
		else {
			return Err(ThumbError::Decode(
				"cr3: no preview box; the sensor data is not a decodable image".into(),
			));
		};
		raw::prepare(src, preview, orientation, spec)
	}
}

struct Walk {
	boxes: usize,
	/// Byte ranges claimed to hold a JPEG, unverified.
	previews: Vec<(u64, u64)>,
	/// Payload range of the first `CMT1` box: the file's EXIF IFD0.
	cmt1: Option<(u64, u64)>,
}

impl Walk {
	fn boxes_in(&mut self, src: &mut dyn ByteSource, start: u64, end: u64, depth: u8) {
		if depth > MAX_DEPTH {
			return;
		}
		let mut at = start;
		while at + BOX_HEADER <= end && self.boxes < MAX_BOXES {
			self.boxes += 1;
			let Some(header) = raw::read_exact_at(src, at, BOX_HEADER) else {
				return;
			};
			let kind: [u8; 4] = header[4..8].try_into().expect("four bytes");
			let mut size = u64::from(u32::from_be_bytes(
				header[0..4].try_into().expect("four bytes"),
			));
			let mut header_len = BOX_HEADER;
			match size {
				// The 64-bit escape.
				1 => {
					let Some(large) = raw::read_exact_at(src, at + BOX_HEADER, 8) else {
						return;
					};
					size = u64::from_be_bytes(large.try_into().expect("eight bytes"));
					header_len = 16;
				}
				// "To the end of the enclosing box."
				0 => size = end - at,
				_ => {}
			}
			// A box that does not fit inside its parent means the tree is
			// forged or truncated; stop rather than guess where the next one
			// starts.
			if size < header_len || at.checked_add(size).is_none_or(|box_end| box_end > end) {
				return;
			}
			let (body, body_end) = (at + header_len, at + size);
			match &kind {
				b"PRVW" | b"THMB" => self.preview_box(src, body, body_end),
				b"CMT1" => {
					if self.cmt1.is_none() {
						self.cmt1 = Some((body, body_end));
					}
				}
				b"moov" => self.boxes_in(src, body, body_end, depth + 1),
				// A `uuid` box names itself with 16 bytes and then, in Canon's
				// case, either starts its children immediately or first writes
				// a version word. Rather than hard-code which UUID does which,
				// look at whether a box header is actually there.
				b"uuid" => {
					if let Some(child) = child_start(src, body + 16, body_end) {
						self.boxes_in(src, child, body_end, depth + 1);
					}
				}
				_ => {}
			}
			at = body_end;
		}
	}

	/// A `PRVW` / `THMB` payload: a short fixed descriptor, then the JPEG. The
	/// descriptor's own width and height fields differ in layout between the
	/// two box types, so the SOI is found by looking rather than by trusting
	/// either, and the dimensions come from the JPEG itself.
	fn preview_box(&mut self, src: &mut dyn ByteSource, body: u64, body_end: u64) {
		let probe_len = PREVIEW_PROBE.min(body_end.saturating_sub(body));
		let Some(probe) = raw::read_exact_at(src, body, probe_len) else {
			return;
		};
		let Some(soi) = probe.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) else {
			return;
		};
		let off = body + soi as u64;
		self.previews.push((off, body_end - off));
	}
}

/// Where a `uuid` box's children begin: right after the UUID, or 8 bytes
/// further on when what sits there is a version word rather than a box.
fn child_start(src: &mut dyn ByteSource, at: u64, end: u64) -> Option<u64> {
	for candidate in [at, at + 8] {
		if candidate + BOX_HEADER > end {
			continue;
		}
		let header = raw::read_exact_at(src, candidate, BOX_HEADER)?;
		// Every box type in this container is four printable characters; a
		// version word is not.
		if header[4..8].iter().all(u8::is_ascii_graphic) {
			return Some(candidate);
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::{Cr3, Walk};
	use crate::{ByteSource, FormatDecoder, MemSource, ThumbSpec, locate_preview};

	fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
		let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
		out.extend_from_slice(kind);
		out.extend_from_slice(body);
		out
	}

	/// A baseline JPEG head, enough for the SOF scan to measure it.
	fn jpeg(w: u16, h: u16) -> Vec<u8> {
		let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
		out.extend_from_slice(&h.to_be_bytes());
		out.extend_from_slice(&w.to_be_bytes());
		out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
		out.resize(256, 0);
		out
	}

	/// `PRVW` / `THMB` bodies: a 16-byte descriptor, then the JPEG.
	fn preview_body(w: u16, h: u16) -> Vec<u8> {
		let mut body = vec![0u8; 16];
		body.extend_from_slice(&jpeg(w, h));
		body
	}

	fn found(file: Vec<u8>) -> Option<(u32, u32)> {
		let mut src = MemSource(file);
		let mut walk = Walk {
			boxes: 0,
			previews: Vec::new(),
			cmt1: None,
		};
		let end = src.len();
		walk.boxes_in(&mut src, 0, end, 0);
		walk.previews
			.into_iter()
			.filter_map(|(off, len)| super::raw::jpeg_window(&mut src, off, len))
			.max_by_key(super::Preview::area)
			.map(|p| p.dims())
	}

	/// The real layout: a `uuid` box whose payload starts with a version word
	/// before its children, and a `moov` whose Canon `uuid` starts with a box
	/// straight away. Both must be walked, and the larger preview must win.
	#[test]
	fn both_canon_uuid_layouts_are_walked_and_the_larger_preview_wins() {
		let mut versioned = vec![0u8; 16];
		versioned.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
		versioned.extend_from_slice(&boxed(b"PRVW", &preview_body(1620, 1080)));

		let mut immediate = vec![0u8; 16];
		immediate.extend_from_slice(&boxed(b"THMB", &preview_body(160, 120)));

		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"moov", &boxed(b"uuid", &immediate)));
		file.extend_from_slice(&boxed(b"uuid", &versioned));
		assert!(Cr3.detect(&file));
		assert_eq!(found(file), Some((1620, 1080)));
	}

	/// Forged box trees: sizes that do not fit their parent, the "rest of the
	/// parent" and 64-bit escapes, and a descriptor with no JPEG behind it.
	/// None may panic, hang or read out of bounds.
	#[test]
	fn forged_box_trees_are_refused_without_panicking() {
		// A child claiming to be bigger than its parent.
		let mut oversized = vec![0u8; 16];
		let mut lying = boxed(b"PRVW", &preview_body(1620, 1080));
		lying[0..4].copy_from_slice(&u32::MAX.to_be_bytes());
		oversized.extend_from_slice(&lying);
		assert_eq!(found(boxed(b"uuid", &oversized)), None);

		// A size word smaller than the header it sits in.
		let mut runt = boxed(b"uuid", &[0u8; 32]);
		runt[0..4].copy_from_slice(&4u32.to_be_bytes());
		assert_eq!(found(runt), None);

		// The 64-bit escape, pointing past the end.
		let mut escape = 1u32.to_be_bytes().to_vec();
		escape.extend_from_slice(b"PRVW");
		escape.extend_from_slice(&u64::MAX.to_be_bytes());
		escape.extend_from_slice(&preview_body(800, 600));
		assert_eq!(found(escape), None);

		// A preview box whose descriptor is not followed by a JPEG.
		assert_eq!(found(boxed(b"PRVW", &[0x41u8; 200])), None);

		// A preview box with nothing in it at all.
		assert_eq!(found(boxed(b"PRVW", &[])), None);

		// Truncated headers and empty input.
		assert_eq!(found(b"\x00\x00\x00".to_vec()), None);
		assert_eq!(found(Vec::new()), None);
	}

	/// A tree wide enough to exhaust the box budget must terminate. A
	/// zero-size box would otherwise never advance the cursor.
	#[test]
	fn a_forged_tree_terminates() {
		// `size == 0` means "to the end of the parent", so the walk must treat
		// it as consuming the rest rather than looping on a zero-width box.
		let mut zero = 0u32.to_be_bytes().to_vec();
		zero.extend_from_slice(b"free");
		zero.extend_from_slice(&[0u8; 64]);
		assert_eq!(found(zero), None);

		// Thousands of empty boxes: capped, not walked.
		let mut many = Vec::new();
		for _ in 0..4096 {
			many.extend_from_slice(&boxed(b"free", &[]));
		}
		assert_eq!(found(many), None);

		// Deeply nested uuid boxes: depth-capped.
		let mut nested = boxed(b"PRVW", &preview_body(320, 240));
		for _ in 0..16 {
			let mut wrapper = vec![0u8; 16];
			wrapper.extend_from_slice(&nested);
			nested = boxed(b"uuid", &wrapper);
		}
		assert_eq!(found(nested), None);
	}

	/// Only Canon's brand is claimed. An ordinary MP4 has to fall through to
	/// the "unsupported" answer, not be claimed and then refused.
	#[test]
	fn only_the_canon_brand_is_claimed() {
		assert!(!Cr3.detect(b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00"));
		assert!(!Cr3.detect(b"not a container"));
		assert!(!Cr3.detect(b""));
		// Claimed but empty: an error, and no panic.
		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"mdat", &[0u8; 64]));
		let spec = ThumbSpec::new(512, 512, crate::DEFAULT_MEM_BUDGET);
		assert!(Cr3.open(Box::new(MemSource(file)), &spec).is_err());
	}

	/// A real, decodable JPEG: `raw::prepare` opens the preview with the JPEG
	/// decoder, which needs more than an SOF to say yes.
	fn real_jpeg(w: u16, h: u16) -> Vec<u8> {
		let mut out = Vec::new();
		let enc = jpeg_encoder::Encoder::new(&mut out, 80);
		enc.encode(
			&vec![0x80u8; usize::from(w) * usize::from(h) * 3],
			w,
			h,
			jpeg_encoder::ColorType::Rgb,
		)
		.expect("encode");
		out
	}

	/// `CMT1`'s payload: a little-endian TIFF whose IFD0 holds one
	/// Orientation entry.
	fn cmt1(orientation: u16) -> Vec<u8> {
		let mut tiff = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
		tiff.extend_from_slice(&1u16.to_le_bytes());
		tiff.extend_from_slice(&0x0112u16.to_le_bytes());
		tiff.extend_from_slice(&3u16.to_le_bytes());
		tiff.extend_from_slice(&1u32.to_le_bytes());
		tiff.extend_from_slice(&u32::from(orientation).to_le_bytes());
		tiff.extend_from_slice(&0u32.to_le_bytes());
		tiff
	}

	/// The real layout: `moov` -> Canon `uuid` -> `CMT1` beside `THMB`, and
	/// the `PRVW` in its own versioned `uuid`.
	fn cr3(cmt1_body: Option<&[u8]>) -> Vec<u8> {
		let mut canon = vec![0u8; 16];
		if let Some(body) = cmt1_body {
			canon.extend_from_slice(&boxed(b"CMT1", body));
		}
		let mut thmb = vec![0u8; 16];
		thmb.extend_from_slice(&real_jpeg(160, 120));
		canon.extend_from_slice(&boxed(b"THMB", &thmb));

		let mut versioned = vec![0u8; 16];
		versioned.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
		let mut prvw = vec![0u8; 16];
		prvw.extend_from_slice(&real_jpeg(640, 480));
		versioned.extend_from_slice(&boxed(b"PRVW", &prvw));

		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"moov", &boxed(b"uuid", &canon)));
		file.extend_from_slice(&boxed(b"uuid", &versioned));
		file
	}

	fn orientation_of(file: Vec<u8>) -> u8 {
		let spec = ThumbSpec::new(512, 512, crate::DEFAULT_MEM_BUDGET);
		let prepared = Cr3
			.open(Box::new(MemSource(file)), &spec)
			.expect("a preview");
		assert_eq!(prepared.dims(), (640, 480), "the PRVW must win over THMB");
		prepared.orientation()
	}

	/// The preview JPEGs are bare — no APP1 — so the only orientation is the
	/// file's, in `CMT1`. Reading it is what keeps a portrait shot upright:
	/// before it was read, every portrait CR3 came out on its side.
	#[test]
	fn the_orientation_comes_from_cmt1_not_the_bare_preview() {
		assert_eq!(orientation_of(cr3(Some(&cmt1(6)))), 6);
		assert_eq!(orientation_of(cr3(Some(&cmt1(1)))), 1);
		assert_eq!(orientation_of(cr3(None)), 1);
	}

	/// Located rather than decoded: the PRVW's own range and SOF dimensions,
	/// the `CMT1` orientation, and a splice point behind the JFIF APP0 the
	/// encoder wrote — the preview carries no EXIF, so the rotation has to be
	/// spliced in for a viewer to see it.
	#[test]
	fn the_prvw_is_located_with_the_cmt1_orientation() {
		let file = cr3(Some(&cmt1(8)));
		let prvw = real_jpeg(640, 480);
		let offset = file
			.windows(prvw.len())
			.position(|w| w == prvw.as_slice())
			.expect("the PRVW JPEG is in the file") as u64;
		let located = locate_preview(&mut MemSource(file))
			.unwrap()
			.expect("a preview");
		assert_eq!((located.offset, located.len), (offset, prvw.len() as u64));
		assert_eq!((located.width, located.height), (640, 480));
		assert_eq!(located.orientation, 8);
		// SOI (2), then the 16-byte JFIF APP0 with its marker and length.
		assert_eq!(located.exif_insert_at, Some(2 + 2 + 16));
		// THMB alone is under the preview floor: a thumbnail, not a preview.
		let mut thmb_only = vec![0u8; 16];
		let mut thmb = vec![0u8; 16];
		thmb.extend_from_slice(&real_jpeg(160, 120));
		thmb_only.extend_from_slice(&boxed(b"THMB", &thmb));
		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"moov", &boxed(b"uuid", &thmb_only)));
		let spec = ThumbSpec::new(64, 64, crate::DEFAULT_MEM_BUDGET);
		assert!(Cr3.open(Box::new(MemSource(file.clone())), &spec).is_ok());
		assert_eq!(locate_preview(&mut MemSource(file)).unwrap(), None);
	}

	/// `CMT1` is attacker-controlled bytes like every other box: garbage, a
	/// value out of range, an IFD0 pointer past the read cap and an empty box
	/// all fall back to upright, never to a panic.
	#[test]
	fn a_forged_cmt1_falls_back_to_upright() {
		assert_eq!(orientation_of(cr3(Some(&[0x41u8; 64]))), 1);
		assert_eq!(orientation_of(cr3(Some(&cmt1(9)))), 1);
		assert_eq!(orientation_of(cr3(Some(&[]))), 1);
		let mut far = b"II\x2a\x00".to_vec();
		far.extend_from_slice(&u32::MAX.to_le_bytes());
		far.resize(80 * 1024, 0);
		assert_eq!(orientation_of(cr3(Some(&far))), 1);
	}
}
