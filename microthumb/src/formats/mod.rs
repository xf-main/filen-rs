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
#[cfg(feature = "heif")]
static HEIF: heif::Heif = heif::Heif;

/// Sniff order is cheapest-most-common first; `Simple` last because it is
/// the grab-bag.
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
	];
	registry.iter().copied().find(|d| d.detect(prefix))
}
