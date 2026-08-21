//! Memory-bounded thumbnail decoding.
//!
//! Built for hosts with hard memory ceilings (an iOS file-provider extension
//! gets ~20 MB before jetsam). Nothing in this crate ever materialises a full
//! decoded frame of an arbitrarily large image: every format either streams
//! (rows, tiles, IDCT-scaled output) into a fixed-size accumulator canvas, or
//! is refused up front by an honest per-decode peak estimate checked against
//! the caller's budget. Fully synchronous — async bridging and any notion of
//! where bytes come from live in the caller's [`ByteSource`].

mod exif;
mod formats;
pub(crate) mod sink;
mod source;

pub use sink::{BoxAccumulator, PixelSink, SmallImage};
pub use source::{ByteSource, FileSource, MemSource, SeqReader};

use crate::sink::accumulator_bytes;

/// Default decode budget: what is realistically left of a 20 MB jetsam limit
/// after the host process' own baseline.
pub const DEFAULT_MEM_BUDGET: usize = 12 * 1024 * 1024;

/// The accumulator canvas never exceeds this on its long side — oversampling
/// past ~2× the largest sane thumbnail target buys nothing. (The budget cap
/// in [`canvas_dims`] usually binds first, at the per-pixel canvas cost
/// `accumulator_bytes` documents.)
const MAX_CANVAS_LONG_SIDE: u32 = 1600;

/// Anti-DoS ceiling on total decode WORK, which the per-unit memory budget
/// cannot bound on its own: the streaming decoders' peaks are per row / strip
/// / tile, so a forged header (a 1×2³⁰ PNG, a `RowsPerStrip=1` TIFF with a
/// huge height) passes the memory check and then runs an effectively unbounded
/// loop. Source area caps the number of units every streaming format can emit.
/// Deliberately GENEROUS — ~20× a 24 MP photo, far above any camera or export;
/// this refuses degenerate forgeries, never real pictures.
const MAX_SOURCE_AREA_PIXELS: u64 = 512_000_000;

/// Companion to [`MAX_SOURCE_AREA_PIXELS`] for degenerate shapes: a
/// 1×500-million-pixel ribbon passes an area cap alone while still driving
/// hundreds of millions of per-row iterations. No real image has a side
/// beyond a million pixels.
const MAX_SOURCE_SIDE_PIXELS: u32 = 1 << 20;

/// The largest square target `mem_budget` could hold a canvas for, leaving
/// half the budget for the decoder.
///
/// Callers ask for what their UI would like — iOS requests 2048×2048 for every
/// item regardless of the display size, because the system caches one
/// thumbnail per item and rescales it for every use. Treating that as a decode
/// target made the whole pipeline refuse: an in-format scaler (JPEG's IDCT
/// scale) has no room to shrink when the target is near the source size, so
/// the estimate stayed at full resolution and blew the budget. The requested
/// size is an upper bound, not a demand — clamp it to what is affordable and
/// return the best image that fits.
fn affordable_target(mem_budget: usize) -> u32 {
	let max_px = (mem_budget / 2 / 20).max(1);
	(max_px as f64).sqrt() as u32
}

#[derive(Debug, Clone, Copy)]
pub struct ThumbSpec {
	pub target_width: u32,
	pub target_height: u32,
	/// Hard ceiling on decoder peak allocation + accumulator canvas, bytes.
	pub mem_budget: usize,
}

/// Which path produced a thumbnail.
///
/// Reported rather than logged: this crate stays free of a logging dependency,
/// and the caller is the one with a tracing subscriber and an item id to name.
/// Worth surfacing because the two paths differ by orders of magnitude in cost
/// — an embedded preview is a couple of container reads, a decode can be a
/// whole-file fetch — so "are we hitting the fast path?" is the first question
/// asked whenever this pipeline is touched, and it is invisible from the
/// outside otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbSource {
	/// The image's own embedded thumbnail (EXIF IFD1, HEIF `thmb` item).
	EmbeddedPreview,
	/// Decoded from the full image, downscaled into the canvas.
	Decoded,
}

impl std::fmt::Display for ThumbSource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ThumbSource::EmbeddedPreview => f.write_str("embedded preview"),
			ThumbSource::Decoded => f.write_str("full decode"),
		}
	}
}

/// A thumbnail and the path that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
	pub image: SmallImage,
	pub source: ThumbSource,
}

#[derive(Debug)]
pub enum ThumbError {
	Io(std::io::Error),
	/// The bitstream is corrupt or the decoder refused it.
	Decode(String),
	/// A decoder pushed pixels outside the geometry it declared.
	Geometry,
}

impl std::fmt::Display for ThumbError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ThumbError::Io(e) => write!(f, "thumbnail source io error: {e}"),
			ThumbError::Decode(msg) => write!(f, "thumbnail decode error: {msg}"),
			ThumbError::Geometry => write!(f, "decoder pushed pixels outside its declared bounds"),
		}
	}
}

impl std::error::Error for ThumbError {}

impl From<std::io::Error> for ThumbError {
	fn from(e: std::io::Error) -> Self {
		ThumbError::Io(e)
	}
}

/// One image format's entry point.
pub trait FormatDecoder: Send + Sync {
	/// Magic-byte sniff on the first bytes of the source. Mime types are a
	/// hint upstream; bytes are the truth.
	fn detect(&self, prefix: &[u8]) -> bool;

	/// Parses the header only — reads a few KB. The spec is available so
	/// formats with in-decoder scaling (JPEG's IDCT scale) can commit to an
	/// output resolution here and report it via
	/// [`PreparedDecode::output_dims`].
	fn open(
		&self,
		src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError>;
}

/// A source with its header parsed, ready to either hand over an embedded
/// preview or decode for real.
pub trait PreparedDecode {
	/// Source dimensions, as declared by the header.
	fn dims(&self) -> (u32, u32);

	/// The coordinate space [`decode_into`](Self::decode_into) will push —
	/// differs from [`dims`](Self::dims) when the decoder scales during
	/// decode.
	fn output_dims(&self) -> (u32, u32) {
		self.dims()
	}

	/// EXIF orientation (1–8) to apply to whatever this decode produces.
	fn orientation(&self) -> u8 {
		1
	}

	/// A cheap embedded preview (EXIF IFD1 thumb, HEIF `thmb` item), read
	/// from the header region only.
	fn embedded_preview(&mut self) -> Result<Option<SmallImage>, ThumbError>;

	/// Contract: [`decode_into`](Self::decode_into) never allocates more than
	/// this, including the decoder's internal state. The orchestrator refuses
	/// decodes whose estimate (plus the accumulator canvas) exceeds budget.
	fn peak_estimate(&self) -> usize;

	fn decode_into(self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError>;
}

/// Canvas dimensions: the decoder-output aspect, scaled so the canvas still
/// covers a 2× oversampled aspect-fill crop of the target, capped at
/// [`MAX_CANVAS_LONG_SIDE`] and at `canvas_budget`, and never upscaled above
/// the output.
///
/// `canvas_budget` is what is left of the caller's budget once the decoder's
/// own peak is subtracted, rather than a flat half: a row-streaming decoder
/// costs a few hundred KB, and charging it half the budget shrank the canvas —
/// and so the thumbnail — for no reason.
pub(crate) fn canvas_dims(
	output: (u32, u32),
	target: (u32, u32),
	canvas_budget: usize,
) -> (u32, u32) {
	let (ow, oh) = (u64::from(output.0.max(1)), u64::from(output.1.max(1)));
	let (tw, th) = (u64::from(target.0.max(1)), u64::from(target.1.max(1)));
	// Fill semantics: scale by the larger target/output ratio, oversampled 2×.
	// Work in per-mille to stay in integers; saturate at 1:1.
	let mut scale_mille = ((2000 * tw).div_ceil(ow))
		.max((2000 * th).div_ceil(oh))
		.min(1000);
	// Area cap from the budget; see `accumulator_bytes` for the per-pixel cost.
	let max_px = (canvas_budget as u64 / 20).max(1);
	while scale_mille > 1
		&& (ow * scale_mille / 1000).max(1) * (oh * scale_mille / 1000).max(1) > max_px
	{
		// Integer bisection beats floating sqrt here: a handful of steps, no
		// rounding surprises.
		scale_mille = scale_mille * 9 / 10;
	}
	let mut cw = (ow * scale_mille / 1000).max(1);
	let mut ch = (oh * scale_mille / 1000).max(1);
	let long = cw.max(ch);
	if long > u64::from(MAX_CANVAS_LONG_SIDE) {
		cw = (cw * u64::from(MAX_CANVAS_LONG_SIDE) / long).max(1);
		ch = (ch * u64::from(MAX_CANVAS_LONG_SIDE) / long).max(1);
	}
	(cw.min(ow) as u32, ch.min(oh) as u32)
}

/// Whether a preview is big enough to serve the request: its long side covers
/// at least half the requested long side.
fn preview_suffices(preview: &SmallImage, spec: &ThumbSpec) -> bool {
	u64::from(preview.width.max(preview.height)) * 2
		>= u64::from(spec.target_width.max(spec.target_height))
}

fn apply_orientation(image: SmallImage, orientation: u8) -> SmallImage {
	if orientation <= 1 {
		return image;
	}
	let Some(buffer) = image::RgbaImage::from_vec(image.width, image.height, image.rgba) else {
		return SmallImage {
			width: 0,
			height: 0,
			rgba: Vec::new(),
		};
	};
	let Some(orientation) = image::metadata::Orientation::from_exif(orientation) else {
		let (width, height) = buffer.dimensions();
		return SmallImage {
			width,
			height,
			rgba: buffer.into_raw(),
		};
	};
	let mut dynamic = image::DynamicImage::ImageRgba8(buffer);
	dynamic.apply_orientation(orientation);
	let rgba = dynamic.into_rgba8();
	let (width, height) = rgba.dimensions();
	SmallImage {
		width,
		height,
		rgba: rgba.into_raw(),
	}
}

/// Produces a downscaled (never exact-size, never upscaled) RGBA image for
/// the request, or `Ok(None)` when the source is unsupported or nothing fits
/// the memory budget. The caller does the final exact resize + encode — both
/// operate on buffers this crate already bounded.
pub fn generate(
	mut src: Box<dyn ByteSource>,
	spec: &ThumbSpec,
) -> Result<Option<Thumbnail>, ThumbError> {
	let mut prefix = [0u8; 64];
	let n = src.read_at(0, &mut prefix)?;
	let Some(format) = formats::sniff(&prefix[..n]) else {
		return Ok(None);
	};
	// Clamped before `open`, because a decoder with an in-format scaler commits
	// to its output resolution there.
	let cap = affordable_target(spec.mem_budget);
	let spec = &ThumbSpec {
		target_width: spec.target_width.min(cap),
		target_height: spec.target_height.min(cap),
		mem_budget: spec.mem_budget,
	};
	let mut prepared = format.open(src, spec)?;
	let orientation = prepared.orientation();

	// A broken preview must not fail a decodable image — previews are an
	// optimisation, the real decode is the contract. Budget-gated like every
	// other path: the preview plus the one rotated copy `apply_orientation`
	// may hold concurrently must fit (2× its RGBA — 8 bytes per pixel); an
	// oversized "preview" falls through to the bounded decode instead.
	let mut preview = prepared
		.embedded_preview()
		.unwrap_or_default()
		.filter(|p| p.rgba.len().saturating_mul(2) <= spec.mem_budget);
	if let Some(preview) = preview.take_if(|p| preview_suffices(p, spec)) {
		return Ok(Some(Thumbnail {
			image: apply_orientation(preview, orientation),
			source: ThumbSource::EmbeddedPreview,
		}));
	}

	let source_dims = prepared.dims();
	let output = prepared.output_dims();
	if output.0 == 0 || output.1 == 0 {
		return Ok(None);
	}
	let over_work_ceiling = u64::from(source_dims.0) * u64::from(source_dims.1)
		> MAX_SOURCE_AREA_PIXELS
		|| source_dims.0.max(source_dims.1) > MAX_SOURCE_SIDE_PIXELS;
	if !over_work_ceiling {
		let peak = prepared.peak_estimate();
		let canvas = canvas_dims(
			output,
			(spec.target_width, spec.target_height),
			spec.mem_budget.saturating_sub(peak),
		);
		if peak
			.checked_add(accumulator_bytes(canvas))
			.is_some_and(|total| total <= spec.mem_budget)
		{
			let mut acc = BoxAccumulator::new(output, canvas);
			prepared.decode_into(&mut acc)?;
			return Ok(Some(Thumbnail {
				image: apply_orientation(acc.finish(), orientation),
				source: ThumbSource::Decoded,
			}));
		}
	}

	// Over budget (or over the work ceiling): an undersized preview beats no
	// thumbnail at all.
	Ok(preview.map(|preview| Thumbnail {
		image: apply_orientation(preview, orientation),
		source: ThumbSource::EmbeddedPreview,
	}))
}

#[cfg(test)]
mod canvas_tests {
	use super::{DEFAULT_MEM_BUDGET, canvas_dims};
	use crate::sink::accumulator_bytes;

	// `canvas_dims` is handed what is LEFT for the canvas, not the whole
	// budget: the orchestrator subtracts the decoder's own peak first.
	#[test]
	fn covers_the_target_and_fits_the_canvas_budget() {
		// 8000×6000 source, 512² target: the canvas keeps the source aspect,
		// covers the target, and its accumulator fits what it was given.
		// A cheap decoder leaves most of the budget for the canvas, and then
		// the canvas covers the target outright.
		let canvas_budget = DEFAULT_MEM_BUDGET;
		let (cw, ch) = canvas_dims((8000, 6000), (512, 512), canvas_budget);
		assert!(ch >= 512, "short side must cover the target, got {ch}");
		assert!(cw <= 1600 && ch <= 1600);
		assert!(accumulator_bytes((cw, ch)) <= canvas_budget + 20);
		// Small sources are never upscaled.
		assert_eq!(canvas_dims((100, 50), (512, 512), canvas_budget), (100, 50));
		// A decoder that eats most of the budget still gets a canvas, just a
		// coarser one — degrading beats refusing.
		let (cw, ch) = canvas_dims((8000, 6000), (512, 512), DEFAULT_MEM_BUDGET / 8);
		assert!(cw >= 1 && ch >= 1);
		assert!(accumulator_bytes((cw, ch)) <= DEFAULT_MEM_BUDGET / 8 + 20);
	}

	#[test]
	fn a_bigger_canvas_budget_buys_a_bigger_canvas() {
		// The point of spending what the decoder does not need: the same
		// source and target yield a larger canvas when more is available.
		let small = canvas_dims((8000, 6000), (512, 512), DEFAULT_MEM_BUDGET / 8);
		let large = canvas_dims((8000, 6000), (512, 512), DEFAULT_MEM_BUDGET / 2);
		assert!(
			large.0 > small.0 && large.1 > small.1,
			"{large:?} should exceed {small:?}"
		);
	}

	#[test]
	fn caps_the_long_side_and_survives_extreme_aspect_ratios() {
		let (cw, ch) = canvas_dims((100_000, 400), (1024, 1024), DEFAULT_MEM_BUDGET / 2);
		assert!(cw <= 1600);
		assert!(ch >= 1);
		// A tiny budget still yields a usable (if coarse) canvas.
		let (cw, ch) = canvas_dims((8000, 6000), (512, 512), 100_000);
		assert!(cw >= 1 && ch >= 1);
		assert!(accumulator_bytes((cw, ch)) <= 100_000 + 20);
	}

	#[test]
	fn an_oversized_request_is_clamped_to_what_the_budget_can_serve() {
		// iOS asks every provider for 2048x2048 regardless of display size.
		// Honouring that literally made the pipeline refuse every real photo;
		// the request is an upper bound, so it clamps instead.
		let cap = super::affordable_target(DEFAULT_MEM_BUDGET);
		assert!(cap < 2048, "a 12 MB budget cannot serve a 2048 canvas");
		assert!(cap >= 256, "but it must still serve a useful thumbnail");
	}
}
