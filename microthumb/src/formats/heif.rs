//! HEIC/HEIF via the vendored libheif. Two memory-bounded paths, in order:
//! the embedded `thmb` item (every iPhone HEIC carries one — the whole probe
//! costs a couple of container reads), then tile-wise decode of the grid
//! (Apple encodes 512×512 tiles), each tile pushed into the sink and freed
//! before the next. Un-tiled non-Apple HEIFs only offer a whole-frame decode,
//! declared honestly (~8 B/px) and left to the budget check.

use heif_decoder::{HeifSession, HeifTiling};
use image::RgbaImage;

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SeqReader, SmallImage, ThumbError,
	ThumbSpec,
};

pub struct Heif;

/// Embedded HEIF thumbnails are ~320 px; anything bigger is not a thumbnail.
const MAX_PREVIEW_PIXELS: u64 = 1024 * 1024;

const BRANDS: &[&[u8; 4]] = &[
	b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
];

impl FormatDecoder for Heif {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.len() >= 12
			&& &prefix[4..8] == b"ftyp"
			&& BRANDS.iter().any(|brand| &prefix[8..12] == *brand)
	}

	fn open(
		&self,
		src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let len = src.len();
		let mut session = HeifSession::new(SeqReader::new(src), len).map_err(decode_err)?;
		// The container's declared tile dims (our peak_estimate) and the HEVC
		// bitstreams' actual picture sizes are unrelated — and libheif skips
		// its own whole-image size check on the tile path. These context
		// limits are the pre-allocation guard for a lying container: no
		// decoded picture past what the budget could ever admit (~8 B/px of
		// transient decode cost), no total past the budget itself.
		session.set_decode_limits((spec.mem_budget / 8) as u64, spec.mem_budget as u64);
		let dims = session.primary_dims().map_err(decode_err)?;
		let tiling = session.tiling().map_err(decode_err)?;
		Ok(Box::new(PreparedHeif {
			session,
			dims,
			tiling,
		}))
	}
}

struct PreparedHeif {
	session: HeifSession<SeqReader>,
	dims: (u32, u32),
	tiling: Option<HeifTiling>,
}

impl PreparedDecode for PreparedHeif {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn output_dims(&self) -> (u32, u32) {
		// The tiling reports the transformed (display) size, which is also
		// the space decode_image_tile produces; primary_dims matches it for
		// the un-tiled path.
		match &self.tiling {
			Some(tiling) => (tiling.image_width, tiling.image_height),
			None => self.dims,
		}
	}

	fn embedded_preview(&mut self) -> Result<Option<SmallImage>, ThumbError> {
		match self.session.embedded_thumbnail_rgba(MAX_PREVIEW_PIXELS) {
			Ok(rgba) => Ok(rgba.map(small_image)),
			// A corrupt thumbnail item must not fail the tile path.
			Err(_) => Ok(None),
		}
	}

	fn peak_estimate(&self) -> usize {
		// Saturating like simple.rs: these factors are container-declared —
		// a wrapped multiply must saturate into a guaranteed refusal, never
		// into a small estimate that passes the budget gate.
		match &self.tiling {
			// One tile decoded at a time: libheif's internal RGBA plus our
			// copy, ~8 B/px of TILE, independent of image size.
			Some(tiling) => (tiling.tile_width as usize)
				.saturating_mul(tiling.tile_height as usize)
				.saturating_mul(8),
			// Whole frame, same 2× accounting.
			None => (self.dims.0 as usize)
				.saturating_mul(self.dims.1 as usize)
				.saturating_mul(8),
		}
	}

	fn decode_into(self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let Some(tiling) = self.tiling else {
			let rgba = self.session.decode_primary_rgba().map_err(decode_err)?;
			return push_clipped(sink, &rgba, 0, 0, self.dims.0, self.dims.1);
		};
		for row in 0..tiling.num_rows {
			for col in 0..tiling.num_columns {
				let x0 = col * tiling.tile_width;
				let y0 = row * tiling.tile_height;
				if x0 >= tiling.image_width || y0 >= tiling.image_height {
					// A grid may be wider than the image it crops to.
					continue;
				}
				let tile = self
					.session
					.decode_tile_rgba(col, row)
					.map_err(decode_err)?;
				push_clipped(sink, &tile, x0, y0, tiling.image_width, tiling.image_height)?;
			}
		}
		Ok(())
	}
}

/// Pushes an RGBA block at (x0, y0), clipped to the image bounds — edge tiles
/// overhang the true image size.
fn push_clipped(
	sink: &mut dyn PixelSink,
	block: &RgbaImage,
	x0: u32,
	y0: u32,
	image_w: u32,
	image_h: u32,
) -> Result<(), ThumbError> {
	let visible_w = block.width().min(image_w.saturating_sub(x0));
	let visible_h = block.height().min(image_h.saturating_sub(y0));
	if visible_w == 0 || visible_h == 0 {
		return Ok(());
	}
	let stride = block.width() as usize * 4;
	let data = block.as_raw();
	let row_bytes = visible_w as usize * 4;
	for r in 0..visible_h {
		let start = r as usize * stride;
		sink.push(x0, y0 + r, visible_w, &data[start..start + row_bytes])?;
	}
	Ok(())
}

fn small_image(rgba: RgbaImage) -> SmallImage {
	let (width, height) = rgba.dimensions();
	SmallImage {
		width,
		height,
		rgba: rgba.into_raw(),
	}
}

fn decode_err(e: heif_decoder::HeifError) -> ThumbError {
	ThumbError::Decode(format!("{e}"))
}
