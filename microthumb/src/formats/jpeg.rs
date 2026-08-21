//! JPEG via `jpeg-decoder`, chosen over the `image` crate's zune-jpeg backend
//! for one capability: IDCT-scaled decode (1/8, 1/4, 1/2), which makes the
//! output buffer — not the source resolution — the allocation. Baseline
//! images of any size decode in a few MB. Progressive images structurally
//! need whole-image coefficient storage (~3 B/px); that cost goes into
//! `peak_estimate` and the budget check decides, which lands at roughly a
//! 4 MP progressive cap under the default budget.

use std::io::BufReader;

use jpeg_decoder::{CodingProcess, Decoder, PixelFormat};

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SeqReader, SmallImage, ThumbError,
	ThumbSpec, exif,
};

pub struct Jpeg;

/// Embedded EXIF thumbnails are ~160 px; anything claiming to be bigger than
/// this is not a thumbnail and not worth decoding on the preview path.
const MAX_PREVIEW_PIXELS: u64 = 1024 * 1024;

impl FormatDecoder for Jpeg {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(&[0xFF, 0xD8, 0xFF])
	}

	fn open(
		&self,
		src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let mut decoder = Decoder::new(BufReader::new(SeqReader::new(src)));
		decoder.read_info().map_err(decode_err)?;
		let info = decoder
			.info()
			.ok_or_else(|| ThumbError::Decode("jpeg header parsed but no info".into()))?;
		let exif = decoder.exif_data().map(<[u8]>::to_vec);
		let orientation = exif.as_deref().map_or(1, exif::orientation);

		// Ask the IDCT for the target itself: the decoder picks the smallest
		// 1/8..1 factor covering it in at least one axis, which keeps the
		// decode buffer at ~1–2× the target instead of 4× (a 2× ask made a
		// 24 MP source pick 1/4 and blow the budget). The short side can land
		// a few percent under the target; the final fill-resize absorbs it.
		let req_w = spec.target_width.min(u16::MAX.into()) as u16;
		let req_h = spec.target_height.min(u16::MAX.into()) as u16;
		let (out_w, out_h) = decoder.scale(req_w, req_h).map_err(decode_err)?;

		Ok(Box::new(PreparedJpeg {
			decoder,
			src_dims: (u32::from(info.width), u32::from(info.height)),
			out_dims: (u32::from(out_w), u32::from(out_h)),
			progressive: info.coding_process == CodingProcess::DctProgressive,
			exif,
			orientation,
		}))
	}
}

struct PreparedJpeg {
	decoder: Decoder<BufReader<SeqReader>>,
	src_dims: (u32, u32),
	out_dims: (u32, u32),
	progressive: bool,
	exif: Option<Vec<u8>>,
	orientation: u8,
}

impl PreparedDecode for PreparedJpeg {
	fn dims(&self) -> (u32, u32) {
		self.src_dims
	}

	fn output_dims(&self) -> (u32, u32) {
		self.out_dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self) -> Result<Option<SmallImage>, ThumbError> {
		let Some(bytes) = self.exif.as_deref().and_then(exif::embedded_thumbnail) else {
			return Ok(None);
		};
		let mut decoder = Decoder::new(bytes);
		if decoder.read_info().is_err() {
			return Ok(None);
		}
		let sane = decoder.info().is_some_and(|info| {
			u64::from(info.width) * u64::from(info.height) <= MAX_PREVIEW_PIXELS
		});
		if !sane {
			return Ok(None);
		}
		let Ok(pixels) = decoder.decode() else {
			// A corrupt embedded thumbnail must not fail the real decode.
			return Ok(None);
		};
		let info = decoder.info().expect("info available after decode");
		Ok(rgba_image(
			u32::from(info.width),
			u32::from(info.height),
			info.pixel_format,
			&pixels,
		))
	}

	fn peak_estimate(&self) -> usize {
		let (out_w, out_h) = self.out_dims;
		let out_px = out_w as usize * out_h as usize;
		// Three buffers are alive at once at the peak, not one:
		//   * the interleaved RGB image jpeg-decoder returns — 3 B/out px;
		//   * its per-component sample planes, which it fills FIRST and still
		//     holds while building that image (`worker/immediate.rs`, the
		//     `results` vecs; handed to `compute_image` in `decode_planes`) —
		//     1.5 B/out px at 4:2:0, 3 B at 4:4:4, so 3 is the honest bound;
		//   * our own per-row RGBA conversion buffer.
		// Counting only the first two let a 24 MP source pass a 12 MB budget
		// and then allocate ~18 MB.
		let out = out_px * 3 + out_px * 3 + out_w as usize * 4;
		if self.progressive {
			// Whole-image coefficient planes: i16 per sample, so 3 B/src px at
			// 4:2:0 and 6 B at 4:4:4 — structural for progressive, no scale
			// escapes it, and the estimate must survive the unsubsampled case.
			let (w, h) = self.src_dims;
			out + w as usize * h as usize * 6
		} else {
			// One MCU row band (≤16 px tall) of the FULL-width image.
			out + self.src_dims.0 as usize * 16 * 4
		}
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let pixels = self.decoder.decode().map_err(decode_err)?;
		let info = self
			.decoder
			.info()
			.ok_or_else(|| ThumbError::Decode("jpeg decoded but no info".into()))?;
		let (w, h) = self.out_dims;
		push_converted(sink, w, h, info.pixel_format, &pixels)
	}
}

fn decode_err(e: jpeg_decoder::Error) -> ThumbError {
	ThumbError::Decode(format!("jpeg: {e}"))
}

/// Converts one decoded frame to RGBA row by row and pushes it — the row
/// buffer is the only allocation.
fn push_converted(
	sink: &mut dyn PixelSink,
	w: u32,
	h: u32,
	format: PixelFormat,
	pixels: &[u8],
) -> Result<(), ThumbError> {
	let per_px = format.pixel_bytes();
	let row_len = w as usize * per_px;
	if row_len == 0 || pixels.len() < row_len * h as usize {
		return Err(ThumbError::Decode("jpeg output buffer too small".into()));
	}
	let mut rgba_row = vec![0u8; w as usize * 4];
	for y in 0..h {
		let row = &pixels[y as usize * row_len..][..row_len];
		convert_row(format, row, &mut rgba_row);
		sink.push(0, y, w, &rgba_row)?;
	}
	Ok(())
}

fn convert_row(format: PixelFormat, row: &[u8], rgba: &mut [u8]) {
	match format {
		PixelFormat::L8 => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row) {
				dst.copy_from_slice(&[*l, *l, *l, 255]);
			}
		}
		PixelFormat::L16 => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
				dst.copy_from_slice(&[l[0], l[0], l[0], 255]);
			}
		}
		PixelFormat::RGB24 => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
				dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
			}
		}
		PixelFormat::CMYK32 => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
				// jpeg-decoder hands CMYK through unconverted (Adobe inverted).
				let (c, m, y, k) = (
					u16::from(src[0]),
					u16::from(src[1]),
					u16::from(src[2]),
					u16::from(src[3]),
				);
				dst.copy_from_slice(&[
					(c * k / 255) as u8,
					(m * k / 255) as u8,
					(y * k / 255) as u8,
					255,
				]);
			}
		}
	}
}

fn rgba_image(w: u32, h: u32, format: PixelFormat, pixels: &[u8]) -> Option<SmallImage> {
	let per_px = format.pixel_bytes();
	let row_len = w as usize * per_px;
	if w == 0 || h == 0 || pixels.len() < row_len * h as usize {
		return None;
	}
	let mut rgba = vec![0u8; w as usize * h as usize * 4];
	for y in 0..h as usize {
		convert_row(
			format,
			&pixels[y * row_len..][..row_len],
			&mut rgba[y * w as usize * 4..][..w as usize * 4],
		);
	}
	Some(SmallImage {
		width: w,
		height: h,
		rgba,
	})
}
