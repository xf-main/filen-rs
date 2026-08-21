//! PNG via the `png` crate's scanline API: rows fold straight into the sink,
//! so working memory is a couple of row buffers plus the inflate window no
//! matter how large the image is. Adam7-interlaced files deliver pixels out
//! of row order across passes; those fall back to a whole-frame decode whose
//! cost is declared honestly and left to the budget check (~3 MP under the
//! default budget). APNG animation is ignored — the first frame is the image.

use std::io::BufReader;

use png::{ColorType, Transformations};

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SeqReader, SmallImage, ThumbError,
	ThumbSpec,
};

pub struct Png;

impl FormatDecoder for Png {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
	}

	fn open(
		&self,
		src: Box<dyn ByteSource>,
		_spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let mut decoder = png::Decoder::new(BufReader::new(SeqReader::new(src)));
		// 8-bit depth and expanded palettes/greyscale: every output row is
		// then one of four fixed layouts to convert.
		decoder.set_transformations(Transformations::normalize_to_color8());
		let reader = decoder.read_info().map_err(decode_err)?;
		let info = reader.info();
		let dims = (info.width, info.height);
		let interlaced = info.interlaced;
		Ok(Box::new(PreparedPng {
			reader,
			dims,
			interlaced,
		}))
	}
}

struct PreparedPng {
	reader: png::Reader<BufReader<SeqReader>>,
	dims: (u32, u32),
	interlaced: bool,
}

impl PreparedPng {
	fn row_rgba_len(&self) -> usize {
		self.dims.0 as usize * 4
	}
}

impl PreparedDecode for PreparedPng {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn embedded_preview(&mut self) -> Result<Option<SmallImage>, ThumbError> {
		// The eXIf chunk can theoretically carry a thumbnail; in practice PNGs
		// with one are vanishingly rare and rows already stream cheaply.
		Ok(None)
	}

	fn peak_estimate(&self) -> usize {
		if self.interlaced {
			// next_frame materialises the whole deinterlaced image.
			self.reader.output_buffer_size().unwrap_or(usize::MAX / 4) + self.row_rgba_len()
		} else {
			// The decoder's two row buffers (current + previous for filters),
			// the inflate window, and our RGBA conversion row.
			let row = self
				.reader
				.output_buffer_size()
				.unwrap_or(usize::MAX / 4)
				.div_ceil(self.dims.1.max(1) as usize);
			row * 3 + 64 * 1024 + self.row_rgba_len()
		}
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let (w, _) = self.dims;
		let color = self.reader.output_color_type().0;
		let mut rgba_row = vec![0u8; self.row_rgba_len()];
		if self.interlaced {
			let Some(size) = self.reader.output_buffer_size() else {
				return Err(ThumbError::Decode("png output size overflows".into()));
			};
			let mut buf = vec![0u8; size];
			let out = self.reader.next_frame(&mut buf).map_err(decode_err)?;
			let row_len = out.line_size;
			for (y, row) in buf
				.chunks_exact(row_len)
				.take(out.height as usize)
				.enumerate()
			{
				convert_row(color, row, &mut rgba_row)?;
				sink.push(0, y as u32, w, &rgba_row)?;
			}
			return Ok(());
		}
		let mut y = 0u32;
		while let Some(row) = self.reader.next_row().map_err(decode_err)? {
			convert_row(color, row.data(), &mut rgba_row)?;
			sink.push(0, y, w, &rgba_row)?;
			y += 1;
		}
		Ok(())
	}
}

fn decode_err(e: png::DecodingError) -> ThumbError {
	ThumbError::Decode(format!("png: {e}"))
}

/// After `normalize_to_color8` the row is 8-bit in one of these layouts.
fn convert_row(color: ColorType, row: &[u8], rgba: &mut [u8]) -> Result<(), ThumbError> {
	let px = rgba.len() / 4;
	let expected = match color {
		ColorType::Grayscale => px,
		ColorType::GrayscaleAlpha => px * 2,
		ColorType::Rgb => px * 3,
		ColorType::Rgba => px * 4,
		ColorType::Indexed => {
			return Err(ThumbError::Decode(
				"png palette survived normalize_to_color8".into(),
			));
		}
	};
	if row.len() < expected {
		return Err(ThumbError::Decode("png row shorter than declared".into()));
	}
	match color {
		ColorType::Grayscale => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row) {
				dst.copy_from_slice(&[*l, *l, *l, 255]);
			}
		}
		ColorType::GrayscaleAlpha => {
			for (dst, la) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
				dst.copy_from_slice(&[la[0], la[0], la[0], la[1]]);
			}
		}
		ColorType::Rgb => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
				dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
			}
		}
		ColorType::Rgba => rgba.copy_from_slice(&row[..expected]),
		ColorType::Indexed => unreachable!("rejected above"),
	}
	Ok(())
}
