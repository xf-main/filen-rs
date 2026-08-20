use std::{path::PathBuf, sync::Arc};

use filen_sdk_rs::{
	fs::{
		HasUUID,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::FilenMetaExt,
};
use futures::StreamExt;
use image::ImageError;
use tokio::sync::OwnedRwLockReadGuard;
use tracing::debug;
use uuid::Uuid;

use crate::{
	CacheError,
	auth::{AuthCacheState, CacheState, FilenMobileCacheState},
	ffi::FfiId,
	sql::{self, object::DBObject},
};

/// Sources larger than this are not worth downloading just to thumbnail them.
const MAX_THUMBNAIL_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// Upper bound on the pixels a single decode may materialise. Chosen ABOVE the
/// common phone sensors — 12 MP is 4032×3024 = 12,192,768 and 16 MP sensors run
/// to ~15.9M, so a cap below that silently denies most camera photos a
/// thumbnail (cached as "none" forever on iOS). Peak memory per decode:
/// ~8 bytes/pixel on the HEIF path (libheif's RGBA buffer plus the returned
/// copy, briefly) ≈ 130 MiB at this cap, ~3–4 bytes/pixel on the image-crate
/// path — and [`DECODE_GATE`] keeps at most [`MAX_CONCURRENT_DECODES`] decodes
/// alive, so the worst case stays bounded. HEIF sources over the cap fall back
/// to their embedded thumbnail; other formats over it get no thumbnail.
const MAX_THUMBNAIL_SOURCE_PIXELS: u64 = 17_000_000;

/// Files.app hands over entire grid batches at once; bound how many ITEMS are
/// in flight so a 1000-photo directory cannot queue hundreds of concurrent
/// downloads. Wider than the decode bound: downloads are disk-streamed and
/// cheap to overlap, and a slot waiting on a download (or a per-item lock) must
/// not starve cache hits behind it.
const THUMBNAIL_CONCURRENCY: usize = 4;

/// How many DECODES may run at once, process-wide — the decode buffers are the
/// memory hazard, and this gate (not the per-batch item bound) is what enforces
/// the budget in [`MAX_THUMBNAIL_SOURCE_PIXELS`]'s arithmetic. Global on
/// purpose: it also covers the single-item `get_thumbnail` path (Android's
/// only route) and concurrent batches.
const MAX_CONCURRENT_DECODES: usize = 2;

/// See [`MAX_CONCURRENT_DECODES`].
static DECODE_GATE: tokio::sync::Semaphore =
	tokio::sync::Semaphore::const_new(MAX_CONCURRENT_DECODES);

/// Removes the temp file a thumbnail encode writes into unless [`disarm`](Self::disarm)ed
/// after the rename into place — the one cleanup covering every exit, including the future
/// being dropped mid-await by a cancelled bulk batch.
struct TmpFileGuard {
	path: Option<PathBuf>,
}

impl TmpFileGuard {
	fn disarm(mut self) {
		self.path = None;
	}
}

impl Drop for TmpFileGuard {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			// Sync removal of one small local file; best-effort by design.
			let _ = std::fs::remove_file(path);
		}
	}
}

impl AuthCacheState {
	async fn get_or_make_thumbnail(
		&self,
		file: &RemoteFile,
		target_width: u32,
		target_height: u32,
	) -> Result<Option<PathBuf>, CacheError> {
		let Some(mime) = file.mime() else {
			debug!("File has no mime type, no thumbnail will be made");
			return Ok(None);
		};

		if !mime.starts_with("image/") {
			debug!("File is not an image, no thumbnail will be made: {mime}");
			return Ok(None);
		}
		let file_path = self.get_cached_file_path(file);
		let file_thumbnails_path = self.thumbnail_dir.join(file.uuid().to_string());
		tokio::fs::create_dir_all(&file_thumbnails_path).await?;
		let thumbnail_path =
			file_thumbnails_path.join(format!("{target_width}x{target_height}.webp"));
		match tokio::fs::metadata(&thumbnail_path).await {
			// Zero-size files are leftovers from the pre-atomic writer; regenerate them.
			Ok(meta) if FilenMetaExt::size(&meta) != 0 => {
				return Ok(Some(thumbnail_path));
			}
			Ok(_) => {}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
			Err(e) => return Err(e.into()),
		}
		let image_file = match tokio::fs::File::open(&file_path).await {
			Ok(file) => file,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				if file.size() > MAX_THUMBNAIL_SOURCE_BYTES {
					debug!(
						"File too large to download for a thumbnail ({} bytes): {}",
						file.size(),
						file_path.display()
					);
					return Ok(None);
				}
				debug!(
					"Thumbnail file not found, downloading: {}",
					file_path.display()
				);
				// Same serialisation the regular download path takes: without it a concurrent
				// clear can evict what this download just wrote, and two downloads of one file
				// interleave their writes into the same tmp path.
				let _local_file_guard = self.lock_local_file(file.uuid()).await;
				let path = self.download_file_io(file, None).await?;
				tokio::fs::File::open(&path).await?
			}
			Err(e) => {
				debug!(
					"Failed to open file for thumbnail: {} at path {}",
					e,
					file_path.display()
				);
				return Err(e.into());
			}
		};

		// Encode into a unique temp file and rename into place: the thumbnail
		// only becomes visible once complete, so neither a cancelled task nor a
		// concurrent request for the same size can surface (and cache) a torn
		// file.
		let tmp_path = file_thumbnails_path.join(format!(
			"{target_width}x{target_height}.{}.tmp",
			Uuid::new_v4()
		));
		let tmp_file = tokio::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&tmp_path)
			.await?;
		// One cleanup for every exit: the guard removes the temp file on drop unless
		// the rename below disarmed it — covering the error arms, a failed rename,
		// AND an aborted batch (cancel() drops this future mid-await while the
		// detached blocking closure keeps writing to the already-open, now-unlinked
		// file, which is harmless).
		let tmp_guard = TmpFileGuard {
			path: Some(tmp_path.clone()),
		};

		let (os_file, mut tmp_file) = futures::join!(image_file.into_std(), tmp_file.into_std());

		let mime = file.mime().map(|m| m.to_string());
		let size = file.size();

		// Decode buffers, not downloads, are the memory hazard — serialize decodes
		// process-wide. Acquired before the blocking task is spawned, so parked work
		// costs a future, not a blocked pool thread; MOVED INTO the closure so the
		// permit is released when the decode actually ends — a cancelled batch drops
		// this future while the blocking closure keeps running detached, and a permit
		// held by the dropped future would let fresh decodes stack on the orphans.
		// Never closed, so the expect is unreachable.
		let decode_permit = DECODE_GATE
			.acquire()
			.await
			.expect("the decode gate is never closed");
		let decode_result = tokio::task::spawn_blocking(
			move || -> Result<Option<(u32, u32)>, filen_sdk_rs::error::Error> {
				let _decode_permit = decode_permit;
				let image_reader = std::io::BufReader::new(os_file);
				filen_sdk_rs::thumbnail::make_thumbnail(
					mime.as_deref(),
					size,
					image_reader,
					target_width,
					target_height,
					MAX_THUMBNAIL_SOURCE_PIXELS,
					&mut tmp_file,
				)
			},
		)
		.await;

		let decode_result = match decode_result {
			Ok(result) => result,
			// A panicking decode must fail this item alone, not tear down the
			// whole bulk batch and leave its callback unanswered.
			Err(e) => {
				return Err(CacheError::image(format!(
					"thumbnail decode task failed: {e}"
				)));
			}
		};

		match decode_result {
			Ok(Some(_)) => {
				tokio::fs::rename(&tmp_path, &thumbnail_path).await?;
				tmp_guard.disarm();
				Ok(Some(thumbnail_path))
			}
			// The source exceeds the decode budget; there is no thumbnail to make.
			Ok(None) => Ok(None),
			Err(e) => match e.downcast::<ImageError>() {
				Ok((ImageError::Unsupported(_), _)) => Ok(None),
				Ok((e, context)) => Err(CacheError::from(e).context(context.join(": "))),
				Err(e) => Err(CacheError::from(e)),
			},
		}
	}

	async fn make_thumbnail_for_path(
		&self,
		path: &FfiId,
		requested_width: u32,
		requested_height: u32,
	) -> ThumbnailResult {
		// An identity-form (`stable/`) id selects its row directly: rebuilding a display path
		// and re-resolving it can hand back a same-named sibling's bytes as this id's thumbnail.
		let selected = if path.0.starts_with(crate::local::STABLE_PREFIX) {
			self.select_object_by_id(path)
		} else {
			let path = match self.canonicalize_id(path) {
				Ok(path) => path,
				Err(e) => return ThumbnailResult::Err(e),
			};
			match path.as_parsed() {
				Ok(pvs) => sql::select_object_at_parsed_id(&self.conn(), &pvs),
				Err(e) => return ThumbnailResult::Err(e),
			}
		};
		let file = match selected {
			Ok(Some(DBObject::File(file))) => file,
			Ok(Some(_)) => return ThumbnailResult::NoThumbnail,
			Ok(None) => return ThumbnailResult::NotFound,
			Err(e) => return ThumbnailResult::Err(e),
		};

		let remote_file = match file.try_into() {
			Ok(remote_file) => remote_file,
			Err(e) => return ThumbnailResult::Err(CacheError::from(e)),
		};
		match self
			.get_or_make_thumbnail(&remote_file, requested_width, requested_height)
			.await
		{
			Ok(Some(path)) => ThumbnailResult::Ok(path.to_string_lossy().to_string()),
			Ok(None) => ThumbnailResult::NoThumbnail,
			Err(e) => ThumbnailResult::Err(e),
		}
	}
}

#[derive(uniffi::Enum)]
pub enum ThumbnailResult {
	Ok(String),
	Err(CacheError),
	NotFound,
	NoThumbnail,
}

impl From<CacheError> for ThumbnailResult {
	fn from(e: CacheError) -> Self {
		ThumbnailResult::Err(e)
	}
}

#[uniffi::export(with_foreign)]
pub trait ThumbnailCallback: Send + Sync {
	fn process(&self, id: FfiId, result: ThumbnailResult);
	fn complete(&self);
}

#[derive(uniffi::Object)]
pub struct BulkThumbnailResponse {
	task: tokio::task::JoinHandle<()>,
}

#[uniffi::export]
impl BulkThumbnailResponse {
	pub fn cancel(&self) {
		if !self.task.is_finished() {
			self.task.abort();
		}
	}
}

impl AuthCacheState {
	pub(crate) fn get_thumbnails(
		this: OwnedRwLockReadGuard<CacheState, Self>,
		items: Vec<FfiId>,
		requested_width: u32,
		requested_height: u32,
		callback: Arc<dyn ThumbnailCallback>,
	) -> BulkThumbnailResponse {
		let arc = Arc::new(this);
		let handle = crate::env::get_runtime().spawn(async move {
			futures::stream::iter(items)
				.for_each_concurrent(THUMBNAIL_CONCURRENCY, |item| {
					let self_ref = arc.clone();
					let callback_ref = callback.clone();
					async move {
						let result = self_ref
							.make_thumbnail_for_path(&item, requested_width, requested_height)
							.await;
						callback_ref.process(item, result);
					}
				})
				.await;
			callback.complete();
		});

		BulkThumbnailResponse { task: handle }
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	pub fn get_thumbnails(
		self: Arc<Self>,
		items: Vec<FfiId>,
		requested_width: u32,
		requested_height: u32,
		callback: Arc<dyn ThumbnailCallback>,
	) -> Result<BulkThumbnailResponse, CacheError> {
		self.sync_execute_authed_owned(move |auth_state| {
			Ok(AuthCacheState::get_thumbnails(
				auth_state,
				items,
				requested_width,
				requested_height,
				callback,
			))
		})
	}
}

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	// not sure why this is necessary for this specific function,
	// but otherwise it seems like the macro wasn't adding this
	#[uniffi::method(name = "get_thumbnail")]
	pub async fn get_thumbnail(
		self: Arc<Self>,
		item: FfiId,
		requested_width: u32,
		requested_height: u32,
	) -> Result<ThumbnailResult, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			Ok(auth_state
				.make_thumbnail_for_path(&item, requested_width, requested_height)
				.await)
		})
		.await
	}
}
