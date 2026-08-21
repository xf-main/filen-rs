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
fn a_large_progressive_jpeg_is_refused_by_the_budget() {
	// 8000×6000 progressive: the coefficient buffer alone (~3 B/px = 144 MB)
	// dwarfs the budget, so the pipeline must answer None without decoding.
	// (The header is all that is ever read — proof the refusal is up-front.)
	let bytes = progressive_jpeg_header(8000, 6000);
	assert_eq!(
		generate(Box::new(MemSource(bytes)), &spec(512)).unwrap(),
		None
	);
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
