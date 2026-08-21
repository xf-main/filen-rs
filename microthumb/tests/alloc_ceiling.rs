//! The memory contract, enforced: a counting allocator measures the high-water
//! mark of `thumb()` on large fixtures and asserts it stays inside the
//! budget. On macOS the meter is libmalloc's `malloc_logger` hook, which sees
//! every malloc/realloc/free in the process — including the C side
//! (libheif/libde265), which a Rust `#[global_allocator]` cannot observe.
//! NOT visible to either meter: thread stacks (libde265 spawns workers) and
//! direct mmap/vm_allocate — the numbers here are heap high-water marks, not
//! total process footprint. Elsewhere the global allocator swap meters the
//! pure-Rust formats only. This lives in its own test binary because both
//! meters are process-wide, and runs the cases in ONE #[test] so no parallel
//! test pollutes the counters.

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use image::{ImageFormat, Rgb, RgbImage};
use microthumb::{DEFAULT_MEM_BUDGET, MemSource, ThumbSpec, generate};

/// Most tests care only about the pixels. The ones that care about WHICH path
/// produced them call `generate` directly and inspect `Thumbnail::source`.
fn thumb(
	src: Box<dyn microthumb::ByteSource>,
	spec: &ThumbSpec,
) -> Result<Option<microthumb::SmallImage>, microthumb::ThumbError> {
	Ok(generate(src, spec)?.map(|t| t.image))
}

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn on_alloc(size: usize) {
	let now = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
	PEAK.fetch_max(now, Ordering::Relaxed);
}

#[cfg(not(target_os = "macos"))]
mod meter {
	use std::alloc::{GlobalAlloc, Layout, System};
	use std::sync::atomic::Ordering;

	use super::{CURRENT, on_alloc};

	struct Counting;

	unsafe impl GlobalAlloc for Counting {
		unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
			on_alloc(layout.size());
			unsafe { System.alloc(layout) }
		}

		unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
			CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
			unsafe { System.dealloc(ptr, layout) }
		}

		unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
			if new_size > layout.size() {
				on_alloc(new_size - layout.size());
			} else {
				CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
			}
			unsafe { System.realloc(ptr, layout, new_size) }
		}
	}

	#[global_allocator]
	static ALLOC: Counting = Counting;

	pub fn install() {}

	pub fn dropped() -> usize {
		0
	}
}

/// libmalloc's logging hook. Installing a `malloc_logger` gets a callback for
/// every malloc/realloc/free across all zones in this process — Rust's
/// `System` allocator funnels through libmalloc too, so this single meter
/// subsumes the global-allocator one AND sees libheif/libde265.
///
/// Free and realloc events do not carry the old block's size, so the callback
/// keeps a fixed-size open-addressing table of live (ptr, size) pairs. The
/// callback runs inside malloc itself and therefore must never allocate: the
/// table is static, insertion is CAS-only, and overflow is counted (never
/// dropped silently) — an untracked block inflates CURRENT forever, which only
/// over-reports the peak, and `dropped()` is asserted zero regardless.
#[cfg(target_os = "macos")]
mod meter {
	use std::sync::Once;
	use std::sync::atomic::{AtomicUsize, Ordering};

	use super::{CURRENT, on_alloc};

	// stack_logging.h: event type bits.
	const TYPE_ALLOC: u32 = 2;
	const TYPE_DEALLOC: u32 = 4;
	const FLAG_ZONE: u32 = 8;

	type Logger = unsafe extern "C" fn(u32, usize, usize, usize, usize, u32);

	unsafe extern "C" {
		static mut malloc_logger: Option<Logger>;
	}

	const SLOTS: usize = 1 << 20;
	const PROBE_LIMIT: usize = 1024;
	const TOMBSTONE: usize = usize::MAX;

	static PTRS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
	static SIZES: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
	static DROPPED: AtomicUsize = AtomicUsize::new(0);

	fn slot(ptr: usize, step: usize) -> usize {
		(((ptr >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) + step) & (SLOTS - 1)
	}

	/// The malloc callback for `ptr` runs before malloc returns it, so no other
	/// thread can race an insert of the same pointer with its own free.
	fn insert(ptr: usize, size: usize) {
		for step in 0..PROBE_LIMIT {
			let s = &PTRS[slot(ptr, step)];
			let found = s.load(Ordering::Relaxed);
			if (found == 0 || found == TOMBSTONE)
				&& s.compare_exchange(found, ptr, Ordering::AcqRel, Ordering::Relaxed)
					.is_ok()
			{
				SIZES[slot(ptr, step)].store(size, Ordering::Release);
				return;
			}
		}
		DROPPED.fetch_add(1, Ordering::Relaxed);
	}

	/// Size the pointer was tracked with, or 0 for blocks allocated before the
	/// hook was installed (their free is a no-op on the counters).
	fn remove(ptr: usize) -> usize {
		for step in 0..PROBE_LIMIT {
			let idx = slot(ptr, step);
			let found = PTRS[idx].load(Ordering::Relaxed);
			if found == ptr {
				let size = SIZES[idx].load(Ordering::Acquire);
				PTRS[idx].store(TOMBSTONE, Ordering::Release);
				return size;
			}
			if found == 0 {
				return 0;
			}
		}
		0
	}

	unsafe extern "C" fn logger(ty: u32, p1: usize, p2: usize, p3: usize, result: usize, _n: u32) {
		// With the zone flag, p1 is the zone and the payload shifts to p2/p3.
		let (a, b) = if ty & FLAG_ZONE != 0 {
			(p2, p3)
		} else {
			(p1, p2)
		};
		match ty & (TYPE_ALLOC | TYPE_DEALLOC) {
			t if t == TYPE_ALLOC | TYPE_DEALLOC => {
				// realloc: a = old ptr, b = new size, result = new ptr.
				let old = remove(a);
				if result != 0 {
					insert(result, b);
					on_alloc(b);
				}
				CURRENT.fetch_sub(old, Ordering::Relaxed);
			}
			TYPE_ALLOC => {
				// malloc/calloc/valloc: a = size, result = the block.
				if result != 0 {
					insert(result, a);
					on_alloc(a);
				}
			}
			TYPE_DEALLOC => {
				// free: a = the block.
				let old = remove(a);
				CURRENT.fetch_sub(old, Ordering::Relaxed);
			}
			_ => {}
		}
	}

	pub fn install() {
		static ONCE: Once = Once::new();
		ONCE.call_once(|| unsafe {
			malloc_logger = Some(logger);
		});
	}

	pub fn dropped() -> usize {
		DROPPED.load(Ordering::Relaxed)
	}
}

/// Peak extra bytes allocated while `f` ran, on top of what was already live.
fn measured_peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
	meter::install();
	let baseline = CURRENT.load(Ordering::Relaxed);
	PEAK.store(baseline, Ordering::Relaxed);
	let out = f();
	let peak = PEAK.load(Ordering::Relaxed);
	(out, peak.saturating_sub(baseline))
}

fn spec(target: u32) -> ThumbSpec {
	ThumbSpec {
		target_width: target,
		target_height: target,
		mem_budget: DEFAULT_MEM_BUDGET,
	}
}

/// Any real photograph downscales to more than one colour; a decode bug that
/// pushes nothing (or one tile) into the canvas leaves it flat.
#[cfg(feature = "heif")]
fn assert_not_flat(image: &microthumb::SmallImage, what: &str) {
	let first = &image.rgba[..4];
	assert!(
		image.rgba.chunks_exact(4).any(|px| px != first),
		"{what}: decoded image is a single flat colour"
	);
}

#[test]
fn generate_stays_inside_the_budget_for_large_sources() {
	// 24 MP — the class of source the old full-frame pipeline died on
	// (96 MB of RGBA). Fixture encoding happens OUTSIDE the measurement.
	let gradient = RgbImage::from_fn(6000, 4000, |x, y| {
		Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
	});

	let mut jpeg = Vec::new();
	gradient
		.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
		.unwrap();
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(jpeg)), &spec(512)));
	// The input Vec (a few MB) is owned by the source and counted at the
	// baseline of the closure via the move — subtract nothing, just assert
	// the whole thing stays under budget + the moved-in source itself.
	let result = result.unwrap().expect("24 MP baseline jpeg must thumbnail");
	assert!(result.width >= 500 || result.height >= 500);
	eprintln!("24 MP baseline jpeg peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP jpeg peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	let mut png = Vec::new();
	gradient
		.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
		.unwrap();
	drop(gradient);
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(png)), &spec(512)));
	result.unwrap().expect("24 MP png must thumbnail");
	eprintln!("24 MP png peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP png peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// A 60 MP baseline JPEG — beyond any phone sensor — still fits: the IDCT
	// scale keeps the decode output proportional to the target, not the file.
	let big = RgbImage::from_pixel(10000, 6000, Rgb([90, 120, 30]));
	let mut jpeg = Vec::new();
	big.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
		.unwrap();
	drop(big);
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(jpeg)), &spec(512)));
	result.unwrap().expect("60 MP baseline jpeg must thumbnail");
	eprintln!("60 MP baseline jpeg peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"60 MP jpeg peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// 24 MP PROGRESSIVE — the class the full decode can never fit (a ~72 MB
	// coefficient buffer): served by the DC-scan parser, which also must not
	// read past the DC scans at the head of the file.
	let gradient = RgbImage::from_fn(6000, 4000, |x, y| {
		Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
	});
	let mut jpeg = Vec::new();
	{
		let mut enc = jpeg_encoder::Encoder::new(&mut jpeg, 85);
		enc.set_progressive(true);
		enc.encode(gradient.as_raw(), 6000, 4000, jpeg_encoder::ColorType::Rgb)
			.unwrap();
	}
	drop(gradient);
	let len = jpeg.len() as u64;
	let watermark = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
	let source = CountingSource(MemSource(jpeg), watermark.clone());
	let (result, peak) = measured_peak(|| thumb(Box::new(source), &spec(512)));
	result
		.unwrap()
		.expect("24 MP progressive jpeg must thumbnail via DC scans");
	let read = watermark.load(Ordering::Relaxed);
	eprintln!(
		"24 MP progressive jpeg peak: {peak} bytes; prefix read: {read} of {len} bytes ({:.1}%)",
		read as f64 * 100.0 / len as f64
	);
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP progressive peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);
	assert!(read * 2 < len, "dc path read {read} of {len} bytes");

	// 12 MP GIF — the row streamer's peak is palettes + two rows.
	let mut gif_bytes = Vec::new();
	{
		let mut palette = Vec::new();
		for i in 0..=255u8 {
			palette.extend_from_slice(&[i, i.wrapping_mul(3), i.wrapping_mul(7)]);
		}
		let mut enc = gif::Encoder::new(&mut gif_bytes, 4000, 3000, &palette).unwrap();
		let pixels: Vec<u8> = (0..4000usize * 3000).map(|i| (i % 256) as u8).collect();
		enc.write_frame(&gif::Frame::from_indexed_pixels(4000, 3000, pixels, None))
			.unwrap();
	}
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(gif_bytes)), &spec(512)));
	result.unwrap().expect("12 MP gif must thumbnail");
	eprintln!("12 MP gif peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"12 MP gif peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// 24 MP striped TIFF — peak is one 16-row strip.
	let mut tiff_bytes = Vec::new();
	{
		let data: Vec<u8> = (0..6000usize * 4000 * 3).map(|i| (i % 253) as u8).collect();
		let mut enc = tiff::encoder::TiffEncoder::new(Cursor::new(&mut tiff_bytes)).unwrap();
		let mut img = enc
			.new_image::<tiff::encoder::colortype::RGB8>(6000, 4000)
			.unwrap();
		img.rows_per_strip(16).unwrap();
		img.write_data(&data).unwrap();
	}
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(tiff_bytes)), &spec(512)));
	result.unwrap().expect("24 MP striped tiff must thumbnail");
	eprintln!("24 MP tiff peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP tiff peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// 24 MP BMP (24bpp, 72 MB of pixel data) — the seek-rows path reads one
	// row at a time; the fixture Vec sits at the baseline, not in the peak.
	let mut bmp_bytes = Vec::with_capacity(6000 * 3 * 4000 + 64);
	bmp_bytes.extend_from_slice(b"BM");
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&54u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&40u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&6000i32.to_le_bytes());
	bmp_bytes.extend_from_slice(&4000i32.to_le_bytes());
	bmp_bytes.extend_from_slice(&1u16.to_le_bytes());
	bmp_bytes.extend_from_slice(&24u16.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&[0u8; 8]);
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	// 6000*3 = 18000 bytes/row, already 4-aligned; identical rows are fine
	// for a memory test.
	let row: Vec<u8> = (0..6000usize * 3).map(|i| (i % 251) as u8).collect();
	for _ in 0..4000 {
		bmp_bytes.extend_from_slice(&row);
	}
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bmp_bytes)), &spec(512)));
	result.unwrap().expect("24 MP bmp must thumbnail");
	eprintln!("24 MP bmp peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP bmp peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	#[cfg(feature = "heif")]
	heif_cases();

	assert_eq!(meter::dropped(), 0, "the live-block table overflowed");
}

/// See tests/pipeline.rs — duplicated because each test binary is its own
/// crate and ten lines beat a shared test-support module.
struct CountingSource(MemSource, std::sync::Arc<std::sync::atomic::AtomicU64>);

impl microthumb::ByteSource for CountingSource {
	fn len(&self) -> u64 {
		self.0.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.0.read_at(offset, buf)?;
		self.1.fetch_max(offset + n as u64, Ordering::Relaxed);
		Ok(n)
	}
}

/// A real device HEIC (`iphone_img.heic` at the workspace root, or
/// `MICROTHUMB_HEIF_FIXTURE`) exercises both HEIF paths against the C side of
/// the pipeline — which only the macOS malloc_logger meter can see; the
/// global-allocator meter under-counts these two cases and their ceilings are
/// asserted for the C-aware meter's benefit. HEVC cannot be encoded at test
/// time (the vendored libheif is decode-only), so this is the one fixture-file
/// dependency; the case skips loudly when the file is absent.
#[cfg(feature = "heif")]
fn heif_cases() {
	let path = std::env::var("MICROTHUMB_HEIF_FIXTURE").unwrap_or_else(|_| {
		format!(
			"{}/../iphone_img.heic",
			env!("CARGO_MANIFEST_DIR").replace('\\', "/")
		)
	});
	let Ok(bytes) = std::fs::read(&path) else {
		eprintln!(
			"SKIP heif cases: no fixture at {path} — the HEIF embedded-preview and \
			 tile-decode memory ceilings are NOT being proven by this run (CI has no \
			 fixture; supply one via MICROTHUMB_HEIF_FIXTURE to exercise them)"
		);
		return;
	};

	// Embedded-thumbnail path: a device HEIC carries a `thmb` item (~320–685 px
	// depending on iOS vintage), which suffices for a 512 target — no tile is
	// ever decoded, and the result IS the preview, whose dimensions anchor the
	// tile-path proof below.
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bytes.clone())), &spec(512)));
	let preview = result.unwrap().expect("device heic must thumbnail at 512");
	assert_not_flat(&preview, "heif embedded preview");
	eprintln!("heif embedded-preview peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"heif preview peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// Tile path. The requested target alone can no longer force it: an
	// oversized request is clamped to what the budget can serve, so asking for
	// 2048 under the default budget just serves the embedded preview again.
	// A budget large enough to make the clamped target exceed twice the
	// preview's long side is what sends the decode through the grid — and the
	// peak is then asserted against THAT budget, since the canvas is entitled
	// to spend it. A strictly larger long side than the 512-target run is the
	// proof the tiles actually ran, whatever thumb this fixture carries.
	const TILE_BUDGET: usize = 96 * 1024 * 1024;
	let tile_spec = ThumbSpec {
		target_width: 2048,
		target_height: 2048,
		mem_budget: TILE_BUDGET,
	};
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bytes)), &tile_spec));
	let result = result.unwrap().expect("device heic must thumbnail at 2048");
	assert!(
		result.width.max(result.height) > preview.width.max(preview.height),
		"expected the tile-decoded canvas to out-resolve the {}x{} preview, got {}x{}",
		preview.width,
		preview.height,
		result.width,
		result.height
	);
	assert_not_flat(&result, "heif tile decode");
	eprintln!("heif tile-decode peak: {peak} bytes");
	assert!(
		peak <= TILE_BUDGET,
		"heif tile decode peaked at {peak} bytes (budget {TILE_BUDGET})"
	);
}
