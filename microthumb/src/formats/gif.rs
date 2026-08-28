//! GIF via the `gif` crate's push parser (`StreamingDecoder`): LZW index
//! bytes stream through one row buffer into the sink, so working memory is
//! O(width) no matter the logical screen size. Only the first frame is
//! decoded — for a thumbnail an animation IS its first frame. Interlaced
//! frames need no reordering pass: each decoded row is pushed at its true
//! display y, and the accumulator takes rows in any order.

use gif::streaming_decoder::{Block, Decoded, OutputBuffer, StreamingDecoder};

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SmallImage, ThumbError, ThumbSpec,
};

pub struct Gif;

fn err(msg: &str) -> ThumbError {
	ThumbError::Decode(format!("gif: {msg}"))
}

impl FormatDecoder for Gif {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a")
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		_spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		// The logical screen descriptor is a fixed 13-byte prefix; no need to
		// spin up the decoder to read two little-endian dims.
		let mut header = [0u8; 13];
		let n = src.read_at(0, &mut header)?;
		if n < 13 {
			return Err(err("truncated header"));
		}
		let width = u32::from(u16::from_le_bytes([header[6], header[7]]));
		let height = u32::from(u16::from_le_bytes([header[8], header[9]]));
		if width == 0 || height == 0 {
			return Err(err("zero logical screen"));
		}
		Ok(Box::new(PreparedGif {
			src,
			dims: (width, height),
		}))
	}
}

struct PreparedGif {
	src: Box<dyn ByteSource>,
	dims: (u32, u32),
}

impl PreparedDecode for PreparedGif {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn embedded_preview(&mut self, _mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(None)
	}

	fn peak_estimate(&self) -> usize {
		// One index row + one RGBA row + the decoder's LZW state and
		// palettes + the input buffer.
		self.dims.0 as usize * 5 + 96 * 1024
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let (screen_w, screen_h) = self.dims;
		let mut decoder = StreamingDecoder::new();
		let mut global_palette: Option<Box<[u8]>> = None;
		let mut background: Option<u8> = None;

		// Current-frame state, set at FrameMetadata.
		struct FrameState {
			left: u32,
			top: u32,
			width: u32,
			height: u32,
			interlaced: bool,
			palette: Option<Vec<u8>>,
			transparent: Option<u8>,
			row: u32,
			filled: usize,
		}
		let mut frame: Option<FrameState> = None;

		let mut input = [0u8; 8192];
		let mut pos = 0u64;
		let mut index_row = vec![0u8; screen_w as usize];
		let mut rgba_row = vec![0u8; screen_w as usize * 4];

		loop {
			let n = self.src.read_at(pos, &mut input)?;
			if n == 0 {
				return Err(err("truncated before the first frame finished"));
			}
			let mut consumed_total = 0usize;
			while consumed_total < n {
				let out_slice = match &frame {
					Some(f) => {
						let row_len = f.width as usize;
						&mut index_row[f.filled..row_len]
					}
					None => &mut index_row[0..0],
				};
				let (consumed, event) = decoder
					.update(
						&input[consumed_total..n],
						&mut OutputBuffer::Slice(out_slice),
					)
					.map_err(|e| err(&e.to_string()))?;
				if consumed == 0 && matches!(event, Decoded::Nothing) {
					// The decoder wants output space it will never get (or is
					// wedged); erroring beats spinning forever on hostile input.
					return Err(err("decoder made no progress"));
				}
				consumed_total += consumed;
				match event {
					Decoded::GlobalPalette(palette) => global_palette = Some(palette),
					Decoded::BackgroundColor(idx) => background = Some(idx),
					Decoded::FrameMetadata(_) => {
						let info = decoder.current_frame();
						let f = FrameState {
							left: u32::from(info.left),
							top: u32::from(info.top),
							width: u32::from(info.width),
							height: u32::from(info.height),
							interlaced: info.interlaced,
							palette: info.palette.clone(),
							transparent: info.transparent,
							row: 0,
							filled: 0,
						};
						if f.width == 0 || f.height == 0 {
							return Err(err("empty first frame"));
						}
						if f.left + f.width > screen_w || f.top + f.height > screen_h {
							return Err(err("frame exceeds the logical screen"));
						}
						// The area the first frame does not cover shows the
						// background; push it now (any order is fine) so the
						// canvas has full coverage exactly once.
						push_background(
							sink,
							(screen_w, screen_h),
							(f.left, f.top, f.width, f.height),
							background,
							global_palette.as_deref(),
							&mut rgba_row,
						)?;
						frame = Some(f);
					}
					Decoded::BytesDecoded(count) => {
						let f = frame.as_mut().ok_or_else(|| err("pixels before a frame"))?;
						f.filled += count.get();
						if f.filled >= f.width as usize {
							let palette = f
								.palette
								.as_deref()
								.or(global_palette.as_deref())
								.ok_or_else(|| err("no palette"))?;
							let y = f.top
								+ interlace_display_row(f.row, f.height, f.interlaced)
									.ok_or_else(|| err("more rows than the frame declared"))?;
							indices_to_rgba(
								&index_row[..f.width as usize],
								palette,
								f.transparent,
								&mut rgba_row[..f.width as usize * 4],
							);
							sink.push(f.left, y, f.width, &rgba_row[..f.width as usize * 4])?;
							f.row += 1;
							f.filled = 0;
						}
					}
					Decoded::DataEnd => {
						let f = frame
							.as_ref()
							.ok_or_else(|| err("data end before a frame"))?;
						if f.row < f.height {
							return Err(err("frame ended short of its declared rows"));
						}
						// First frame complete — the thumbnail does not care
						// about the rest of an animation.
						return Ok(());
					}
					// A complete first frame returns at DataEnd above, so any
					// trailer reaching here means the file has no first frame.
					// Bail BEFORE the next update() call: the decoder's trailer
					// state consumes nothing and reports nothing, so feeding it
					// remaining input spins forever inside update() itself —
					// past the reach of the no-progress guard above.
					Decoded::BlockStart(Block::Trailer) => {
						return Err(err("trailer before the first frame"));
					}
					_ => {}
				}
			}
			pos += n as u64;
		}
	}
}

/// Display row for the `i`-th decoded row of a frame — identity for
/// non-interlaced, the four-pass sequence otherwise.
fn interlace_display_row(i: u32, height: u32, interlaced: bool) -> Option<u32> {
	if !interlaced {
		return (i < height).then_some(i);
	}
	let mut seen = 0u32;
	for (start, step) in [(0u32, 8u32), (4, 8), (2, 4), (1, 2)] {
		let rows = if height > start {
			(height - start).div_ceil(step)
		} else {
			0
		};
		if i < seen + rows {
			return Some(start + (i - seen) * step);
		}
		seen += rows;
	}
	None
}

fn indices_to_rgba(indices: &[u8], palette: &[u8], transparent: Option<u8>, rgba: &mut [u8]) {
	for (dst, &idx) in rgba.chunks_exact_mut(4).zip(indices) {
		if transparent == Some(idx) {
			dst.copy_from_slice(&[0, 0, 0, 0]);
			continue;
		}
		let p = idx as usize * 3;
		// A palette can legally be shorter than the largest index in the
		// stream; out-of-range indices show as black rather than erroring.
		let (r, g, b) = match palette.get(p..p + 3) {
			Some(c) => (c[0], c[1], c[2]),
			None => (0, 0, 0),
		};
		dst.copy_from_slice(&[r, g, b, 255]);
	}
}

/// Pushes the background color into the parts of the screen the first
/// frame's rectangle does not cover (its four surrounding bands).
fn push_background(
	sink: &mut dyn PixelSink,
	screen: (u32, u32),
	frame: (u32, u32, u32, u32),
	background: Option<u8>,
	global_palette: Option<&[u8]>,
	rgba_row: &mut [u8],
) -> Result<(), ThumbError> {
	let (sw, sh) = screen;
	let (fx, fy, fw, fh) = frame;
	if (fx, fy, fw, fh) == (0, 0, sw, sh) {
		return Ok(());
	}
	let color = match (background, global_palette) {
		(Some(idx), Some(palette)) => match palette.get(idx as usize * 3..idx as usize * 3 + 3) {
			Some(c) => [c[0], c[1], c[2], 255],
			None => [0, 0, 0, 0],
		},
		_ => [0, 0, 0, 0],
	};
	let mut fill =
		|x: u32, y0: u32, w: u32, rows: u32, rgba_row: &mut [u8]| -> Result<(), ThumbError> {
			if w == 0 || rows == 0 {
				return Ok(());
			}
			for px in rgba_row[..w as usize * 4].chunks_exact_mut(4) {
				px.copy_from_slice(&color);
			}
			for r in 0..rows {
				sink.push(x, y0 + r, w, &rgba_row[..w as usize * 4])?;
			}
			Ok(())
		};
	// Above, below, left, right of the frame rectangle.
	fill(0, 0, sw, fy, rgba_row)?;
	fill(0, fy + fh, sw, sh - fy - fh, rgba_row)?;
	fill(0, fy, fx, fh, rgba_row)?;
	fill(fx + fw, fy, sw - fx - fw, fh, rgba_row)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::interlace_display_row;

	#[test]
	fn interlace_sequence_partitions_the_frame() {
		for height in [1u32, 2, 3, 4, 7, 8, 9, 33] {
			let mut seen: Vec<u32> = (0..height)
				.map(|i| interlace_display_row(i, height, true).unwrap())
				.collect();
			seen.sort_unstable();
			let expected: Vec<u32> = (0..height).collect();
			assert_eq!(seen, expected, "height {height}");
			assert_eq!(interlace_display_row(height, height, true), None);
		}
	}
}
