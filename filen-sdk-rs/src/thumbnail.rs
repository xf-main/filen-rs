use image::{DynamicImage, ImageDecoder, imageops::FilterType};

use crate::{
	ErrorKind,
	auth::Client,
	error::{Error, MetadataWasNotDecryptedError},
	fs::file::{RemoteFile, traits::HasFileInfo},
	io::client_impl::IoSharedClientExt,
	runtime,
};

// MUST BE SORTED ALPHABETICALLY
const SUPPORTED_THUMBNAIL_MIME_TYPES: &[&str] = &[
	#[cfg(feature = "avif-decoder")]
	"image/avif",
	"image/gif",
	#[cfg(feature = "heif-decoder")]
	"image/heic",
	#[cfg(feature = "heif-decoder")]
	"image/heif",
	"image/jpeg",
	"image/png",
	"image/qoi",
	"image/tiff",
	"image/webp",
	"image/x-qoi",
];

pub fn is_supported_thumbnail_mime(mime: &str) -> bool {
	SUPPORTED_THUMBNAIL_MIME_TYPES.binary_search(&mime).is_ok()
}

impl Client {
	pub async fn make_thumbnail_in_memory<Id: Send + Sync>(
		&self,
		file: &RemoteFile<Id>,
		max_width: u32,
		max_height: u32,
	) -> Result<DynamicImage, Error> {
		let mime = file.mime().ok_or(MetadataWasNotDecryptedError)?;
		if !is_supported_thumbnail_mime(mime) {
			return Err(Error::custom(
				ErrorKind::ImageError,
				format!("unsupported thumbnail mime type: {mime}"),
			));
		}
		let image_data = self.download_file(file).await?;

		runtime::do_cpu_intensive(|| {
			let image = if mime == "image/heic" || mime == "image/heif" {
				#[cfg(feature = "heif-decoder")]
				{
					DynamicImage::ImageRgba8(heif_decoder::try_get_rgba_image_from_slice(
						&image_data,
					)?)
				}
				#[cfg(not(feature = "heif-decoder"))]
				{
					unreachable!(
						"heif/heic support not enabled, should be handled by is_supported_thumbnail_mime"
					)
				}
			} else {
				let reader = image::ImageReader::new(std::io::Cursor::new(&image_data))
					.with_guessed_format()?;
				let mut decoder = reader.into_decoder()?;
				let orientation = decoder.orientation()?;
				let mut image = DynamicImage::from_decoder(decoder)?;
				image.apply_orientation(orientation);
				image
			};

			Ok(image.resize(max_width, max_height, FilterType::CatmullRom))
		})
		.await
	}
}

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod js_impls {
	use filen_macros::js_type;
	use image::codecs::webp::WebPEncoder;

	use crate::{
		Error,
		auth::JsClient,
		fs::file::AnonymousRemoteFile,
		js::File,
		runtime::{self, do_on_commander},
	};

	#[js_type(import)]
	pub struct MakeThumbnailInMemoryParams {
		pub file: File,
		pub max_width: u32,
		pub max_height: u32,
	}

	#[js_type(export)]
	pub struct MakeThumbnailInMemoryResult {
		// this is correct, ts requires the specifity
		// because of https://github.com/microsoft/typescript/issues/62546
		// not sure who to upstream this to, tsify, js_sys, or wasm-bindgen
		#[cfg(feature = "wasm-full")]
		#[tsify(type = "Uint8Array<ArrayBuffer>")]
		pub webp_data: serde_bytes::ByteBuf,
		#[cfg(feature = "uniffi")]
		pub webp_data: Vec<u8>,
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "makeThumbnailInMemory")
		)]
		pub async fn make_thumbnail_in_memory(
			&self,
			params: MakeThumbnailInMemoryParams,
		) -> Result<Option<MakeThumbnailInMemoryResult>, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				let image = match this
					.make_thumbnail_in_memory(
						// Thumbnailing only reads the file, so a file from a link
						// or shared-in listing (which reports no stable id) is fine.
						&AnonymousRemoteFile::try_from(params.file)?,
						params.max_width,
						params.max_height,
					)
					.await
				{
					Ok(image) => image,
					Err(e) => {
						tracing::debug!("failed to create thumbnail: {}", e);
						return Ok(None);
					}
				};
				runtime::do_cpu_intensive(
					|| -> Result<Option<MakeThumbnailInMemoryResult>, Error> {
						// really wish I knew the exact size beforehand so we could preallocate
						let mut image_data = Vec::new();
						image.write_with_encoder(WebPEncoder::new_lossless(&mut image_data))?;
						let webp_data = {
							#[cfg(all(target_family = "wasm", target_os = "unknown"))]
							{
								serde_bytes::ByteBuf::from(image_data)
							}
							#[cfg(feature = "uniffi")]
							{
								image_data
							}
						};
						Ok(Some(MakeThumbnailInMemoryResult { webp_data }))
					},
				)
				.await
			})
			.await
		}
	}
}

// The bounded pipeline is native-only: its only consumer is the mobile
// cache (wasm thumbnails run through `make_thumbnail_in_memory` above),
// and the microthumb dependency is gated to match.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod bounded {
	use std::io::{BufRead, Seek, Write};

	use image::codecs::webp::WebPEncoder;

	use super::*;

	pub use microthumb::DEFAULT_MEM_BUDGET as DEFAULT_THUMBNAIL_MEM_BUDGET;
	pub use microthumb::{ByteSource, FileSource};
	use microthumb::{ThumbError, ThumbSpec};

	/// [`microthumb::ByteSource`] over any `BufRead + Seek` — the local-file
	/// path. `len` is the caller-declared size; reads past it answer 0.
	struct ReadSeekSource<R> {
		inner: R,
		len: u64,
	}

	impl<R: BufRead + Seek + Send> ByteSource for ReadSeekSource<R> {
		fn len(&self) -> u64 {
			self.len
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			if offset >= self.len {
				return Ok(0);
			}
			self.inner.seek(std::io::SeekFrom::Start(offset))?;
			self.inner.read(buf)
		}
	}

	/// Resident bytes a [`RemoteChunkSource`] keeps for the whole decode (its
	/// two chunk slots) — callers subtract this from the budget they hand
	/// [`make_thumbnail_from_source`], because the pipeline's own accounting
	/// cannot see the source's memory.
	pub const REMOTE_SOURCE_RESIDENT_BYTES: usize = 2 * crate::consts::CHUNK_SIZE_U64 as usize;

	/// Encodes a `target_width`×`target_height` webp thumbnail of `image_reader`
	/// into `out` without ever materialising a full decoded frame: microthumb
	/// streams every format it can (IDCT-scaled JPEG, PNG scanlines, HEIF tiles,
	/// embedded previews) and refuses anything whose honest decode peak exceeds
	/// `mem_budget` bytes ([`DEFAULT_THUMBNAIL_MEM_BUDGET`] fits an iOS
	/// file-provider extension's ~20 MB jetsam ceiling). `Ok(None)` means no
	/// thumbnail could be made within the budget — or the bytes are not a
	/// supported image, which the sniff decides; `mime` is unused and kept for
	/// callers that log it.
	pub fn make_thumbnail<R, W>(
		_mime: Option<&str>,
		image_file_size: u64,
		image_reader: R,
		target_width: u32,
		target_height: u32,
		mem_budget: usize,
		out: &mut W,
	) -> Result<Option<ThumbnailInfo>, Error>
	where
		R: BufRead + Seek + Send + 'static,
		W: Write,
	{
		let source = ReadSeekSource {
			inner: image_reader,
			len: image_file_size,
		};
		make_thumbnail_from_source(
			Box::new(source),
			target_width,
			target_height,
			mem_budget,
			out,
		)
	}

	/// [`make_thumbnail`] for callers that already hold a [`ByteSource`] —
	/// notably [`RemoteChunkSource`], where lazy chunk fetches mean an
	/// embedded-preview hit never downloads the rest of the file.
	pub fn make_thumbnail_from_source<W>(
		source: Box<dyn ByteSource>,
		target_width: u32,
		target_height: u32,
		mem_budget: usize,
		out: &mut W,
	) -> Result<Option<ThumbnailInfo>, Error>
	where
		W: Write,
	{
		let spec = ThumbSpec {
			target_width,
			target_height,
			mem_budget,
		};
		let (small, thumb_source) = match microthumb::generate(source, &spec) {
			Ok(Some(thumb)) => (thumb.image, thumb.source),
			Ok(None) => return Ok(None),
			// Corrupt or refused bytes are the same cacheable "no thumbnail"
			// verdict the sniff gives unsupported formats — parity with the
			// old image-crate path, whose Unsupported errors the mobile layer
			// mapped to NoThumbnail. Only transport/system errors stay hard:
			// an offline blip must never stick a permanent "no thumbnail"
			// verdict in the platform's cache.
			Err(e @ (ThumbError::Decode(_) | ThumbError::Geometry)) => {
				tracing::debug!("thumbnail decode refused: {e}");
				return Ok(None);
			}
			Err(ThumbError::Io(e)) => return Err(e.into()),
		};
		let Some(rgba) = image::RgbaImage::from_vec(small.width, small.height, small.rgba) else {
			return Err(Error::custom(
				ErrorKind::ImageError,
				"thumbnail canvas has an inconsistent buffer".to_string(),
			));
		};
		let img = DynamicImage::ImageRgba8(rgba);
		let created_width = target_width.min(img.width());
		let created_height = target_height.min(img.height());
		let thumbnail = img.resize_to_fill(created_width, created_height, FilterType::CatmullRom);
		let encoder = WebPEncoder::new_lossless(out);
		thumbnail.write_with_encoder(encoder)?;
		Ok(Some(ThumbnailInfo {
			width: created_width,
			height: created_height,
			source: thumb_source,
		}))
	}

	/// Which path produced a thumbnail, re-exported so callers can log it
	/// without depending on `microthumb` directly.
	pub use microthumb::ThumbSource;

	/// The thumbnail that was written, and how it was obtained.
	#[derive(Debug, Clone, Copy, PartialEq, Eq)]
	pub struct ThumbnailInfo {
		pub width: u32,
		pub height: u32,
		/// Cheap embedded preview vs a real decode — orders of magnitude apart
		/// in bytes fetched and CPU spent, and otherwise indistinguishable
		/// from the outside.
		pub source: ThumbSource,
	}

	/// [`microthumb::ByteSource`] that fetches and decrypts 1 MiB chunks of a
	/// remote file on demand, keeping only the last two — so a thumbnail served
	/// by an embedded preview costs one chunk of download, not the file. Sync by
	/// contract (microthumb runs inside `spawn_blocking`); each miss bridges to
	/// the async chunk fetch via `Handle::block_on`, which is legal there.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub struct RemoteChunkSource {
		// Owned, so the source can move into the `spawn_blocking` closure the
		// pipeline runs in.
		client: std::sync::Arc<crate::auth::Client>,
		file: crate::fs::file::RemoteFile,
		handle: tokio::runtime::Handle,
		/// Checked before every fetch: cancellation surfaces as
		/// `ErrorKind::Interrupted`, which unwinds the decode through its normal
		/// error path at chunk granularity — the same points an async decoder
		/// would get to cancel at.
		cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
		len: u64,
		slots: [Option<(u64, Vec<u8>)>; 2],
		next_evict: usize,
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	impl RemoteChunkSource {
		pub fn new(
			client: std::sync::Arc<crate::auth::Client>,
			file: crate::fs::file::RemoteFile,
			handle: tokio::runtime::Handle,
			cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
		) -> Self {
			let len = file.size();
			RemoteChunkSource {
				client,
				file,
				handle,
				cancel,
				len,
				slots: [None, None],
				next_evict: 0,
			}
		}

		fn chunk(&mut self, index: u64) -> std::io::Result<&Vec<u8>> {
			// Checked on EVERY read, cache hit included. Gating this on the
			// fetch instead would mean a decode whose remaining reads all land
			// in the two resident slots — anything under ~2 MiB once warm —
			// never notices the cancel and runs to completion, while the caller
			// documents both source kinds as answering their next read with
			// Interrupted.
			if self
				.cancel
				.as_ref()
				.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
			{
				return Err(std::io::Error::new(
					std::io::ErrorKind::Interrupted,
					"thumbnail cancelled",
				));
			}
			// Two fixed slots instead of a map: header + payload locality is all
			// the demuxers need, and eviction keeps the source at ≤2 MiB.
			if let Some(slot) = self
				.slots
				.iter()
				.position(|s| s.as_ref().is_some_and(|(i, _)| *i == index))
			{
				return Ok(&self.slots[slot].as_ref().expect("just matched").1);
			}
			let start = index * crate::consts::CHUNK_SIZE_U64;
			let end = (start + crate::consts::CHUNK_SIZE_U64).min(self.len);
			let (client, file) = (&self.client, &self.file);
			let data = self
				.handle
				.block_on(async move {
					use futures::AsyncReadExt;
					let mut reader =
						crate::fs::file::read::FileReaderBuilder::new(client.unauthed(), file)
							.with_start(start)
							.with_end(end)
							.build();
					let mut data = Vec::with_capacity((end - start) as usize);
					reader.read_to_end(&mut data).await?;
					Ok::<_, Error>(data)
				})
				.map_err(std::io::Error::other)?;
			let slot = self.next_evict;
			self.next_evict = (self.next_evict + 1) % self.slots.len();
			self.slots[slot] = Some((index, data));
			Ok(&self.slots[slot].as_ref().expect("just stored").1)
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	impl ByteSource for RemoteChunkSource {
		fn len(&self) -> u64 {
			self.len
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			if offset >= self.len {
				return Ok(0);
			}
			let index = offset / crate::consts::CHUNK_SIZE_U64;
			let within = (offset % crate::consts::CHUNK_SIZE_U64) as usize;
			let chunk = self.chunk(index)?;
			if within >= chunk.len() {
				return Ok(0);
			}
			// May be short at a chunk boundary; readers loop per the Read contract.
			let n = buf.len().min(chunk.len() - within);
			buf[..n].copy_from_slice(&chunk[within..within + n]);
			Ok(n)
		}
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use bounded::{
	ByteSource, DEFAULT_THUMBNAIL_MEM_BUDGET, FileSource, REMOTE_SOURCE_RESIDENT_BYTES,
	RemoteChunkSource, ThumbSource, ThumbnailInfo, make_thumbnail, make_thumbnail_from_source,
};

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use image::{ImageFormat, RgbImage};

	use super::make_thumbnail;

	fn png_bytes(width: u32, height: u32) -> Vec<u8> {
		let image = RgbImage::from_fn(width, height, |x, y| {
			image::Rgb([(x % 256) as u8, (y % 256) as u8, 0])
		});
		let mut bytes = Vec::new();
		image
			.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
			.unwrap();
		bytes
	}

	#[test]
	fn refuses_sources_above_the_memory_budget() {
		// A budget too small even for PNG's row buffers: the pipeline must
		// answer None (no thumbnail) rather than decode past its ceiling.
		let bytes = png_bytes(100, 80);
		let mut out = Vec::new();
		let len = bytes.len() as u64;
		let result = make_thumbnail(
			Some("image/png"),
			len,
			Cursor::new(bytes),
			32,
			32,
			1024,
			&mut out,
		)
		.unwrap();
		assert_eq!(result, None);
		assert!(out.is_empty());
	}

	#[test]
	fn makes_a_thumbnail_within_the_default_budget() {
		let bytes = png_bytes(100, 80);
		let mut out = Vec::new();
		let len = bytes.len() as u64;
		let result = make_thumbnail(
			Some("image/png"),
			len,
			Cursor::new(bytes),
			32,
			32,
			super::DEFAULT_THUMBNAIL_MEM_BUDGET,
			&mut out,
		)
		.unwrap();
		let info = result.expect("a thumbnail");
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn never_upscales_small_sources() {
		let bytes = png_bytes(16, 16);
		let mut out = Vec::new();
		let len = bytes.len() as u64;
		let result = make_thumbnail(
			Some("image/png"),
			len,
			Cursor::new(bytes),
			64,
			64,
			super::DEFAULT_THUMBNAIL_MEM_BUDGET,
			&mut out,
		)
		.unwrap();
		let info = result.expect("a thumbnail");
		assert_eq!((info.width, info.height), (16, 16));
	}

	use std::sync::Arc;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

	/// A [`super::ByteSource`] with [`super::RemoteChunkSource`]'s observable
	/// behavior, minus the network: short reads at every chunk boundary, and a
	/// cancel flag answered with `Interrupted` — optionally flipped by the
	/// source itself after N reads, standing in for a batch cancel landing
	/// mid-decode.
	struct ChunkySource {
		data: Vec<u8>,
		chunk: usize,
		cancel: Arc<AtomicBool>,
		cancel_after_reads: Option<usize>,
		reads: AtomicUsize,
	}

	impl super::ByteSource for ChunkySource {
		fn len(&self) -> u64 {
			self.data.len() as u64
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			let reads = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
			if self
				.cancel_after_reads
				.is_some_and(|threshold| reads > threshold)
			{
				self.cancel.store(true, Ordering::Relaxed);
			}
			if self.cancel.load(Ordering::Relaxed) {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Interrupted,
					"thumbnail cancelled",
				));
			}
			let Ok(offset) = usize::try_from(offset) else {
				return Ok(0);
			};
			if offset >= self.data.len() {
				return Ok(0);
			}
			let to_boundary = self.chunk - (offset % self.chunk);
			let n = buf.len().min(to_boundary).min(self.data.len() - offset);
			buf[..n].copy_from_slice(&self.data[offset..offset + n]);
			Ok(n)
		}
	}

	#[test]
	fn short_reads_at_chunk_boundaries_still_thumbnail() {
		// A 1 KiB chunk size forces hundreds of boundary-shortened reads
		// through the same read_at contract RemoteChunkSource answers with.
		let source = ChunkySource {
			data: png_bytes(300, 200),
			chunk: 1024,
			cancel: Arc::new(AtomicBool::new(false)),
			cancel_after_reads: None,
			reads: AtomicUsize::new(0),
		};
		let mut out = Vec::new();
		let result = super::make_thumbnail_from_source(
			Box::new(source),
			32,
			32,
			super::DEFAULT_THUMBNAIL_MEM_BUDGET,
			&mut out,
		)
		.unwrap();
		let info = result.expect("a thumbnail");
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn a_cancel_mid_decode_is_a_hard_error_not_a_cached_verdict() {
		// The flag flips after the header reads succeed, the way a batch
		// cancel lands mid-decode. Interrupted must surface as Err — never as
		// Ok(None), which the platform would cache as "no thumbnail" forever.
		let source = ChunkySource {
			data: png_bytes(300, 200),
			chunk: 1024,
			cancel: Arc::new(AtomicBool::new(false)),
			cancel_after_reads: Some(3),
			reads: AtomicUsize::new(0),
		};
		let mut out = Vec::new();
		let result = super::make_thumbnail_from_source(
			Box::new(source),
			32,
			32,
			super::DEFAULT_THUMBNAIL_MEM_BUDGET,
			&mut out,
		);
		assert!(result.is_err(), "expected a hard error, got {result:?}");
	}

	#[test]
	fn corrupt_bytes_are_the_cacheable_no_thumbnail_verdict() {
		// Sniffable as JPEG, undecodable past the marker: the same cacheable
		// None the sniff gives unsupported formats — not an error the caller
		// retries forever.
		let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
		bytes.extend_from_slice(&[0x00; 64]);
		let len = bytes.len() as u64;
		let mut out = Vec::new();
		let result = make_thumbnail(
			Some("image/jpeg"),
			len,
			Cursor::new(bytes),
			32,
			32,
			super::DEFAULT_THUMBNAIL_MEM_BUDGET,
			&mut out,
		)
		.unwrap();
		assert_eq!(result, None);
		assert!(out.is_empty());
	}
}
