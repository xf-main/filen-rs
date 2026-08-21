//! End-to-end pipeline tests over fixtures generated at runtime — no binary
//! fixture files, ever.

use std::io::Cursor;

use image::{ImageFormat, Rgb, RgbImage};
use microthumb::{DEFAULT_MEM_BUDGET, MemSource, ThumbSpec, generate};

fn spec(target: u32) -> ThumbSpec {
	ThumbSpec {
		target_width: target,
		target_height: target,
		mem_budget: DEFAULT_MEM_BUDGET,
	}
}

fn encode(image: &RgbImage, format: ImageFormat) -> Vec<u8> {
	let mut bytes = Vec::new();
	image
		.write_to(&mut Cursor::new(&mut bytes), format)
		.unwrap();
	bytes
}

fn checkerboard(w: u32, h: u32) -> RgbImage {
	RgbImage::from_fn(w, h, |x, y| {
		if (x / 2 + y / 2) % 2 == 0 {
			Rgb([0, 0, 0])
		} else {
			Rgb([255, 255, 255])
		}
	})
}

fn mean_channel(rgba: &[u8], channel: usize) -> u32 {
	let mut sum = 0u64;
	let mut n = 0u64;
	for px in rgba.chunks_exact(4) {
		sum += u64::from(px[channel]);
		n += 1;
	}
	(sum / n.max(1)) as u32
}

#[test]
fn jpeg_baseline_downscales_a_checkerboard_to_grey() {
	let bytes = encode(&checkerboard(512, 512), ImageFormat::Jpeg);
	let result = generate(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("baseline jpeg must thumbnail");
	assert!(result.width >= 64 && result.width <= 512);
	assert_eq!(result.width, result.height);
	let grey = mean_channel(&result.rgba, 0);
	assert!(
		(107..=147).contains(&grey),
		"checkerboard should average to ~127, got {grey}"
	);
}

#[test]
fn png_rows_stream_and_preserve_aspect() {
	let image = RgbImage::from_fn(1000, 500, |x, _| Rgb([(x % 256) as u8, 0, 0]));
	let bytes = encode(&image, ImageFormat::Png);
	let result = generate(Box::new(MemSource(bytes)), &spec(100))
		.unwrap()
		.expect("png must thumbnail");
	// 2:1 aspect survives the canvas.
	let ratio = f64::from(result.width) / f64::from(result.height);
	assert!(
		(1.8..=2.2).contains(&ratio),
		"aspect drifted: {}x{}",
		result.width,
		result.height
	);
}

#[test]
fn small_sources_are_never_upscaled() {
	let bytes = encode(&checkerboard(16, 16), ImageFormat::Png);
	let result = generate(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("small png must thumbnail");
	assert_eq!((result.width, result.height), (16, 16));
}

#[test]
fn webp_over_budget_is_refused_but_streaming_png_is_not() {
	// Same pixel count, tiny budget: the full-frame format is refused, the
	// row-streaming one sails through — the whole point of per-format peaks.
	let image = checkerboard(600, 600);
	let tiny = ThumbSpec {
		target_width: 64,
		target_height: 64,
		mem_budget: 1024 * 1024,
	};
	let webp = encode(&image, ImageFormat::WebP);
	assert_eq!(generate(Box::new(MemSource(webp)), &tiny).unwrap(), None);
	let png = encode(&image, ImageFormat::Png);
	assert!(generate(Box::new(MemSource(png)), &tiny).unwrap().is_some());
}

#[test]
fn unsupported_bytes_are_ok_none_not_an_error() {
	let bytes = b"this is not an image at all, not even close".to_vec();
	assert_eq!(
		generate(Box::new(MemSource(bytes)), &spec(64)).unwrap(),
		None
	);
}

#[test]
fn truncated_jpeg_is_an_error_not_a_thumbnail() {
	let mut bytes = encode(&checkerboard(512, 512), ImageFormat::Jpeg);
	bytes.truncate(bytes.len() / 3);
	assert!(generate(Box::new(MemSource(bytes)), &spec(64)).is_err());
}

/// SOI + SOF2 (progressive) header for an image of the given size — enough
/// for `read_info`, no entropy data. Assembled by hand because no pure-Rust
/// encoder writes progressive JPEG.
fn progressive_jpeg_header(width: u16, height: u16) -> Vec<u8> {
	let mut bytes = vec![0xFF, 0xD8]; // SOI
	bytes.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]); // DQT, table 0
	bytes.extend_from_slice(&[16u8; 64]);
	bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08]); // SOF2, 1 component
	bytes.extend_from_slice(&height.to_be_bytes());
	bytes.extend_from_slice(&width.to_be_bytes());
	bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
	bytes
}

#[test]
fn a_large_progressive_jpeg_routes_to_the_dc_parser() {
	// 8000×6000 progressive: the full decode's coefficient buffer (~3 B/px =
	// 144 MB) dwarfs the budget, so the DC-scan path takes over. This header
	// has no scan data, so the observable is the DC parser's own truncation
	// error — proof of the routing (the old behavior was a silent None).
	let bytes = progressive_jpeg_header(8000, 6000);
	let err = generate(Box::new(MemSource(bytes)), &spec(512)).unwrap_err();
	assert!(err.to_string().contains("dc-scan"), "got: {err}");
}

#[test]
fn a_small_progressive_jpeg_is_at_least_attempted() {
	// Same header at 400×300 fits the budget, so the pipeline proceeds into
	// the decode — which then fails on the truncated entropy data. The error
	// (vs None above) is what distinguishes "attempted" from "refused".
	let bytes = progressive_jpeg_header(400, 300);
	assert!(generate(Box::new(MemSource(bytes)), &spec(512)).is_err());
}

/// A JPEG whose EXIF APP1 carries orientation 6 and an embedded thumbnail of
/// a *green* image, while the main image is red — so tests can tell exactly
/// which path produced the pixels.
fn jpeg_with_exif_thumbnail(orientation_: u16) -> Vec<u8> {
	let thumb = encode(
		&RgbImage::from_pixel(160, 120, Rgb([0, 255, 0])),
		ImageFormat::Jpeg,
	);
	// TIFF payload: IFD0 {orientation, next → IFD1}, IFD1 {thumb offset/len}.
	let mut tiff = Vec::new();
	tiff.extend_from_slice(b"II");
	tiff.extend_from_slice(&42u16.to_le_bytes());
	tiff.extend_from_slice(&8u32.to_le_bytes());
	tiff.extend_from_slice(&1u16.to_le_bytes());
	tiff.extend_from_slice(&0x0112u16.to_le_bytes());
	tiff.extend_from_slice(&3u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&u32::from(orientation_).to_le_bytes());
	tiff.extend_from_slice(&26u32.to_le_bytes());
	tiff.extend_from_slice(&2u16.to_le_bytes());
	tiff.extend_from_slice(&0x0201u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&56u32.to_le_bytes());
	tiff.extend_from_slice(&0x0202u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&(thumb.len() as u32).to_le_bytes());
	tiff.extend_from_slice(&0u32.to_le_bytes());
	assert_eq!(tiff.len(), 56);
	tiff.extend_from_slice(&thumb);

	let main = encode(
		&RgbImage::from_pixel(300, 200, Rgb([255, 0, 0])),
		ImageFormat::Jpeg,
	);
	let mut exif_payload = b"Exif\0\0".to_vec();
	exif_payload.extend_from_slice(&tiff);
	let app1_len = (exif_payload.len() + 2) as u16;
	let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1];
	bytes.extend_from_slice(&app1_len.to_be_bytes());
	bytes.extend_from_slice(&exif_payload);
	bytes.extend_from_slice(&main[2..]); // main image sans its SOI
	bytes
}

#[test]
fn the_exif_thumbnail_serves_small_requests_without_a_full_decode() {
	let bytes = jpeg_with_exif_thumbnail(1);
	let result = generate(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("the embedded thumbnail must serve this");
	// Green means the preview path; red would mean the main image decoded.
	assert!(
		mean_channel(&result.rgba, 1) > 200,
		"expected the green thumb"
	);
	assert!(mean_channel(&result.rgba, 0) < 50);
}

#[test]
fn exif_orientation_rotates_the_result() {
	// Orientation 6 = rotate 90° clockwise: the 300×200 red main image must
	// come out portrait. Target large enough that the 160px thumb cannot
	// serve it (160*2 < 512), forcing the main decode + rotation.
	let bytes = jpeg_with_exif_thumbnail(6);
	let result = generate(Box::new(MemSource(bytes)), &spec(512))
		.unwrap()
		.expect("must decode the main image");
	assert!(mean_channel(&result.rgba, 0) > 200, "expected the red main");
	assert!(
		result.height > result.width,
		"orientation 6 must rotate to portrait, got {}x{}",
		result.width,
		result.height
	);
}

#[test]
fn gif_and_bmp_and_tiff_thumbnail_within_their_caps() {
	for format in [ImageFormat::Gif, ImageFormat::Bmp, ImageFormat::Tiff] {
		let bytes = encode(&checkerboard(200, 100), format);
		let result = generate(Box::new(MemSource(bytes)), &spec(50))
			.unwrap()
			.unwrap_or_else(|| panic!("{format:?} must thumbnail"));
		assert!(result.width >= 50, "{format:?}");
	}
}

// ---- progressive DC-scan path ----

fn progressive_jpeg(image: &RgbImage, sampling: jpeg_encoder::SamplingFactor) -> Vec<u8> {
	let mut out = Vec::new();
	let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
	enc.set_progressive(true);
	enc.set_sampling_factor(sampling);
	enc.encode(
		image.as_raw(),
		image.width() as u16,
		image.height() as u16,
		jpeg_encoder::ColorType::Rgb,
	)
	.unwrap();
	out
}

/// A ByteSource that remembers the furthest byte it was ever asked for —
/// how the prefix-only property gets asserted.
struct CountingSource(MemSource, std::sync::Arc<std::sync::atomic::AtomicU64>);

impl microthumb::ByteSource for CountingSource {
	fn len(&self) -> u64 {
		self.0.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.0.read_at(offset, buf)?;
		self.1
			.fetch_max(offset + n as u64, std::sync::atomic::Ordering::Relaxed);
		Ok(n)
	}
}

/// Per-channel mean absolute error between the DC-path output and the block
/// means of the original image, over blocks fully inside the image.
fn dc_mae(original: &RgbImage, dc: &microthumb::SmallImage) -> f64 {
	let bw = original.width() / 8;
	let bh = original.height() / 8;
	assert!(dc.width >= bw && dc.height >= bh);
	let mut total = 0f64;
	let mut n = 0f64;
	for by in 0..bh {
		for bx in 0..bw {
			let mut mean = [0f64; 3];
			for y in 0..8 {
				for x in 0..8 {
					let px = original.get_pixel(bx * 8 + x, by * 8 + y);
					for (m, v) in mean.iter_mut().zip(px.0) {
						*m += f64::from(v);
					}
				}
			}
			let o = ((by * dc.width + bx) * 4) as usize;
			for (c, m) in mean.iter().enumerate() {
				total += (m / 64.0 - f64::from(dc.rgba[o + c])).abs();
				n += 1.0;
			}
		}
	}
	total / n
}

fn smooth_image(w: u32, h: u32) -> RgbImage {
	RgbImage::from_fn(w, h, |x, y| {
		Rgb([
			(x * 255 / w.max(1)) as u8,
			(y * 255 / h.max(1)) as u8,
			((x + y) * 127 / (w + h).max(1)) as u8,
		])
	})
}

/// Budget that the full progressive decode cannot fit but the DC path can —
/// forcing the routing without needing a huge fixture.
fn dc_forcing_spec(target: u32) -> ThumbSpec {
	ThumbSpec {
		target_width: target,
		target_height: target,
		mem_budget: 400_000,
	}
}

#[test]
fn dc_parse_matches_the_full_decode_within_tolerance() {
	let image = smooth_image(512, 384);
	for (sampling, tolerance) in [
		(jpeg_encoder::SamplingFactor::F_1_1, 6.0),
		// 4:2:0 chroma covers 16×16 per block; a bit more drift is expected.
		(jpeg_encoder::SamplingFactor::F_2_2, 10.0),
	] {
		let bytes = progressive_jpeg(&image, sampling);
		let result = generate(Box::new(MemSource(bytes)), &dc_forcing_spec(64))
			.unwrap()
			.expect("dc path must produce a preview");
		assert_eq!((result.width, result.height), (64, 48));
		let mae = dc_mae(&image, &result);
		assert!(
			mae <= tolerance,
			"MAE {mae} over tolerance {tolerance} for {sampling:?}"
		);
	}
}

#[test]
fn dc_parse_handles_grayscale() {
	let mut out = Vec::new();
	let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
	enc.set_progressive(true);
	let luma: Vec<u8> = (0..200u32 * 120)
		.map(|i| ((i % 200) * 255 / 200) as u8)
		.collect();
	enc.encode(&luma, 200, 120, jpeg_encoder::ColorType::Luma)
		.unwrap();
	let result = generate(Box::new(MemSource(out)), &dc_forcing_spec(32))
		.unwrap()
		.expect("grayscale dc path must produce a preview");
	// Grey means R==G==B everywhere.
	for px in result.rgba.chunks_exact(4) {
		assert_eq!(px[0], px[1]);
		assert_eq!(px[1], px[2]);
	}
}

#[test]
fn dc_parse_reads_only_a_prefix_of_the_file() {
	// Real-entropy content so the AC scans (which the DC path must never
	// read) dominate the file.
	let image = RgbImage::from_fn(2000, 1500, |x, y| {
		Rgb([
			(x * 7 % 251) as u8,
			(y * 13 % 249) as u8,
			((x ^ y) % 256) as u8,
		])
	});
	let bytes = progressive_jpeg(&image, jpeg_encoder::SamplingFactor::F_2_2);
	let len = bytes.len() as u64;
	let watermark = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
	let source = CountingSource(MemSource(bytes), watermark.clone());
	let result = generate(Box::new(source), &spec(512)).unwrap();
	result.expect("large progressive must produce a dc preview");
	let read = watermark.load(std::sync::atomic::Ordering::Relaxed);
	eprintln!(
		"dc prefix: read {read} of {len} bytes ({:.1}%)",
		read as f64 * 100.0 / len as f64
	);
	assert!(
		read * 2 < len,
		"dc path read {read} of {len} bytes — not a prefix decode"
	);
}

#[test]
fn hostile_progressive_streams_error_cleanly() {
	// Truncated mid-scan: routed to the DC parser, which must error, not hang
	// or panic.
	let image = smooth_image(512, 384);
	let mut bytes = progressive_jpeg(&image, jpeg_encoder::SamplingFactor::F_2_2);
	bytes.truncate(bytes.len() / 20);
	assert!(generate(Box::new(MemSource(bytes)), &dc_forcing_spec(64)).is_err());

	// A DHT whose counts exceed 256 symbols.
	let mut bad_dht = progressive_jpeg_header(4000, 3000);
	bad_dht.extend_from_slice(&[0xFF, 0xC4, 0x01, 0x15, 0x00]);
	bad_dht.extend_from_slice(&[16u8; 16]);
	bad_dht.extend_from_slice(&[0u8; 256]);
	bad_dht.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
	assert!(generate(Box::new(MemSource(bad_dht)), &spec(512)).is_err());
}

/// A progressive JPEG built to overflow the DC dequantiser: a 16-bit DQT with
/// the largest quantizer, a DC scan at the largest point transform the probe
/// admits (`Al` = 7 — jpeg-decoder rejects a shift past the frame precision
/// before our parser ever sees the file), and every block carrying the largest
/// DC magnitude 8-bit precision allows, so the predictor accumulates across
/// 4096 blocks. The product runs to ~1e14, far outside i32, and
/// `[profile.test]` keeps overflow-checks on: before the widening this
/// panicked with "attempt to multiply with overflow", breaking the module's
/// no-panic contract on hostile input. Sized and starved so the routing sends
/// it to the DC parser rather than a full decode — an image small enough to
/// decode normally never reaches this code at all.
#[test]
fn a_dc_scan_with_a_huge_quantizer_and_shift_does_not_panic() {
	const DIM: u16 = 512;
	const AL: u8 = 7;
	const CAT: u8 = 11;

	let mut bytes = vec![0xFF, 0xD8]; // SOI
	// DQT, 16-bit precision (Pq=1), table 0, DC quantizer = 65535.
	bytes.extend_from_slice(&[0xFF, 0xDB]);
	bytes.extend_from_slice(&(2u16 + 1 + 128).to_be_bytes());
	bytes.push(0x10);
	for _ in 0..64 {
		bytes.extend_from_slice(&65535u16.to_be_bytes());
	}
	// SOF2: one component, quant table 0.
	bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08]);
	bytes.extend_from_slice(&DIM.to_be_bytes());
	bytes.extend_from_slice(&DIM.to_be_bytes());
	bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
	// DHT, DC table 0: a single one-bit code "0" meaning category CAT.
	bytes.extend_from_slice(&[0xFF, 0xC4]);
	bytes.extend_from_slice(&(2u16 + 1 + 16 + 1).to_be_bytes());
	bytes.push(0x00); // class 0 (DC), table 0
	bytes.push(1); // one code of length 1
	bytes.extend_from_slice(&[0u8; 15]);
	bytes.push(CAT);
	// SOS: one component, Ss=0, Se=0, Ah=0, Al=AL.
	bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, AL]);

	// Entropy: per block, code "0" then CAT one-bits (maximum positive
	// magnitude). 0xFF is byte-stuffed as required inside a scan.
	let blocks = u64::from(DIM.div_ceil(8)).pow(2);
	let (mut acc, mut n) = (0u32, 0u32);
	let mut scan = Vec::new();
	let push = |bit: u32, acc: &mut u32, n: &mut u32, out: &mut Vec<u8>| {
		*acc = (*acc << 1) | bit;
		*n += 1;
		if *n == 8 {
			let byte = *acc as u8;
			out.push(byte);
			if byte == 0xFF {
				out.push(0);
			}
			*acc = 0;
			*n = 0;
		}
	};
	for _ in 0..blocks {
		push(0, &mut acc, &mut n, &mut scan);
		for _ in 0..CAT {
			push(1, &mut acc, &mut n, &mut scan);
		}
	}
	while n != 0 {
		push(1, &mut acc, &mut n, &mut scan);
	}
	bytes.extend_from_slice(&scan);
	bytes.extend_from_slice(&[0xFF, 0xD9]); // EOI

	let starved = ThumbSpec {
		target_width: 64,
		target_height: 64,
		mem_budget: 400 * 1024,
	};
	let result = generate(Box::new(MemSource(bytes)), &starved)
		.expect("a hostile DC scan must not error out here")
		.expect("the DC parser must still produce a preview");
	// Saturated rather than wrapped: the clamp is what the widening buys.
	assert!(result.rgba.chunks_exact(4).all(|px| px[0] == 255));
}

/// Zero dimensions in a progressive SOF2 must be refused, not divided by.
/// The DC parser guards `width == 0 || height == 0` explicitly; nothing
/// exercised it until now, so the guard shipped untested.
#[test]
fn a_progressive_jpeg_with_zero_dimensions_is_refused() {
	for (w, h) in [(0u16, 300u16), (300, 0), (0, 0)] {
		let bytes = progressive_jpeg_header(w, h);
		let result = generate(Box::new(MemSource(bytes)), &spec(512));
		assert!(
			matches!(result, Ok(None) | Err(_)),
			"{w}x{h} must not produce a thumbnail"
		);
	}
}

#[test]
fn a_progressive_jpeg_past_the_dc_block_cap_still_serves_its_embedded_thumbnail() {
	// The real case this comes from: a 188 MP progressive library scan whose DC
	// planes (4.4M blocks) exceed the parser's pre-allocation cap. `open` refuses
	// — and used to propagate that refusal out of the pipeline, discarding the
	// EXIF thumbnail it had already parsed, because the orchestrator only looks
	// at the preview AFTER `open` returns. A refusal must degrade to the
	// cheapest path, not abort the whole request.
	//
	// 20000x20000 at 4:2:0 is 6.25M DC blocks, three times the cap.
	let thumb = encode(
		&RgbImage::from_pixel(160, 120, Rgb([0, 255, 0])),
		ImageFormat::Jpeg,
	);
	let mut tiff = Vec::new();
	tiff.extend_from_slice(b"II");
	tiff.extend_from_slice(&42u16.to_le_bytes());
	tiff.extend_from_slice(&8u32.to_le_bytes());
	tiff.extend_from_slice(&1u16.to_le_bytes());
	tiff.extend_from_slice(&0x0112u16.to_le_bytes());
	tiff.extend_from_slice(&3u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&26u32.to_le_bytes());
	tiff.extend_from_slice(&2u16.to_le_bytes());
	tiff.extend_from_slice(&0x0201u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&56u32.to_le_bytes());
	tiff.extend_from_slice(&0x0202u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&(thumb.len() as u32).to_le_bytes());
	tiff.extend_from_slice(&0u32.to_le_bytes());
	assert_eq!(tiff.len(), 56);
	tiff.extend_from_slice(&thumb);

	let mut exif_payload = b"Exif\0\0".to_vec();
	exif_payload.extend_from_slice(&tiff);
	let app1_len = (exif_payload.len() + 2) as u16;
	let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1];
	bytes.extend_from_slice(&app1_len.to_be_bytes());
	bytes.extend_from_slice(&exif_payload);
	// SOF2 with 4:2:0 sampling, far past the DC parser's block cap.
	bytes.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
	bytes.extend_from_slice(&[16u8; 64]);
	bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x11, 0x08]);
	bytes.extend_from_slice(&20000u16.to_be_bytes()); // height
	bytes.extend_from_slice(&20000u16.to_be_bytes()); // width
	bytes.push(3);
	bytes.extend_from_slice(&[0x01, 0x22, 0x00]); // Y  h=2 v=2
	bytes.extend_from_slice(&[0x02, 0x11, 0x00]); // Cb
	bytes.extend_from_slice(&[0x03, 0x11, 0x00]); // Cr

	let result = generate(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("the embedded thumbnail must survive the DC parser's refusal");
	assert!(
		mean_channel(&result.rgba, 1) > 200,
		"expected the green embedded thumb"
	);
}
