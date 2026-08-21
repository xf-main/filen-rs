#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use std::{
	ffi::{CStr, CString, c_int, c_void},
	io::{Read, Seek, SeekFrom},
	marker::PhantomData,
};

use image::RgbaImage;

struct HeicContext<'a> {
	inner: *mut heif_context,
	_lifetime: PhantomData<&'a [u8]>,
}

impl HeicContext<'_> {
	fn from_slice(data: &[u8]) -> Result<Self, HeifError> {
		let context = unsafe { heif_context_alloc() };
		// Own the context immediately so it is freed by `Drop` on every early
		// return; otherwise a failed read below leaks it (and its buffers).
		let ctx = HeicContext {
			inner: context,
			_lifetime: PhantomData,
		};
		let result = unsafe {
			heif_context_read_from_memory_without_copy(
				context,
				data.as_ptr() as *const c_void,
				data.len(),
				std::ptr::null(),
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(ctx)
	}

	fn from_file(path: &str) -> Result<HeicContext<'static>, HeifError> {
		let file_name = CString::new(path)
			.map_err(|_| HeifError::invalid_input("file path contains an interior NUL byte"))?;
		let context = unsafe { heif_context_alloc() };
		// Own the context immediately so it is freed by `Drop` on every early
		// return; otherwise a failed read below leaks it (and its buffers).
		let ctx = HeicContext {
			inner: context,
			_lifetime: PhantomData,
		};
		let result =
			unsafe { heif_context_read_from_file(context, file_name.as_ptr(), std::ptr::null()) };
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(ctx)
	}

	fn from_reader<T: Read + Seek>(reader: &mut HeifReader<T>) -> Result<Self, HeifError> {
		let context = unsafe { heif_context_alloc() };
		// Own the context immediately so it is freed by `Drop` on every early
		// return; otherwise a failed read below leaks it (and its buffers).
		let ctx = HeicContext {
			inner: context,
			_lifetime: PhantomData,
		};
		// The vtable pointer must be the one stored INSIDE the reader:
		// libheif keeps it and calls through it lazily on every later decode,
		// so a temporary table here would dangle the moment this call returns.
		let vtable = &raw const reader.vtable;
		let result = unsafe {
			heif_context_read_from_reader(
				context,
				vtable,
				reader as *mut _ as *mut c_void,
				std::ptr::null(),
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(ctx)
	}
}

impl Drop for HeicContext<'_> {
	fn drop(&mut self) {
		unsafe { heif_context_free(self.inner) };
	}
}

struct ImageHandle<'a> {
	inner: *mut heif_image_handle,
	_lifetime: PhantomData<&'a HeicContext<'a>>,
}

impl ImageHandle<'_> {
	fn new(ctx: &HeicContext) -> Result<Self, HeifError> {
		let mut handle = std::ptr::null_mut();
		let result = unsafe { heif_context_get_primary_image_handle(ctx.inner, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok {
			// copy the message while `ctx`, which owns the buffer it points into, is alive
			return Err(HeifError::from_raw(result));
		}

		Ok(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}
}

impl<'a> ImageHandle<'a> {
	fn width(&self) -> c_int {
		unsafe { heif_image_handle_get_width(self.inner) }
	}

	fn height(&self) -> c_int {
		unsafe { heif_image_handle_get_height(self.inner) }
	}

	/// Pixel count, or `None` when libheif reports non-positive dimensions.
	fn pixel_count(&self) -> Option<u64> {
		let (width, height) = (self.width(), self.height());
		if width <= 0 || height <= 0 {
			return None;
		}
		Some(width as u64 * height as u64)
	}

	fn covers(&self, target_width: u32, target_height: u32) -> bool {
		self.width() as i64 >= target_width as i64 && self.height() as i64 >= target_height as i64
	}

	/// Pixel count as DECLARED by the item's `ispe` box, before any
	/// transformation — `None` when the box is absent or non-positive.
	///
	/// This is the number libheif itself checks against
	/// `max_image_size_pixels`, and the only size a caller can learn without
	/// decoding. It is a declaration, not a measurement: see
	/// [`HeifSession::set_decode_limits`] for what that does and does not buy.
	fn ispe_pixel_count(&self) -> Option<u64> {
		let width = unsafe { heif_image_handle_get_ispe_width(self.inner) };
		let height = unsafe { heif_image_handle_get_ispe_height(self.inner) };
		if width <= 0 || height <= 0 {
			return None;
		}
		Some(width as u64 * height as u64)
	}

	/// The first embedded thumbnail, if any ("usually 0 or 1" per libheif).
	fn first_thumbnail(&self) -> Option<ImageHandle<'a>> {
		let count = unsafe { heif_image_handle_get_number_of_thumbnails(self.inner) };
		if count <= 0 {
			return None;
		}
		let mut id: heif_item_id = 0;
		let filled = unsafe { heif_image_handle_get_list_of_thumbnail_IDs(self.inner, &mut id, 1) };
		if filled < 1 {
			return None;
		}
		let mut handle = std::ptr::null_mut();
		let result = unsafe { heif_image_handle_get_thumbnail(self.inner, id, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok || handle.is_null() {
			return None;
		}
		Some(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}
}

impl Drop for ImageHandle<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_handle_release(self.inner) };
	}
}

struct OutImage<'a> {
	inner: *mut heif_image,
	_lifetime: PhantomData<&'a ImageHandle<'a>>,
}

impl OutImage<'_> {
	fn new(handle: &ImageHandle) -> Result<Self, HeifError> {
		let mut heif_image_ptr = std::ptr::null_mut();
		let result = unsafe {
			heif_decode_image(
				handle.inner,
				(&mut heif_image_ptr) as *mut *mut heif_image,
				heif_colorspace_heif_colorspace_RGB,
				heif_chroma_heif_chroma_interleaved_RGBA,
				std::ptr::null(),
			)
		};

		if result.code != heif_error_code_heif_error_Ok {
			// copy the message while `handle`, which owns the buffer it points into, is alive
			return Err(HeifError::from_raw(result));
		}

		Ok(OutImage {
			inner: heif_image_ptr,
			_lifetime: PhantomData,
		})
	}

	fn make_rgba(&self) -> Result<RgbaImage, HeifError> {
		let mut stride = 0usize;
		let plane_data = unsafe {
			heif_image_get_plane_readonly2(
				self.inner,
				heif_channel_heif_channel_interleaved,
				&mut stride as *mut usize,
			)
		};

		let width =
			unsafe { heif_image_get_width(self.inner, heif_channel_heif_channel_interleaved) };
		let height =
			unsafe { heif_image_get_height(self.inner, heif_channel_heif_channel_interleaved) };

		if plane_data.is_null() {
			return Err(HeifError::invalid_decoded_image());
		}
		let layout = validate_rgba_layout(width, height, stride)
			.ok_or_else(HeifError::invalid_decoded_image)?;

		let mut rgba_data = Vec::with_capacity(layout.capacity);

		for y in 0..layout.height {
			let row_start = y * stride;
			rgba_data.extend_from_slice(unsafe {
				std::slice::from_raw_parts(plane_data.add(row_start), layout.row_bytes)
			});
		}

		image::RgbaImage::from_vec(width as u32, height as u32, rgba_data)
			.ok_or_else(HeifError::invalid_decoded_image)
	}
}

impl Drop for OutImage<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_release(self.inner) };
	}
}

struct RgbaLayout {
	height: usize,
	row_bytes: usize,
	capacity: usize,
}

fn validate_rgba_layout(width: c_int, height: c_int, stride: usize) -> Option<RgbaLayout> {
	if width <= 0 || height <= 0 {
		return None;
	}
	let width = width as usize;
	let height = height as usize;
	let row_bytes = width.checked_mul(4)?;
	if stride < row_bytes {
		return None;
	}
	let capacity = row_bytes.checked_mul(height)?;
	// the end of the last row must be addressable without overflowing usize
	let last_row_end = (height - 1).checked_mul(stride)?.checked_add(row_bytes)?;
	// Vec allocations and raw-pointer offsets are limited to isize::MAX bytes
	if capacity > isize::MAX as usize || last_row_end > isize::MAX as usize {
		return None;
	}
	Some(RgbaLayout {
		height,
		row_bytes,
		capacity,
	})
}

pub fn try_get_rgba_image_from_slice(data: &[u8]) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_slice(data)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

pub fn try_get_rgba_image_from_file(path: &str) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_file(path)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

pub fn try_get_rgba_image_from_reader<T: Read + Seek>(
	reader: T,
	file_size: u64,
) -> Result<RgbaImage, HeifError> {
	let mut heif_reader = HeifReader::new(reader, file_size);
	let context = HeicContext::from_reader(&mut heif_reader)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

/// Decodes an RGBA image suitable as a `target_width`×`target_height` thumbnail
/// source without ever decoding more than `max_pixels` pixels (the decode
/// briefly holds ~8 bytes per pixel: libheif's RGBA buffer plus the returned
/// copy). Preference order:
///
/// 1. an embedded thumbnail that covers the target (iPhone HEICs always carry
///    one, and it is orders of magnitude cheaper than the primary image),
/// 2. the primary image, when it fits the budget,
/// 3. an undersized embedded thumbnail — better than nothing when the primary
///    is over budget,
/// 4. `Ok(None)`: nothing fits the budget and the caller should skip the
///    thumbnail rather than decode an arbitrarily large image.
pub fn try_get_rgba_thumbnail_from_reader<T: Read + Seek>(
	reader: T,
	file_size: u64,
	target_width: u32,
	target_height: u32,
	max_pixels: u64,
) -> Result<Option<RgbaImage>, HeifError> {
	let mut heif_reader = HeifReader::new(reader, file_size);
	let context = HeicContext::from_reader(&mut heif_reader)?;
	let primary = ImageHandle::new(&context)?;
	let thumbnail = ImageHandle::first_thumbnail(&primary).filter(|thumb| {
		thumb
			.pixel_count()
			.is_some_and(|pixels| pixels <= max_pixels)
	});

	let handle = if let Some(thumb) = &thumbnail
		&& thumb.covers(target_width, target_height)
	{
		thumb
	} else if primary
		.pixel_count()
		.is_some_and(|pixels| pixels <= max_pixels)
	{
		&primary
	} else if let Some(thumb) = &thumbnail {
		thumb
	} else {
		return Ok(None);
	};

	let out_image = OutImage::new(handle)?;
	out_image.make_rgba().map(Some)
}

/// The tile grid of a `grid`-encoded HEIF (every Apple HEIC): decode one
/// 512²-ish tile at a time instead of the whole frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeifTiling {
	pub num_columns: u32,
	pub num_rows: u32,
	pub tile_width: u32,
	pub tile_height: u32,
	pub image_width: u32,
	pub image_height: u32,
}

/// A parsed HEIF whose reader stays open across calls, so the container can
/// be inspected (dims, tiling, embedded thumbnail) and then decoded tile by
/// tile without re-parsing. The one-shot `try_get_*` entry points above stay
/// for callers that want a single decode.
pub struct HeifSession<T: Read + Seek> {
	// Declared before `reader` on purpose: fields drop in declaration order,
	// and libheif must never outlive the reader its callbacks point into.
	context: HeicContext<'static>,
	// Boxed so the address libheif captured in `new` stays stable; libheif
	// calls back into it lazily on every decode.
	#[allow(dead_code)] // held for its stable address + Drop order
	reader: Box<HeifReader<T>>,
	/// Ceiling from [`set_decode_limits`](Self::set_decode_limits), kept on
	/// this side too: libheif does not apply its own `max_image_size_pixels`
	/// on the tile path, so [`decode_tile_rgba`](Self::decode_tile_rgba)
	/// enforces it against the tile item itself.
	max_image_pixels: Option<u64>,
	/// Whether the whole grid has been size-checked against
	/// `max_image_pixels` yet — the scan is per session, not per tile.
	tiles_validated: std::cell::Cell<bool>,
}

impl<T: Read + Seek> HeifSession<T> {
	pub fn new(reader: T, file_size: u64) -> Result<Self, HeifError> {
		let mut reader = Box::new(HeifReader::new(reader, file_size));
		// Mirrors `HeicContext::from_reader`, but the reader is heap-pinned so
		// the context may keep calling it (and the vtable stored inside it)
		// after this function returns. The 'static on the context is the same
		// lifetime laundering `from_file` already does; the struct's field
		// order keeps it honest.
		let context: HeicContext<'static> = HeicContext::from_reader(&mut reader)?;
		Ok(HeifSession {
			context,
			reader,
			max_image_pixels: None,
			tiles_validated: std::cell::Cell::new(false),
		})
	}

	/// Caps what any single decode through this session may materialise.
	///
	/// Two guards, with different reach — be precise about which is which:
	///
	/// - `max_image_pixels` is enforced HERE, before decoding, against each
	///   item's DECLARED (`ispe`) size: libheif applies its own check only
	///   when decoding a whole image (`image_item.cc`,
	///   `if (!decode_tile_only)`), so on the tile path
	///   [`decode_tile_rgba`](Self::decode_tile_rgba) does it instead.
	/// - `max_total_memory` is enforced by libheif at
	///   `heif_image_add_plane_safe`, i.e. when the decoded picture is copied
	///   OUT of the codec. That is a backstop, not a pre-allocation guard.
	///
	/// What neither guard covers: a bitstream whose SPS declares a picture
	/// larger than the item's `ispe` promised. libde265 (vendored, 1.0.16)
	/// takes no size limit of its own and materialises the picture in its DPB
	/// before libheif sees a single plane, so such a file is caught only on
	/// copy-out, after the codec has already allocated. That gap is libheif's
	/// on every path, tiled or not — it is not specific to this API, and it
	/// cannot be closed without bounding libde265 itself.
	///
	/// The limits live on the context and apply to every later decode call;
	/// the container parse in [`new`](Self::new) already happened under
	/// libheif's global defaults.
	pub fn set_decode_limits(&mut self, max_image_pixels: u64, max_total_memory: u64) {
		self.max_image_pixels = Some(max_image_pixels);
		self.tiles_validated.set(false);
		let limits = unsafe { heif_context_get_security_limits(self.context.inner) };
		if limits.is_null() {
			// Not reachable per the API contract; losing the cap must not
			// break decoding, the caller's own budget checks still stand.
			return;
		}
		unsafe {
			(*limits).max_image_size_pixels = max_image_pixels;
			(*limits).max_total_memory = max_total_memory;
		}
	}

	fn primary(&self) -> Result<ImageHandle<'_>, HeifError> {
		ImageHandle::new(&self.context)
	}

	pub fn primary_dims(&self) -> Result<(u32, u32), HeifError> {
		let primary = self.primary()?;
		let (width, height) = (primary.width(), primary.height());
		if width <= 0 || height <= 0 {
			return Err(HeifError::invalid_decoded_image());
		}
		Ok((width as u32, height as u32))
	}

	/// The embedded thumbnail, decoded — `None` when there is none or it is
	/// implausibly large for a thumbnail.
	pub fn embedded_thumbnail_rgba(&self, max_pixels: u64) -> Result<Option<RgbaImage>, HeifError> {
		let primary = self.primary()?;
		let Some(thumb) = primary.first_thumbnail() else {
			return Ok(None);
		};
		if thumb.pixel_count().is_none_or(|px| px > max_pixels) {
			return Ok(None);
		}
		OutImage::new(&thumb)?.make_rgba().map(Some)
	}

	/// The primary image's tile grid, or `None` when it is a single tile
	/// (then only [`decode_primary_rgba`](Self::decode_primary_rgba) helps).
	/// Sizes are in the transformed (display) coordinate space, matching what
	/// [`decode_tile_rgba`](Self::decode_tile_rgba) produces.
	pub fn tiling(&self) -> Result<Option<HeifTiling>, HeifError> {
		let primary = self.primary()?;
		let mut raw: heif_image_tiling = unsafe { std::mem::zeroed() };
		let result = unsafe { heif_image_handle_get_image_tiling(primary.inner, 1, &mut raw) };
		if result.code != heif_error_code_heif_error_Ok {
			return Ok(None);
		}
		if raw.num_columns <= 1 && raw.num_rows <= 1 {
			return Ok(None);
		}
		if raw.tile_width == 0
			|| raw.tile_height == 0
			|| raw.image_width == 0
			|| raw.image_height == 0
		{
			return Ok(None);
		}
		Ok(Some(HeifTiling {
			num_columns: raw.num_columns,
			num_rows: raw.num_rows,
			tile_width: raw.tile_width,
			tile_height: raw.tile_height,
			image_width: raw.image_width,
			image_height: raw.image_height,
		}))
	}

	/// The grid tile item at ORIGINAL (untransformed) grid indices, as its own
	/// handle, so its declared size can be read before anything decodes it.
	/// `None` when the primary is not a grid or the indices name no tile.
	///
	/// Untransformed on purpose. `heif_image_handle_get_grid_image_tile_id`
	/// range-checks its arguments against the untransformed grid and only
	/// THEN maps them (`heif_tiling.cc`), so passing display coordinates makes
	/// it reject the rows a rotation added — an 8x6 grid shown as 6x8 loses
	/// rows 6 and 7. Enumerating the original grid sidesteps that entirely,
	/// and for a size bound the tile ORDER does not matter.
	fn original_tile_handle(
		&self,
		primary: &ImageHandle<'_>,
		tile_x: u32,
		tile_y: u32,
	) -> Option<ImageHandle<'_>> {
		let mut tile_id: heif_item_id = 0;
		let result = unsafe {
			heif_image_handle_get_grid_image_tile_id(primary.inner, 0, tile_x, tile_y, &mut tile_id)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return None;
		}
		let mut handle = std::ptr::null_mut();
		let result =
			unsafe { heif_context_get_image_handle(self.context.inner, tile_id, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok || handle.is_null() {
			return None;
		}
		Some(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}

	/// Checks EVERY tile's declared (`ispe`) size against the ceiling, once
	/// per session. Cheap: item-property reads, no decoding.
	///
	/// Whole-grid rather than per-tile because the tiling struct reports only
	/// TILE 0's size (`grid.cc`, `get_heif_image_tiling`) — the size the
	/// caller's own budget arithmetic is built on — so a grid whose later
	/// tiles declare something far larger would otherwise sail through the
	/// budget check and reach the codec unchallenged.
	///
	/// The scan walks a square that contains the original grid whichever way
	/// round it is, and counts what libheif accepted: every coordinate it
	/// names must check out, and the count must reach the tile total, or the
	/// grid is refused rather than half-verified.
	fn validate_tile_sizes(
		&self,
		primary: &ImageHandle<'_>,
		max_pixels: u64,
	) -> Result<(), HeifError> {
		let Some(tiling) = self.tiling()? else {
			return Ok(());
		};
		let expected = u64::from(tiling.num_columns) * u64::from(tiling.num_rows);
		let span = tiling.num_columns.max(tiling.num_rows);
		let mut checked = 0u64;
		for y in 0..span {
			for x in 0..span {
				let Some(tile) = self.original_tile_handle(primary, x, y) else {
					continue;
				};
				match tile.ispe_pixel_count() {
					Some(pixels) if pixels <= max_pixels => checked += 1,
					Some(pixels) => {
						return Err(HeifError::invalid_input(&format!(
							"grid tile declares {pixels} pixels, over the {max_pixels} allowed"
						)));
					}
					None => {
						return Err(HeifError::invalid_input(
							"a grid tile does not declare its size; refusing to decode the grid",
						));
					}
				}
			}
		}
		if checked < expected {
			return Err(HeifError::invalid_input(&format!(
				"only {checked} of {expected} grid tiles could be size-checked; refusing the grid"
			)));
		}
		Ok(())
	}

	/// Decodes exactly one grid tile to RGBA (~tile-sized allocation, not
	/// image-sized). Tile coordinates are grid indices, transformed space.
	///
	/// The first call verifies every tile's declared size against
	/// [`set_decode_limits`](Self::set_decode_limits), because libheif skips
	/// its own `ispe` check for single-tile decodes
	/// (`image_item.cc`, `if (!decode_tile_only)`).
	pub fn decode_tile_rgba(&self, tile_x: u32, tile_y: u32) -> Result<RgbaImage, HeifError> {
		let primary = self.primary()?;
		if let Some(max_pixels) = self.max_image_pixels
			&& !self.tiles_validated.get()
		{
			self.validate_tile_sizes(&primary, max_pixels)?;
			self.tiles_validated.set(true);
		}
		let mut heif_image_ptr = std::ptr::null_mut();
		let result = unsafe {
			heif_image_handle_decode_image_tile(
				primary.inner,
				&mut heif_image_ptr,
				heif_colorspace_heif_colorspace_RGB,
				heif_chroma_heif_chroma_interleaved_RGBA,
				std::ptr::null(),
				tile_x,
				tile_y,
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}
		let out = OutImage {
			inner: heif_image_ptr,
			_lifetime: PhantomData,
		};
		out.make_rgba()
	}

	/// Whole-frame decode of the primary image — only for sources the caller
	/// already verified are small.
	pub fn decode_primary_rgba(&self) -> Result<RgbaImage, HeifError> {
		let primary = self.primary()?;
		OutImage::new(&primary)?.make_rgba()
	}
}

struct HeifReader<T>
where
	T: Read + Seek,
{
	inner: T,
	file_size: u64,
	/// libheif does NOT copy the function table — `StreamReader_CApi` keeps
	/// the raw `const heif_reader*` and dereferences it on every later lazy
	/// read (bitstream.h). The table must therefore live exactly as long as
	/// the reader itself, not as a temporary at the registration call site.
	vtable: heif_reader,
}

impl<T: Read + Seek> HeifReader<T> {
	fn new(inner: T, file_size: u64) -> Self {
		HeifReader {
			inner,
			file_size,
			vtable: heif_reader {
				reader_api_version: 1,
				get_position: Some(get_position_impl::<T>),
				read: Some(read_impl::<T>),
				seek: Some(seek_impl::<T>),
				wait_for_file_size: Some(wait_for_file_size_impl::<T>),
				request_range: None,
				preload_range_hint: None,
				release_file_range: None,
				release_error_msg: None,
			},
		}
	}

	fn get_position(&mut self) -> Result<u64, std::io::Error> {
		self.inner.stream_position()
	}

	// Reads exactly `buffer.len()` bytes, looping over short reads (which a
	// generic `Read` may return mid-file) and treating a premature EOF as an
	// error. libheif only calls the read callback after `wait_for_file_size`
	// confirmed the bytes are available, so a short read is a genuine failure,
	// not a partial success to zero-fill.
	fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), std::io::Error> {
		self.inner.read_exact(buffer)
	}

	// Helper method to seek
	fn seek(&mut self, position: i64) -> Result<(), std::io::Error> {
		self.inner.seek(SeekFrom::Start(position as u64))?;
		Ok(())
	}

	// Helper method to check if we can read up to target_size
	fn wait_for_file_size(&mut self, target_size: i64) -> heif_reader_grow_status {
		if target_size as u64 <= self.file_size {
			heif_reader_grow_status_heif_reader_grow_status_size_reached
		} else {
			heif_reader_grow_status_heif_reader_grow_status_size_beyond_eof
		}
	}
}

unsafe extern "C" fn get_position_impl<T: Read + Seek>(userdata: *mut c_void) -> i64 {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	reader.get_position().map(|pos| pos as i64).unwrap_or(-1)
}

unsafe extern "C" fn read_impl<T: Read + Seek>(
	data: *mut c_void,
	size: usize,
	userdata: *mut c_void,
) -> c_int {
	// from_raw_parts_mut requires a non-null pointer even for size == 0;
	// libheif treats any non-zero return as a read failure
	if data.is_null() {
		return -1;
	}
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	let buffer = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, size) };

	match reader.read_exact(buffer) {
		Ok(()) => 0,  // Success — the whole buffer was filled
		Err(_) => -1, // Error, or a genuine short read / premature EOF
	}
}

unsafe extern "C" fn seek_impl<T: Read + Seek>(position: i64, userdata: *mut c_void) -> c_int {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	match reader.seek(position) {
		Ok(_) => 0,   // Success
		Err(_) => -1, // Error
	}
}

unsafe extern "C" fn wait_for_file_size_impl<T: Read + Seek>(
	target_size: i64,
	userdata: *mut c_void,
) -> heif_reader_grow_status {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	reader.wait_for_file_size(target_size)
}

#[derive(Debug)]
pub struct HeifError {
	code: heif_error_code,
	#[allow(dead_code)] // diagnostic detail, surfaced via the Debug impl
	subcode: heif_suberror_code,
	message: String,
}

impl HeifError {
	/// Copies the message eagerly: libheif error messages point into a buffer
	/// owned by the producing context/handle, so the pointer dangles once that
	/// object is freed. Call this while the producing object is still alive.
	fn from_raw(error: heif_error) -> Self {
		let message = if error.message.is_null() {
			String::from("unknown error")
		} else {
			unsafe { CStr::from_ptr(error.message) }
				.to_string_lossy()
				.into_owned()
		};
		HeifError {
			code: error.code,
			subcode: error.subcode,
			message,
		}
	}

	fn invalid_input(message: &str) -> Self {
		HeifError {
			code: heif_error_code_heif_error_Invalid_input,
			subcode: heif_suberror_code_heif_suberror_Unspecified,
			message: String::from(message),
		}
	}

	fn invalid_decoded_image() -> Self {
		Self::invalid_input("decoded image has invalid plane, dimensions, or stride")
	}
}

impl std::fmt::Display for HeifError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"heif error: code: {}, message: {}",
			self.code, self.message
		)
	}
}

impl std::error::Error for HeifError {}

// #[cfg(test)]
// mod tests {
// 	use super::*;

// 	const TEST_HEIC_FILE: &str = "/Users/end/Documents/tmp/image1.heic"; // Update this path to a valid HEIC file for testing
// 	const TEST_OUTPUT_DIR: &str = "/Users/end/Documents/tmp/"; // Update this path to a valid output directory

// 	// very basic tests for now

// 	#[test]
// 	fn test_reader() {
// 		let heic_file = std::fs::File::open(TEST_HEIC_FILE).unwrap();
// 		let file_size = heic_file.metadata().unwrap().len();
// 		let image = try_get_rgba_image_from_reader(heic_file, file_size).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_reader.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}

// 	#[test]
// 	fn test_file() {
// 		let image = try_get_rgba_image_from_file(TEST_HEIC_FILE).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_file.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}

// 	#[test]
// 	fn test_slice() {
// 		let heic_data = std::fs::read(TEST_HEIC_FILE).unwrap();
// 		let image = try_get_rgba_image_from_slice(&heic_data).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_slice.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}
// }

#[cfg(test)]
mod layout_tests {
	use super::validate_rgba_layout;

	#[test]
	fn rejects_non_positive_dimensions() {
		assert!(validate_rgba_layout(0, 10, 40).is_none());
		assert!(validate_rgba_layout(10, 0, 40).is_none());
		assert!(validate_rgba_layout(-1, 10, 40).is_none());
		assert!(validate_rgba_layout(10, -1, 40).is_none());
		assert!(validate_rgba_layout(i32::MIN, i32::MIN, usize::MAX).is_none());
	}

	#[test]
	fn rejects_stride_smaller_than_row() {
		assert!(validate_rgba_layout(10, 10, 39).is_none());
		assert!(validate_rgba_layout(10, 10, 0).is_none());
	}

	#[test]
	fn rejects_row_offset_overflow() {
		assert!(validate_rgba_layout(2, 2, usize::MAX).is_none());
		assert!(validate_rgba_layout(i32::MAX, i32::MAX, usize::MAX).is_none());
	}

	#[test]
	fn accepts_dimensions_whose_byte_size_overflows_i32() {
		let side = 23_171_i32;
		let layout = validate_rgba_layout(side, side, side as usize * 4).unwrap();
		assert_eq!(layout.height, side as usize);
		assert_eq!(layout.row_bytes, side as usize * 4);
		assert_eq!(layout.capacity, side as usize * side as usize * 4);
	}

	#[test]
	fn accepts_padded_stride() {
		let layout = validate_rgba_layout(3, 2, 16).unwrap();
		assert_eq!(layout.height, 2);
		assert_eq!(layout.row_bytes, 12);
		assert_eq!(layout.capacity, 24);
	}

	#[test]
	fn rejects_capacity_exceeding_isize_max() {
		// width == height == i32::MAX with an unpadded stride: the byte counts
		// still fit in usize (so the checked_mul overflow guards above don't
		// trigger), but the resulting capacity exceeds isize::MAX, which
		// Vec::with_capacity cannot allocate.
		let side = i32::MAX;
		let row_bytes = side as usize * 4;
		assert!((row_bytes * side as usize) > isize::MAX as usize);
		assert!(validate_rgba_layout(side, side, row_bytes).is_none());
	}
}

#[cfg(test)]
mod api_tests {
	use std::io::Cursor;

	use super::*;

	#[test]
	fn from_file_rejects_path_with_interior_nul() {
		let path = "does/not/exist\0evil.heic";
		let err = try_get_rgba_image_from_file(path).unwrap_err();
		// must fail before any filesystem access is attempted
		assert!(err.to_string().contains("interior NUL byte"));
	}

	#[test]
	fn read_impl_rejects_null_data_pointer() {
		let mut reader = HeifReader::new(Cursor::new(Vec::<u8>::new()), 0);
		let result = unsafe {
			read_impl::<Cursor<Vec<u8>>>(
				std::ptr::null_mut(),
				4,
				&mut reader as *mut _ as *mut c_void,
			)
		};
		assert_eq!(result, -1);
	}

	#[test]
	fn read_impl_reports_short_read_as_failure() {
		// The reader holds only 2 bytes but libheif asks for 4. A short read must
		// be reported as a failure (-1), not zero-filled and reported as success.
		let mut reader = HeifReader::new(Cursor::new(vec![1u8, 2]), 2);
		let mut buf = [0xEEu8; 4];
		let result = unsafe {
			read_impl::<Cursor<Vec<u8>>>(
				buf.as_mut_ptr() as *mut c_void,
				buf.len(),
				&mut reader as *mut _ as *mut c_void,
			)
		};
		assert_eq!(result, -1);
	}
}
