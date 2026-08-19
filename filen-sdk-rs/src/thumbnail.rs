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
	use microthumb::{ByteSource, ThumbError, ThumbSpec};

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

	fn thumb_error(e: ThumbError) -> Error {
		match e {
			ThumbError::Io(e) => e.into(),
			other => Error::custom(ErrorKind::ImageError, other.to_string()),
		}
	}

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
	) -> Result<Option<(u32, u32)>, Error>
	where
		R: BufRead + Seek + Send + 'static,
		W: Write,
	{
		let source = ReadSeekSource {
			inner: image_reader,
			len: image_file_size,
		};
		let spec = ThumbSpec {
			target_width,
			target_height,
			mem_budget,
		};
		let Some(small) = microthumb::generate(Box::new(source), &spec).map_err(thumb_error)?
		else {
			return Ok(None);
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
		Ok(Some((created_width, created_height)))
	}

	/// [`microthumb::ByteSource`] that fetches and decrypts 1 MiB chunks of a
	/// remote file on demand, keeping only the last two — so a thumbnail served
	/// by an embedded preview costs one chunk of download, not the file. Sync by
	/// contract (microthumb runs inside `spawn_blocking`); each miss bridges to
	/// the async chunk fetch via `Handle::block_on`, which is legal there.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub struct RemoteChunkSource<'a> {
		client: &'a crate::auth::unauth::UnauthClient,
		file: &'a dyn crate::fs::file::traits::File,
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
	impl<'a> RemoteChunkSource<'a> {
		pub fn new(
			client: &'a crate::auth::unauth::UnauthClient,
			file: &'a dyn crate::fs::file::traits::File,
			handle: tokio::runtime::Handle,
			cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
		) -> Self {
			RemoteChunkSource {
				client,
				file,
				handle,
				cancel,
				len: file.size(),
				slots: [None, None],
				next_evict: 0,
			}
		}

		fn chunk(&mut self, index: u64) -> std::io::Result<&Vec<u8>> {
			// Two fixed slots instead of a map: header + payload locality is all
			// the demuxers need, and eviction keeps the source at ≤2 MiB.
			if let Some(slot) = self
				.slots
				.iter()
				.position(|s| s.as_ref().is_some_and(|(i, _)| *i == index))
			{
				return Ok(&self.slots[slot].as_ref().expect("just matched").1);
			}
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
			let start = index * crate::consts::CHUNK_SIZE_U64;
			let end = (start + crate::consts::CHUNK_SIZE_U64).min(self.len);
			let (client, file) = (self.client, self.file);
			let data = self
				.handle
				.block_on(async move {
					use futures::AsyncReadExt;
					let mut reader = crate::fs::file::read::FileReaderBuilder::new(client, file)
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
	impl ByteSource for RemoteChunkSource<'_> {
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
pub use bounded::{DEFAULT_THUMBNAIL_MEM_BUDGET, RemoteChunkSource, make_thumbnail};

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
		assert_eq!(result, Some((32, 32)));
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
		assert_eq!(result, Some((16, 16)));
	}
}
