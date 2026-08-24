mod bmp;
mod cr3;
mod gif;
#[cfg(feature = "heif")]
mod heif;
mod jpeg;
mod jpeg_dc;
mod png;
mod raf;
pub(crate) mod raw;
mod simple;
#[cfg(feature = "svg")]
mod svg;
mod tiff;

use crate::FormatDecoder;

static JPEG: jpeg::Jpeg = jpeg::Jpeg;
static PNG: png::Png = png::Png;
static GIF: gif::Gif = gif::Gif;
static TIFF: tiff::Tiff = tiff::Tiff;
static BMP: bmp::Bmp = bmp::Bmp;
static RAF: raf::Raf = raf::Raf;
static CR3: cr3::Cr3 = cr3::Cr3;
static SIMPLE: simple::Simple = simple::Simple;
#[cfg(feature = "svg")]
static SVG: svg::Svg = svg::Svg;
#[cfg(feature = "heif")]
static HEIF: heif::Heif = heif::Heif;

/// Sniff order is cheapest-most-common first; `Simple` late because it is
/// the grab-bag, and `Svg` dead last: it sniffs text shape rather than a
/// magic number, so every binary format must get its look first.
pub(crate) fn sniff(prefix: &[u8]) -> Option<&'static dyn FormatDecoder> {
	let registry: &[&'static dyn FormatDecoder] = &[
		&JPEG,
		&PNG,
		#[cfg(feature = "heif")]
		&HEIF,
		&GIF,
		&TIFF,
		&BMP,
		&RAF,
		&CR3,
		&SIMPLE,
		#[cfg(feature = "svg")]
		&SVG,
	];
	registry.iter().copied().find(|d| d.detect(prefix))
}
