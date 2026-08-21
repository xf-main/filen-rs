#[cfg(feature = "heif")]
mod heif;
mod jpeg;
mod png;
mod simple;

use crate::FormatDecoder;

static JPEG: jpeg::Jpeg = jpeg::Jpeg;
static PNG: png::Png = png::Png;
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
		&SIMPLE,
	];
	registry.iter().copied().find(|d| d.detect(prefix))
}
