use image::{DynamicImage, codecs::webp::WebPEncoder, imageops::FilterType};

use crate::{ErrorKind, error::Error};

// MUST BE SORTED ALPHABETICALLY
const SUPPORTED_THUMBNAIL_MIME_TYPES: &[&str] = &[
	// AVIF goes through `heif-decoder` — libheif on its dav1d backend, the
	// same container path HEIC takes, available on every target this builds
	// for.
	#[cfg(feature = "heif-decoder")]
	"image/avif",
	"image/gif",
	#[cfg(feature = "heif-decoder")]
	"image/heic",
	#[cfg(feature = "heif-decoder")]
	"image/heif",
	"image/jpeg",
	"image/png",
	"image/qoi",
	// Native only: the wasm build takes microthumb without its `svg` feature
	// (resvg is a large dependency for a format phones never produce), so
	// wasm keeps refusing SVG up front.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	"image/svg+xml",
	"image/tiff",
	"image/webp",
	"image/x-qoi",
];

pub fn is_supported_thumbnail_mime(mime: &str) -> bool {
	SUPPORTED_THUMBNAIL_MIME_TYPES.binary_search(&mime).is_ok()
}

/// Extensions worth spending bytes on, evaluated from the filename at READ
/// time.
///
/// This list is ours on purpose. The stored mime is written once at upload by
/// whichever client uploaded the file — `mime-types` (JS), `mime_guess` (Rust)
/// or Go's stdlib `mime` — and then never revisited, so it is a fossil of that
/// library's table at that date, on that machine:
///
/// - all three collapse "unknown extension", "no extension" and "genuinely
///   binary" into `application/octet-stream`, so a NEGATIVE carries no
///   information at all;
/// - HEIC — the dominant phone format — is stored as octet-stream by every
///   Rust client before June 2024 (when `mime_guess` gained it) and by Go
///   clients on macOS or in slim containers, where the host has no
///   `/etc/mime.types`;
/// - RAW formats are absent from the JS table entirely, and from Go's unless
///   the host happens to have `shared-mime-info` installed.
///
/// Deciding from the extension now, against a table we control, makes the
/// answer the same everywhere and lets a new format be added in one line
/// instead of waiting for three upstreams and a re-upload.
///
/// The RAW entries are attempts, not promises: they are TIFF or ISO-BMFF
/// containers that normally carry an embedded JPEG preview, which is exactly
/// what the pipeline's preview probe looks for. Whether every vendor's layout
/// yields one is unverified — no RAW fixtures were available — but a miss
/// costs one chunk and answers "no thumbnail", which is what they do today
/// anyway.
///
/// `avif` is present because the vendored libheif carries an AV1 decoder
/// (dav1d) alongside HEVC — the same container code path as HEIC (pinned by
/// `heif-decoder`'s `hevc_and_av1_decode`).
const THUMBNAILABLE_EXTENSIONS: &[&str] = &[
	// Formats the pipeline decodes directly. `svgz` is deliberately absent:
	// the pipeline refuses gzipped SVG (no inflate), so looking would cost a
	// chunk and answer nothing.
	"avif", "bmp", "gif", "heic", "heif", "hif", "jfif", "jpe", "jpeg", "jpg", "png", "qoi", "svg",
	"tif", "tiff", "webp", //
	// RAW: TIFF/BMFF containers, reached for their embedded preview.
	"3fr", "arw", "cr2", "cr3", "dng", "erf", "iiq", "kdc", "mos", "mrw", "nef", "nrw", "orf",
	"pef", "raf", "raw", "rw2", "rwl", "srf", "srw", "x3f",
];

/// Whether a thumbnail is worth attempting for this file.
///
/// Magic bytes remain the authority — this only decides whether to spend the
/// bytes to look.
///
/// **An extension, when there is one, decides on its own. The mime is
/// consulted only for names that carry no extension at all.** Every library
/// that wrote those stored mimes derived them from the extension in the first
/// place, so for a named file the extension is the same evidence, only fresher
/// and evaluated against a table we control. Falling back to the mime for
/// extensionless names still honours a client that supplied one explicitly —
/// a browser's `File.type`, say — which is the one case where the mime knows
/// something the name does not.
///
/// This is what keeps `psd`, `dwg`, `tga` and friends out despite their
/// `image/*` mimes: nothing here decodes them, so looking costs a chunk and
/// answers nothing. It also means a file whose extension lies (`photo.xyz`
/// holding a JPEG) is skipped even if
/// its mime says otherwise — accepted, because that mime can only have been
/// supplied by hand, and the alternative is paying a chunk for every unknown
/// extension on the drive.
///
/// Filename handling, all deliberate: the extension is what follows the LAST
/// dot, matching every library that produced the stored mimes, so
/// `photo.jpg.enc` reads as `enc` and is skipped. Matching is case-insensitive
/// and tolerates surrounding whitespace. A dotfile named `.jpg` counts as a
/// JPEG — permissive, and it costs at most a chunk to be wrong.
pub fn might_be_thumbnailable(name: Option<&str>, mime: Option<&str>) -> bool {
	match name.and_then(|name| name.rsplit_once('.')) {
		Some((_, ext)) => {
			let ext = ext.trim();
			THUMBNAILABLE_EXTENSIONS
				.iter()
				.any(|known| ext.eq_ignore_ascii_case(known))
		}
		None => mime.is_some_and(|mime| mime.starts_with("image/")),
	}
}

#[cfg(test)]
mod gate_tests {
	use super::might_be_thumbnailable;

	#[test]
	fn the_extension_rescues_what_the_stored_mime_lost() {
		// The case this exists for: HEIC stored as octet-stream by a Go client
		// on macOS, or by any Rust client predating mime_guess 2.0.5.
		assert!(might_be_thumbnailable(
			Some("IMG_0042.heic"),
			Some("application/octet-stream")
		));
		// RAW, absent from the JS table entirely.
		assert!(might_be_thumbnailable(
			Some("DSC_0001.NEF"),
			Some("application/octet-stream")
		));
		// AVIF, decoded by libheif's AV1 backend since dav1d was vendored — if
		// this ever goes back to refusing, the decoder went away with it.
		assert!(might_be_thumbnailable(
			Some("photo.avif"),
			Some("application/octet-stream")
		));
		assert!(might_be_thumbnailable(
			Some("photo.avif"),
			Some("image/avif")
		));
		// Uppercase is the norm straight off a camera.
		assert!(might_be_thumbnailable(Some("IMG_1234.JPG"), None));
		// SVG rasterises through the bounded pipeline; gzipped SVG does not
		// (the pipeline refuses it), so svgz stays out.
		assert!(might_be_thumbnailable(
			Some("logo.svg"),
			Some("image/svg+xml")
		));
		assert!(!might_be_thumbnailable(
			Some("logo.svgz"),
			Some("image/svg+xml")
		));
	}

	#[test]
	fn the_mime_is_the_fallback_only_when_there_is_no_extension() {
		// An explicitly-supplied mime is the only signal for a name that
		// carries no extension, so it is honoured.
		assert!(might_be_thumbnailable(Some("scan"), Some("image/jpeg")));
		assert!(might_be_thumbnailable(None, Some("image/png")));
		// But an extension we cannot decode wins over its own correct mime —
		// looking would cost a chunk to learn nothing.
		assert!(!might_be_thumbnailable(
			Some("art.psd"),
			Some("image/vnd.adobe.photoshop")
		));
	}

	#[test]
	fn everything_else_is_left_alone() {
		assert!(!might_be_thumbnailable(
			Some("archive.zip"),
			Some("application/zip")
		));
		// A wrapper suffix hides the real extension from every mime library
		// too, so the stored mime is octet-stream and this stays skipped.
		assert!(!might_be_thumbnailable(
			Some("photo.jpg.enc"),
			Some("application/octet-stream")
		));
		// No dot, nothing to go on.
		assert!(!might_be_thumbnailable(
			Some("IMG_1234"),
			Some("application/octet-stream")
		));
		assert!(!might_be_thumbnailable(None, None));
		// A dotfile is treated as its extension, deliberately.
		assert!(might_be_thumbnailable(Some(".jpg"), None));
	}
}

// ---------------------------------------------------------------------------
// The bounded pipeline. ONE core function; everything below it is a wrapper
// that differs only in where the bytes come from and where they go.
// ---------------------------------------------------------------------------

pub use microthumb::{
	APP_PROCESS_MEM_BUDGET as APP_PROCESS_THUMBNAIL_MEM_BUDGET, ByteSource,
	DEFAULT_MEM_BUDGET as DEFAULT_THUMBNAIL_MEM_BUDGET, FileSource, MemSource, ThumbSource,
	ThumbSpec,
};

/// Sources bigger than this are not worth streaming end to end to make one
/// small thumbnail. Past it the pipeline is restricted to embedded previews
/// ([`ThumbSpec::allow_full_decode`] off), which cost a chunk or two whatever
/// the file size — so a 200 MB HEIC still gets its `thmb` item, and a source
/// with nothing embedded answers [`ThumbnailOutcome::OverBudget`] having read
/// almost nothing.
pub const MAX_THUMBNAIL_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// Resident bytes a [`RemoteChunkSource`] keeps for the whole decode (its two
/// chunk slots) — callers subtract this from the budget they hand
/// [`make_thumbnail_from_source`], because the pipeline's own accounting
/// cannot see the source's memory.
pub const REMOTE_SOURCE_RESIDENT_BYTES: usize = 2 * crate::consts::CHUNK_SIZE_U64 as usize;

/// The thumbnail that was written, and how it was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailInfo {
	pub width: u32,
	pub height: u32,
	/// Cheap embedded preview vs a real decode — orders of magnitude apart in
	/// bytes fetched and CPU spent, and otherwise indistinguishable from the
	/// outside.
	pub source: ThumbSource,
}

/// How a thumbnail attempt ended, short of transport/system trouble (which
/// stays an `Err`).
///
/// Every variant here is a CACHEABLE answer about these bytes: a caller that
/// remembers "no thumbnail" per item — which both mobile platforms do — may
/// store any of them without risking a permanent wrong verdict from an offline
/// blip. They are kept apart because they mean different things: `Unsupported`
/// is final, `OverBudget` is a verdict about the spec (the same file may
/// thumbnail once its bytes are local, or under a roomier budget), and
/// `Corrupt` is about the bitstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbnailOutcome {
	Thumbnail(ThumbnailInfo),
	/// Not an image format this build decodes. Decided by the magic-byte
	/// sniff, not by the stored mime.
	Unsupported,
	/// A recognised image, but nothing fit: over the memory budget, over the
	/// pipeline's work ceiling, or a full decode was needed and the spec only
	/// allowed an embedded preview.
	OverBudget,
	/// A recognised format whose bitstream the decoder refused. Carries the
	/// decoder's message for logging.
	Corrupt(String),
}

/// How the bounded canvas is fitted into the requested box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbnailFit {
	/// Aspect preserved, fitted INSIDE the box — the result is usually shorter
	/// than the box on one axis.
	Contain,
	/// Cropped to fill the box exactly, which is what a fixed-size grid tile
	/// wants.
	Cover,
}

/// Encodes a webp thumbnail of `source` into `out` without ever materialising
/// a full decoded frame: microthumb streams every format it can (IDCT-scaled
/// JPEG, PNG scanlines, HEIF tiles, embedded previews) and refuses anything
/// whose honest decode peak exceeds `spec.mem_budget`
/// ([`DEFAULT_THUMBNAIL_MEM_BUDGET`] fits an iOS file-provider extension's
/// ~20 MB jetsam ceiling; [`APP_PROCESS_THUMBNAIL_MEM_BUDGET`] is for a whole
/// app process or browser tab).
///
/// Nothing is written to `out` unless the returned outcome is
/// [`ThumbnailOutcome::Thumbnail`]. The output is never upscaled past what was
/// decoded — microthumb's canvas is routinely SMALLER than the request (the
/// budget clamps it, and an embedded preview can be smaller still) and
/// blowing that back up would only mean a blurrier thumbnail in a BIGGER
/// payload.
///
/// This is the single decode path in the SDK. It is synchronous by contract:
/// callers run it off their async runtime's threads and hand it a `source`
/// that knows how to fetch bytes from wherever it lives.
pub fn make_thumbnail_from_source<W>(
	source: Box<dyn ByteSource>,
	spec: &ThumbSpec,
	fit: ThumbnailFit,
	out: &mut W,
) -> Result<ThumbnailOutcome, Error>
where
	W: std::io::Write,
{
	let thumb = match microthumb::generate(source, spec) {
		Ok(microthumb::ThumbOutcome::Thumbnail(thumb)) => thumb,
		Ok(microthumb::ThumbOutcome::Unsupported) => return Ok(ThumbnailOutcome::Unsupported),
		Ok(microthumb::ThumbOutcome::OverBudget) => return Ok(ThumbnailOutcome::OverBudget),
		// Corrupt or refused bytes are a verdict about the image, not a
		// failure of the request — the caller may cache them exactly like the
		// other two. Only transport/system errors stay hard: an offline blip
		// must never stick a permanent "no thumbnail" in a platform cache.
		Err(e @ (microthumb::ThumbError::Decode(_) | microthumb::ThumbError::Geometry)) => {
			return Ok(ThumbnailOutcome::Corrupt(e.to_string()));
		}
		// A decoder that ran off the end of the bytes is describing the IMAGE,
		// not the transport, and some report it as an io error rather than a
		// decode one (the `png` crate does; `jpeg-decoder` does not). None of
		// the sources here can produce `UnexpectedEof`: a read past the end
		// answers `Ok(0)`, a cancel answers `Interrupted`, and a failed fetch
		// answers `Other`. So this can only have been synthesised by a decoder
		// reading a truncated file — and left classified as transport it would
		// make a truncated PNG retry forever instead of settling.
		Err(microthumb::ThumbError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
			return Ok(ThumbnailOutcome::Corrupt(e.to_string()));
		}
		Err(microthumb::ThumbError::Io(e)) => return Err(e.into()),
	};
	let Some(rgba) =
		image::RgbaImage::from_vec(thumb.image.width, thumb.image.height, thumb.image.rgba)
	else {
		return Err(Error::custom(
			ErrorKind::ImageError,
			"thumbnail canvas has an inconsistent buffer".to_string(),
		));
	};
	let img = DynamicImage::ImageRgba8(rgba);
	let width = spec.target_width.min(img.width());
	let height = spec.target_height.min(img.height());
	let thumbnail = match fit {
		ThumbnailFit::Contain => img.resize(width, height, FilterType::CatmullRom),
		ThumbnailFit::Cover => img.resize_to_fill(width, height, FilterType::CatmullRom),
	};
	thumbnail.write_with_encoder(WebPEncoder::new_lossless(out))?;
	Ok(ThumbnailOutcome::Thumbnail(ThumbnailInfo {
		width: thumbnail.width(),
		height: thumbnail.height(),
		source: thumb.source,
	}))
}

// Everything from here on needs a way to fetch remote bytes and a thread it may
// block. The `service-worker` profile has neither — it is built WITHOUT atomics,
// where `std`'s parker is a silent no-op rather than a real wait — so none of it
// is compiled there.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
mod remote_chunks {
	use super::{ByteSource, Error};
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	use super::{ErrorKind, ThumbSpec, ThumbnailFit, ThumbnailOutcome, make_thumbnail_from_source};
	use crate::{
		auth::Client,
		fs::file::{RemoteFile, traits::HasFileInfo},
	};
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};

	/// Reads one plaintext byte range of a remote file.
	///
	/// Shared by both bridges below: what a synchronous `read_at` awaits is the
	/// same everywhere, only HOW it gets to await it differs per target.
	async fn fetch_range(
		client: &Client,
		file: &dyn crate::fs::file::traits::File,
		start: u64,
		end: u64,
	) -> Result<Vec<u8>, Error> {
		use futures::AsyncReadExt;

		let mut reader = crate::fs::file::read::FileReaderBuilder::new(client.unauthed(), file)
			.with_start(start)
			.with_end(end)
			.build();
		let mut data = Vec::with_capacity((end - start) as usize);
		reader.read_to_end(&mut data).await?;
		Ok(data)
	}

	/// [`ByteSource`] that fetches and decrypts 1 MiB chunks of a remote file on
	/// demand, keeping only the last two — so a thumbnail served by an embedded
	/// preview costs one chunk of download, not the file.
	///
	/// Synchronous by contract, because the pipeline is. The bridge to the async
	/// fetch is the one genuinely per-target piece: on native a
	/// `Handle::block_on` inside `spawn_blocking`, on wasm a channel round-trip
	/// to the async runtime from a dedicated worker (see
	/// [`over_requests`](Self::over_requests)). Everything else — the two-slot
	/// cache, the cancel contract, the short reads at chunk boundaries — is
	/// shared.
	pub struct RemoteChunkSource {
		fetch: Box<dyn FnMut(u64, u64) -> std::io::Result<Vec<u8>> + Send>,
		/// Checked before every read: cancellation surfaces as
		/// `ErrorKind::Interrupted`, which unwinds the decode through its normal
		/// error path at chunk granularity — the same points an async decoder
		/// would get to cancel at.
		cancel: Option<Arc<AtomicBool>>,
		len: u64,
		slots: [Option<(u64, Vec<u8>)>; 2],
		next_evict: usize,
	}

	impl RemoteChunkSource {
		fn with_fetcher(
			len: u64,
			cancel: Option<Arc<AtomicBool>>,
			fetch: Box<dyn FnMut(u64, u64) -> std::io::Result<Vec<u8>> + Send>,
		) -> Self {
			RemoteChunkSource {
				fetch,
				cancel,
				len,
				slots: [None, None],
				next_evict: 0,
			}
		}

		/// The native bridge: a miss blocks the calling thread on the async
		/// fetch, which is legal because the pipeline runs inside
		/// `spawn_blocking`.
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		pub fn new<Id: Send + Sync + 'static>(
			client: Arc<Client>,
			file: RemoteFile<Id>,
			handle: tokio::runtime::Handle,
			cancel: Option<Arc<AtomicBool>>,
		) -> Self {
			let len = file.size();
			Self::with_fetcher(
				len,
				cancel,
				Box::new(move |start, end| {
					handle
						.block_on(fetch_range(&client, &file, start, end))
						.map_err(std::io::Error::other)
				}),
			)
		}

		/// The wasm bridge: a miss posts the range to the async runtime and
		/// parks this thread on the reply.
		///
		/// Only ever driven from [`decode_worker`], which is where the parking
		/// is legal; the driver side is [`thumbnail_remote_file`].
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		fn over_requests(
			len: u64,
			cancel: Option<Arc<AtomicBool>>,
			requests: tokio::sync::mpsc::UnboundedSender<ChunkRequest>,
		) -> Self {
			// One reply channel for the whole decode, not one per request: the
			// source blocks until each answer arrives, so there is never more
			// than one in flight.
			let (reply, replies) = std::sync::mpsc::channel();
			Self::with_fetcher(
				len,
				cancel,
				Box::new(move |start, end| {
					// A dropped driver (the caller's future went away) closes
					// one of these; either way the decode unwinds at the same
					// chunk granularity a cancel flag would give it.
					let cancelled = || {
						std::io::Error::new(std::io::ErrorKind::Interrupted, "thumbnail cancelled")
					};
					requests
						.send(ChunkRequest {
							start,
							end,
							reply: reply.clone(),
						})
						.map_err(|_| cancelled())?;
					replies.recv().map_err(|_| cancelled())?
				}),
			)
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
				.is_some_and(|c| c.load(Ordering::Relaxed))
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
			let data = (self.fetch)(start, end)?;
			let slot = self.next_evict;
			self.next_evict = (self.next_evict + 1) % self.slots.len();
			self.slots[slot] = Some((index, data));
			Ok(&self.slots[slot].as_ref().expect("just stored").1)
		}
	}

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

	/// One chunk fetch, crossing from the decode worker to the async runtime.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	struct ChunkRequest {
		start: u64,
		end: u64,
		reply: std::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
	}

	/// Runs the synchronous pipeline on ONE long-lived dedicated wasm worker.
	///
	/// Which thread this is matters, in three ways:
	/// - **Not the commander.** It drives every async task in the SDK, the very
	///   chunk fetches a blocked decode waits for included.
	/// - **Not a rayon worker.** `runtime::do_cpu_intensive` IS the rayon pool,
	///   and chunk DECRYPTION runs there too — a decode parked on a rayon worker
	///   would be waiting on the pool that has to run to unblock it.
	/// - **One worker, not one per call.** Each `runtime::spawn` instantiates a
	///   fresh wasm module and its thread state in the shared linear memory that
	///   is never returned to the host, and retains the worker's JS wrapper for
	///   the life of the spawning thread. A worker per thumbnail would grow
	///   exactly the memory this pipeline exists to bound. Serialising decodes
	///   is what the budget assumes anyway.
	///
	/// Parking here is legal: `wasm-full` builds with `+atomics`, where `std`
	/// selects the futex parker, and this is a dedicated worker rather than the
	/// JS main thread.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	mod decode_worker {
		use std::sync::{OnceLock, mpsc};

		type Job = Box<dyn FnOnce() + Send>;

		static JOBS: OnceLock<mpsc::Sender<Job>> = OnceLock::new();

		pub(super) fn submit<T: Send + 'static>(
			job: impl FnOnce() -> T + Send + 'static,
		) -> tokio::sync::oneshot::Receiver<T> {
			let (result_tx, result_rx) = tokio::sync::oneshot::channel();
			let jobs = JOBS.get_or_init(|| {
				let (tx, rx) = mpsc::channel::<Job>();
				crate::runtime::spawn(move || {
					// `recv` parks this worker between jobs. The sender lives in
					// a static, so it never errors and the worker never exits —
					// which is the point: the wasm module is instantiated once.
					while let Ok(job) = rx.recv() {
						job();
					}
				});
				tx
			});
			// The receiving worker never exits, so this cannot fail.
			let _ = jobs.send(Box::new(move || {
				let _ = result_tx.send(job());
			}));
			result_rx
		}
	}

	/// Thumbnails a remote file straight from its chunks, off the async
	/// runtime's threads: the webp bytes and the verdict, without the file ever
	/// being resident in full.
	///
	/// The two arms differ only in how the synchronous pipeline is taken off the
	/// runtime and how its source gets back on it.
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	pub(crate) async fn thumbnail_remote_file<Id: Send + Sync + 'static>(
		client: Arc<Client>,
		file: RemoteFile<Id>,
		spec: ThumbSpec,
	) -> Result<(ThumbnailOutcome, Vec<u8>), Error> {
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			// A dropped caller (a cancelled request, a closed view) leaves the
			// blocking closure running detached with live network fetches. The
			// guard flips the source's cancel flag so its next read answers
			// Interrupted instead.
			struct CancelOnDrop(Option<Arc<AtomicBool>>);
			impl Drop for CancelOnDrop {
				fn drop(&mut self) {
					if let Some(flag) = self.0.take() {
						flag.store(true, Ordering::Relaxed);
					}
				}
			}
			let cancel = Arc::new(AtomicBool::new(false));
			let mut guard = CancelOnDrop(Some(cancel.clone()));
			let source = RemoteChunkSource::new(
				client,
				file,
				tokio::runtime::Handle::current(),
				Some(cancel),
			);
			let joined = tokio::task::spawn_blocking(move || {
				let mut webp = Vec::new();
				let outcome = make_thumbnail_from_source(
					Box::new(source),
					&spec,
					ThumbnailFit::Contain,
					&mut webp,
				)?;
				Ok::<_, Error>((outcome, webp))
			})
			.await;
			// The decode is over — nothing is left for a late drop to cancel.
			guard.0 = None;
			joined.map_err(|e| {
				Error::custom(
					ErrorKind::ImageError,
					format!("thumbnail decode task failed: {e}"),
				)
			})?
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel();
			// No cancel flag: dropping this future drops `incoming`, and the
			// source's next request fails the same way.
			let source = RemoteChunkSource::over_requests(file.size(), None, requests);
			let mut done = decode_worker::submit(move || {
				let mut webp = Vec::new();
				make_thumbnail_from_source(
					Box::new(source),
					&spec,
					ThumbnailFit::Contain,
					&mut webp,
				)
				.map(|outcome| (outcome, webp))
			});
			let died = || {
				Error::custom(
					ErrorKind::ImageError,
					"thumbnail decode worker died".to_string(),
				)
			};
			loop {
				tokio::select! {
					biased;
					result = &mut done => return result.map_err(|_| died())?,
					request = incoming.recv() => {
						let Some(request) = request else {
							// The source was dropped inside the worker: no
							// further chunk can be asked for, only a result.
							return done.await.map_err(|_| died())?;
						};
						let data = fetch_range(&client, &file, request.start, request.end)
							.await
							.map_err(std::io::Error::other);
						let _ = request.reply.send(data);
					}
				}
			}
		}
	}
}

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub use remote_chunks::RemoteChunkSource;

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod js_impls {
	use filen_macros::js_type;

	use super::{
		APP_PROCESS_THUMBNAIL_MEM_BUDGET, MAX_THUMBNAIL_SOURCE_BYTES, REMOTE_SOURCE_RESIDENT_BYTES,
		ThumbSource, ThumbSpec, ThumbnailOutcome, is_supported_thumbnail_mime,
		remote_chunks::thumbnail_remote_file,
	};
	use crate::{
		Error,
		auth::JsClient,
		error::MetadataWasNotDecryptedError,
		fs::file::{AnonymousRemoteFile, traits::HasFileInfo},
		js::File,
		runtime::do_on_commander,
	};

	#[js_type(import)]
	pub struct MakeThumbnailInMemoryParams {
		pub file: File,
		pub max_width: u32,
		pub max_height: u32,
	}

	/// A thumbnail, and what it cost to make.
	#[js_type(export)]
	pub struct InMemoryThumbnail {
		// this is correct, ts requires the specifity
		// because of https://github.com/microsoft/typescript/issues/62546
		// not sure who to upstream this to, tsify, js_sys, or wasm-bindgen
		#[cfg(feature = "wasm-full")]
		#[tsify(type = "Uint8Array<ArrayBuffer>")]
		pub webp_data: serde_bytes::ByteBuf,
		#[cfg(feature = "uniffi")]
		pub webp_data: Vec<u8>,
		/// Dimensions of the encoded image. Never larger than the request and
		/// often smaller: the memory budget clamps the decode canvas, an
		/// embedded preview can be smaller still, and nothing is ever upscaled.
		pub width: u32,
		pub height: u32,
		/// True when this came from the image's own embedded preview (EXIF
		/// IFD1, a HEIF `thmb` item) rather than a decode of the full image —
		/// a couple of container reads against, at worst, a whole-file stream.
		/// The first question worth asking when thumbnails feel slow.
		pub from_embedded_preview: bool,
	}

	/// What came of a thumbnail request.
	///
	/// Only the transport can fail outright (that arrives as a rejected
	/// promise / a thrown error); every variant here is a settled, CACHEABLE
	/// answer about this file, and they are kept apart because they mean
	/// different things to a UI: `unsupported` is final, `overBudget` says the
	/// image was recognised but too expensive for this host to render at this
	/// size, and `corrupt` says the bytes themselves are broken.
	#[js_type(export, no_deser, tagged)]
	pub enum MakeThumbnailInMemoryResult {
		Thumbnail {
			thumbnail: InMemoryThumbnail,
		},
		/// Not an image format this build decodes — decided by the file's
		/// magic bytes, not by its stored mime.
		Unsupported,
		/// A recognised image, but no thumbnail fits: over the host's memory
		/// budget, over the pipeline's work ceiling, or too large to stream and
		/// carrying no embedded preview.
		OverBudget,
		/// A recognised format whose bitstream the decoder refused.
		Corrupt {
			message: String,
		},
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	// Every sibling `impl JsClient` carries this and this one did not, so the
	// method compiled for uniffi (its result type even has a `Vec<u8>` field
	// written specifically for that boundary) but never reached the generated
	// Kotlin/Swift — the one consumer of the unbounded path was unable to call
	// it. Exported now that the path is bounded.
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "makeThumbnailInMemory")
		)]
		pub async fn make_thumbnail_in_memory(
			&self,
			params: MakeThumbnailInMemoryParams,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				// Thumbnailing only reads the file, so a file from a link or a
				// shared-in listing (which reports no stable id) is fine.
				let file = AnonymousRemoteFile::try_from(params.file)?;
				let mime = file.mime().ok_or(MetadataWasNotDecryptedError)?;
				if !is_supported_thumbnail_mime(mime) {
					return Ok(MakeThumbnailInMemoryResult::Unsupported);
				}
				let spec = ThumbSpec {
					target_width: params.max_width,
					target_height: params.max_height,
					// The chunk source's two resident slots live outside the
					// pipeline's own accounting; hand the decode what is
					// actually left.
					mem_budget: APP_PROCESS_THUMBNAIL_MEM_BUDGET
						.saturating_sub(REMOTE_SOURCE_RESIDENT_BYTES),
					// Past the cap, the embedded preview is still worth a
					// chunk or two — streaming the whole thing is not.
					allow_full_decode: file.size() <= MAX_THUMBNAIL_SOURCE_BYTES,
				};
				let (outcome, webp_data) = thumbnail_remote_file(this, file, spec).await?;
				Ok(match outcome {
					ThumbnailOutcome::Thumbnail(info) => MakeThumbnailInMemoryResult::Thumbnail {
						thumbnail: InMemoryThumbnail {
							#[cfg(feature = "wasm-full")]
							webp_data: serde_bytes::ByteBuf::from(webp_data),
							#[cfg(feature = "uniffi")]
							webp_data,
							width: info.width,
							height: info.height,
							from_embedded_preview: info.source == ThumbSource::EmbeddedPreview,
						},
					},
					ThumbnailOutcome::Unsupported => MakeThumbnailInMemoryResult::Unsupported,
					ThumbnailOutcome::OverBudget => MakeThumbnailInMemoryResult::OverBudget,
					ThumbnailOutcome::Corrupt(message) => {
						MakeThumbnailInMemoryResult::Corrupt { message }
					}
				})
			})
			.await
		}
	}
}

#[cfg(test)]
mod tests {
	use microthumb::MemSource;

	use super::{
		DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbSpec, ThumbnailFit, ThumbnailOutcome,
		make_thumbnail_from_source,
	};

	fn png_bytes(width: u32, height: u32) -> Vec<u8> {
		use image::{ImageFormat, RgbImage};
		let image = RgbImage::from_fn(width, height, |x, y| {
			image::Rgb([(x % 256) as u8, (y % 256) as u8, 0])
		});
		let mut bytes = Vec::new();
		image
			.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
			.unwrap();
		bytes
	}

	/// The shape every caller uses: bytes in, webp out, plus a verdict.
	fn thumbnail(
		bytes: Vec<u8>,
		target: u32,
		mem_budget: usize,
		fit: ThumbnailFit,
	) -> (ThumbnailOutcome, Vec<u8>) {
		let mut out = Vec::new();
		let outcome = make_thumbnail_from_source(
			Box::new(MemSource(bytes)),
			&ThumbSpec::new(target, target, mem_budget),
			fit,
			&mut out,
		)
		.unwrap();
		(outcome, out)
	}

	#[test]
	fn refuses_sources_above_the_memory_budget() {
		// A budget too small even for PNG's row buffers: the pipeline must
		// refuse rather than decode past its ceiling — and say that the FORMAT
		// was fine, which is what makes the verdict retryable elsewhere.
		let (outcome, out) = thumbnail(png_bytes(100, 80), 32, 1024, ThumbnailFit::Cover);
		assert_eq!(outcome, ThumbnailOutcome::OverBudget);
		assert!(out.is_empty());
	}

	#[test]
	fn makes_a_thumbnail_within_the_default_budget() {
		let (outcome, out) = thumbnail(
			png_bytes(100, 80),
			32,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn cover_crops_to_the_box_and_contain_keeps_the_aspect() {
		// The one thing the two wrappers ask for differently: a grid tile wants
		// an exact square, an in-memory preview wants the picture's shape.
		let (cover, _) = thumbnail(
			png_bytes(400, 200),
			64,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		let ThumbnailOutcome::Thumbnail(cover) = cover else {
			panic!("expected a thumbnail");
		};
		assert_eq!((cover.width, cover.height), (64, 64));

		let (contain, _) = thumbnail(
			png_bytes(400, 200),
			64,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Contain,
		);
		let ThumbnailOutcome::Thumbnail(contain) = contain else {
			panic!("expected a thumbnail");
		};
		assert_eq!((contain.width, contain.height), (64, 32));
	}

	#[test]
	fn never_upscales_small_sources() {
		let (outcome, _) = thumbnail(
			png_bytes(16, 16),
			64,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (16, 16));
	}

	use std::sync::Arc;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

	/// A [`microthumb::ByteSource`] with [`super::RemoteChunkSource`]'s
	/// observable behavior, minus the network: short reads at every chunk
	/// boundary, and a cancel flag answered with `Interrupted` — optionally
	/// flipped by the source itself after N reads, standing in for a batch
	/// cancel landing mid-decode.
	struct ChunkySource {
		data: Vec<u8>,
		chunk: usize,
		cancel: Arc<AtomicBool>,
		cancel_after_reads: Option<usize>,
		reads: AtomicUsize,
	}

	impl microthumb::ByteSource for ChunkySource {
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
		let outcome = make_thumbnail_from_source(
			Box::new(source),
			&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
			ThumbnailFit::Cover,
			&mut out,
		)
		.unwrap();
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn a_cancel_mid_decode_is_a_hard_error_not_a_cached_verdict() {
		// The flag flips after the header reads succeed, the way a batch
		// cancel lands mid-decode. Interrupted must surface as Err — never as
		// a settled verdict, which the platform would cache forever.
		let source = ChunkySource {
			data: png_bytes(300, 200),
			chunk: 1024,
			cancel: Arc::new(AtomicBool::new(false)),
			cancel_after_reads: Some(3),
			reads: AtomicUsize::new(0),
		};
		let mut out = Vec::new();
		let result = make_thumbnail_from_source(
			Box::new(source),
			&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
			ThumbnailFit::Cover,
			&mut out,
		);
		assert!(result.is_err(), "expected a hard error, got {result:?}");
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn svg_rasterises_through_the_bounded_pipeline() {
		let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="100" height="100" fill="red"/></svg>"#.to_vec();
		let (outcome, out) =
			thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected an svg thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn corrupt_bytes_are_a_settled_verdict_not_an_error() {
		// Sniffable as JPEG, undecodable past the marker: a verdict the caller
		// may cache, not an error it retries forever — and distinguishable
		// from "we do not decode this format".
		let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
		bytes.extend_from_slice(&[0x00; 64]);
		let (outcome, out) =
			thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
		assert!(
			matches!(outcome, ThumbnailOutcome::Corrupt(_)),
			"got {outcome:?}"
		);
		assert!(out.is_empty());
	}

	#[test]
	fn a_truncated_image_is_a_verdict_too_even_when_its_decoder_calls_it_io() {
		// `png` reports running off the end as an io error, not a decode one.
		// That must not be mistaken for the network dropping: only the sources
		// can speak for the transport, and none of them produce UnexpectedEof.
		let mut bytes = png_bytes(300, 200);
		bytes.truncate(bytes.len() / 3);
		let (outcome, out) =
			thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
		assert!(
			matches!(outcome, ThumbnailOutcome::Corrupt(_)),
			"got {outcome:?}"
		);
		assert!(out.is_empty());
	}

	#[test]
	fn bytes_that_are_not_an_image_are_unsupported() {
		let (outcome, out) = thumbnail(
			b"this is not an image at all, not even close".to_vec(),
			32,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		assert_eq!(outcome, ThumbnailOutcome::Unsupported);
		assert!(out.is_empty());
	}
}
