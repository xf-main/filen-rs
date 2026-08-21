//! The live path: an always-on socket subscription feeding `native_cache.db` directly.
//!
//! An `NSFileProviderReplicatedExtension` never re-enumerates a MATERIALIZED container on its own
//! — the working set is the only channel by which remote changes to such a container reach the
//! system, and the working set only moves when something writes this database. This module is
//! that writer: it subscribes to the drive's socket events **unconditionally at auth** (no
//! engine, no sync roots, no registration dance), applies each event through the same upserts a
//! listing refresh uses — so the change-sequence and tombstone triggers fire exactly as they do
//! for anything else — and pokes the working-set listener, whose platform side answers with
//! `signalEnumerator(.workingSet)`.
//!
//! One channel, one drainer: the socket thread's callback only deep-copies the event and sends
//! it; a single consumer applies strictly in delivery order, so an edit's `fileNew` can never be
//! undone by the trash event that retired its predecessor being applied after it.
//!
//! The relevance gate keeps this O(1) per event and stops the cache growing on its own: an event
//! is applied only when the cache already holds the row it names OR that row's parent. Anything
//! else is a container this device has never listed — dropped, because that folder is listed on
//! first browse anyway.
//!
//! LOCK DISCIPLINE, inherited from the tracking module this replaces: tokio's `RwLock` is fair,
//! so a queued auth-refresh writer parks every later reader behind whoever holds a read guard.
//! The drainer takes the guard PER EVENT, and the common (local-only) arms hold it across
//! nothing but SQL. The one arm that reaches the network (`trashEmpty`'s trash relist) holds it
//! the way every FFI operation already does — for its own duration only.

use std::{
	borrow::Cow,
	sync::{
		Arc, Mutex, Weak,
		atomic::{AtomicBool, Ordering},
	},
};

use filen_sdk_rs::{
	fs::{
		HasUUID,
		categories::NonRootItemType,
		dir::RemoteDirectory,
		file::{RemoteFile, meta::FileMeta},
	},
	socket::{DecryptedDriveEvent, DecryptedSocketEvent, ListenerHandle},
};
use filen_types::{
	fs::{ParentUuid, StableUuid, Uuid, UuidStr},
	traits::{CowHelpers, CowHelpersExt},
};
use futures::StreamExt;
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{
	RwLock,
	mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::{
	CacheError,
	auth::{AuthCacheState, AuthStatus, CacheState, FilenMobileCacheState},
	sql::{
		dir::DBDir, error::OptionalExtensionSQL, file::DBFile, item::RawDBItem, object::DBObject,
	},
	traits::WorkingSetUpdateListener,
};

/// One deep-copied socket event, on its way from the socket thread to the drainer.
type OwnedEvent = DecryptedSocketEvent<'static>;

/// Everything the live path owns, hanging off [`AuthCacheState`] so deauth tears it down:
/// dropping the listener handle unsubscribes, dropping the state aborts the drainer.
#[derive(Default)]
pub(crate) struct LiveState {
	/// Claimed exactly once per authenticated state; a failed start releases it so a later call
	/// retries.
	started: AtomicBool,
	handle: Mutex<Option<LiveHandle>>,
}

struct LiveHandle {
	/// Held for its `Drop`: it unregisters the callback from the socket's routing table.
	_listener: ListenerHandle,
	drainer: tokio::task::JoinHandle<()>,
}

impl Drop for LiveState {
	fn drop(&mut self) {
		if let Some(handle) = lock(&self.handle).take() {
			handle.drainer.abort();
		}
	}
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn conn(mutex: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
	lock(mutex)
}

/// The dispatch-side names of every event the drainer consumes. Registering the list (rather
/// than a global listener) keeps the socket layer from decrypting chat/note traffic nobody here
/// reads. `authSuccess`/`reconnecting` are the connection-state cues the gap path keys on.
fn live_event_types() -> Vec<Cow<'static, str>> {
	[
		"fileArchiveRestored",
		"fileNew",
		"fileRestore",
		"fileMove",
		"fileTrash",
		"fileArchived",
		"folderTrash",
		"folderMove",
		"folderSubCreated",
		"folderRestore",
		"folderColorChanged",
		"trashEmpty",
		"itemFavorite",
		"fileDeletedPermanent",
		"folderMetadataChanged",
		"folderDeletedPermanent",
		"fileMetadataChanged",
		"deleteAll",
		"deleteVersioned",
		"driveMalformed",
		"authSuccess",
		"reconnecting",
	]
	.into_iter()
	.map(Cow::Borrowed)
	.collect()
}

/// Brings the live path up for the current authenticated state, if it is not up already.
/// Idempotent and cheap when it is (one read guard, one atomic swap), so it is called from every
/// path that (re)establishes auth. Fire-and-forget: a failure only means the next call retries.
pub(crate) fn ensure_started(state: &Arc<RwLock<CacheState>>) {
	// Fast path, called from every authenticated FFI entry: no task and no queueing when the
	// live path is already up (or the state is not authenticated). `try_read` failing means a
	// writer is active — fall through to the task, which waits its turn.
	if let Ok(guard) = state.try_read() {
		match &guard.status {
			AuthStatus::Authenticated(auth) if !auth.live.started.load(Ordering::Acquire) => {}
			_ => return,
		}
	}
	let state = state.clone();
	crate::env::get_runtime().spawn(async move {
		// Phase 1, GUARDED and local-only: check auth, claim the start, clone the client.
		let client = {
			let guard = state.read().await;
			let AuthStatus::Authenticated(auth) = &guard.status else {
				return;
			};
			if auth.live.started.swap(true, Ordering::AcqRel) {
				return;
			}
			auth.client.clone()
		};

		// Phase 2 with the guard RELEASED (see the module's lock discipline). Subscribe FIRST —
		// before any gap check the launch path runs — so nothing can land between a check and
		// the subscription; `add_event_listener_sync` registers the callback synchronously even
		// while the socket is still connecting, so no event past this point is missed.
		let (sender, receiver) = unbounded_channel();
		let listener = match client
			.add_event_listener_sync(live_callback(sender), Some(live_event_types()))
			.await
		{
			Ok(listener) => listener,
			Err(e) => {
				tracing::warn!("live socket subscription failed: {e}");
				// Release the claim so a later auth-path call retries the subscription.
				let guard = state.read().await;
				if let AuthStatus::Authenticated(auth) = &guard.status
					&& Arc::ptr_eq(&auth.client, &client)
				{
					auth.live.started.store(false, Ordering::Release);
				}
				return;
			}
		};
		let drainer =
			crate::env::get_runtime().spawn(drain_live_events(Arc::downgrade(&state), receiver));

		// One brief re-acquire to install the handle. A re-auth may have replaced the state in
		// between — a subscription on the old client belongs to nobody, so it is dropped (which
		// unregisters it) and the drainer aborted rather than filed under the new state.
		let guard = state.read().await;
		match &guard.status {
			AuthStatus::Authenticated(auth) if Arc::ptr_eq(&auth.client, &client) => {
				*lock(&auth.live.handle) = Some(LiveHandle {
					_listener: listener,
					drainer,
				});
				tracing::debug!("live socket path started");
			}
			_ => drainer.abort(),
		}
	});
}

/// The socket thread's side: deep-copy and send, nothing else. It runs on the thread draining
/// the websocket and must not block; a send failing means the drainer is gone, where dropping
/// the event is exactly right.
fn live_callback(
	sender: UnboundedSender<OwnedEvent>,
) -> Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static> {
	Box::new(move |event| {
		let _ = sender.send(event.to_owned_cow());
	})
}

/// The ONE consumer: applies events strictly in the order the socket delivered them.
///
/// The gap pass runs INSIDE this loop (on `authSuccess`, which the socket broadcasts on every
/// connect and reconnect), which is what closes the snapshot race by construction: events
/// arriving while a pass's listings are in flight buffer in the channel and are applied only
/// after every listing has landed — a listing taken before a rename can therefore never be
/// applied after the rename's event and silently revert it.
async fn drain_live_events(
	state: Weak<RwLock<CacheState>>,
	mut events: UnboundedReceiver<OwnedEvent>,
) {
	while let Some(event) = events.recv().await {
		let Some(state) = state.upgrade() else {
			return;
		};
		match event {
			// Everything on the drive is gone — or so the event says. Never applied as a bulk
			// delete: the reconciling pass tells "gone" from "unreachable" per container, and
			// the guarded forgets keep any unsent local edit. The watermark advances only if
			// the pass converged; otherwise the pass is recorded as owed and the next gap check
			// retries it.
			DecryptedSocketEvent::Drive {
				inner: DecryptedDriveEvent::DeleteAll,
				drive_message_id,
			} => {
				tracing::warn!("deleteAll received; reconciling every materialized container");
				reconcile_pass(&state, drive_message_id).await;
				notify_working_set_changed(&state).await;
			}
			DecryptedSocketEvent::Drive {
				inner,
				drive_message_id,
			} => {
				let event_type = inner.event_type();
				let applied = {
					let guard = state.read().await;
					let AuthStatus::Authenticated(auth) = &guard.status else {
						continue;
					};
					auth.apply_drive_event(inner).await
				};
				match applied {
					Ok(applied) => {
						// Advanced AFTER the apply committed, never before: a crash between
						// the two re-delivers (via the gap pass) instead of skipping forever.
						advance_watermark(&state, drive_message_id).await;
						if applied {
							tracing::debug!("applied live {event_type} event");
							notify_working_set_changed(&state).await;
						}
					}
					Err(e) => {
						// The event did NOT commit, so the watermark must not move past it —
						// but holding it back is not enough on its own: the NEXT event advances
						// the watermark past this id, and a watermark-only gap check would then
						// see nothing to close. The debt is recorded durably instead, and the
						// next gap check re-lists what this failed to apply. Best-effort beyond
						// that: the cache is authoritative for nothing.
						tracing::warn!("failed to apply live {event_type} event: {e}");
						mark_pending_reconcile(&state).await;
					}
				}
			}
			// A drive event happened whose payload could not be read. Nothing to apply — and
			// nothing to fear for the watermark: the id is known, and the affected item
			// reconciles on its next event or browse.
			DecryptedSocketEvent::DriveMalformed { drive_message_id } => {
				advance_watermark(&state, drive_message_id).await;
			}
			// The socket (re)connected: check for a gap and close it. This is ALSO the launch
			// path's check — the first authSuccess after subscribing is the first moment a gap
			// is even answerable, and it cannot race the subscription that produced it.
			DecryptedSocketEvent::AuthSuccess => {
				gap_check(&state).await;
			}
			// Remaining connection-state cues need no action: a reconnect ends in its own
			// authSuccess, and a failed auth delivers nothing to fall behind on.
			_ => {}
		}
	}
}

/// Compares the persisted watermark against the account's drive counter and, on any gap — or on
/// a durably owed pass — re-lists every reported materialized container, the only directories
/// whose staleness the system cannot heal by itself. The watermark advances to the PRE-PASS
/// remote counter, and only when every container converged; a partial pass leaves it put and
/// records the debt, because the watermark alone cannot hold a retry open (the next event
/// advances it past the gap).
async fn gap_check(state: &Arc<RwLock<CacheState>>) {
	let (client, watermark, owed) = {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return;
		};
		(
			auth.client.clone(),
			auth.drive_watermark(),
			auth.pending_reconcile() > 0,
		)
	};
	// NO state guard across the round trip (the module's lock discipline).
	let remote = match client.get_last_event_ids().await {
		Ok(response) => response.drive,
		Err(e) => {
			tracing::debug!("gap check could not read the drive counter: {e}");
			return;
		}
	};
	if !pass_owed(owed, watermark, remote) {
		return;
	}
	tracing::info!(
		"drive event gap (local {watermark:?}, remote {remote}, owed {owed}); re-listing \
		 materialized containers"
	);
	reconcile_pass(state, remote).await;
	notify_working_set_changed(state).await;
}

/// The gap gate: a pass runs when the remote counter has moved past our watermark, and ALSO
/// whenever one is durably owed — a container that failed to converge, an event that failed to
/// apply, a container reported for the first time. The debt has to be its own answer here,
/// because the events that follow the failure advance the watermark past the gap and leave the
/// counters saying "caught up" over a container nothing has re-listed.
fn pass_owed(owed: bool, watermark: Option<u64>, remote: u64) -> bool {
	owed || watermark.is_none_or(|local| local < remote)
}

/// One reconcile pass and the durable bookkeeping it owes: the watermark advances to `id` and
/// the pending flag clears only when EVERY container converged. A partial pass leaves the
/// watermark where it was AND records the debt, which is what makes the retry survive the later
/// events that carry the watermark past `id`.
async fn reconcile_pass(state: &Arc<RwLock<CacheState>>, id: u64) {
	// Read BEFORE the container set is, so a report landing mid-pass fails the compare-and-clear
	// below instead of being cleared by a pass that never listed it.
	let owed = {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return;
		};
		auth.pending_reconcile()
	};
	if reconcile_containers(state).await {
		advance_watermark(state, id).await;
		clear_pending_reconcile(state, owed).await;
	} else {
		mark_pending_reconcile(state).await;
	}
}

async fn mark_pending_reconcile(state: &Arc<RwLock<CacheState>>) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.mark_pending_reconcile().await;
	}
}

async fn clear_pending_reconcile(state: &Arc<RwLock<CacheState>>, owed: u64) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.clear_pending_reconcile(owed).await;
	}
}

/// Re-lists every reported materialized container (the account root included), concurrency
/// bounded, best-effort per container — one failure must not abort the pass. Returns whether
/// EVERY container converged, which is what gates the watermark.
async fn reconcile_containers(state: &Arc<RwLock<CacheState>>) -> bool {
	let containers = {
		let guard = state.read().await;
		let AuthStatus::Authenticated(auth) = &guard.status else {
			return false;
		};
		auth.working_set_containers()
	};
	let results: Vec<bool> = futures::stream::iter(containers.into_iter().map(|uuid| {
		let state = state.clone();
		async move {
			// The guard is PER CONTAINER, so a queued writer waits for one listing at most.
			let guard = state.read().await;
			let AuthStatus::Authenticated(auth) = &guard.status else {
				return false;
			};
			auth.refresh_container(Uuid::from(uuid)).await
		}
	}))
	.buffer_unordered(CONTAINER_PROBE_CONCURRENCY)
	.collect()
	.await;
	results.into_iter().all(|converged| converged)
}

async fn advance_watermark(state: &Arc<RwLock<CacheState>>, id: u64) {
	let guard = state.read().await;
	if let AuthStatus::Authenticated(auth) = &guard.status {
		auth.advance_drive_watermark(id).await;
	}
}

async fn notify_working_set_changed(state: &Arc<RwLock<CacheState>>) {
	state.read().await.notify_working_set();
}

/// What the pure dispatch decided about one event: either it was applied (or dropped) locally,
/// or it needs one of the three operations that reach past the database — a guarded forget, or
/// the trash relist.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalApply {
	/// Locally settled; `true` means a row changed.
	Applied(bool),
	/// The lineage is gone for good — retire it through the pending-guarded forget path.
	ForgetFile(StableUuid),
	ForgetDir(Uuid),
	/// The trash was emptied elsewhere — reconcile our trashed rows against a fresh listing.
	RelistTrash,
}

/// Applies one decrypted drive event to the database, or names the follow-up it needs.
///
/// Pure with respect to everything but the connection, which is what makes the whole 19-arm
/// dispatch — folds, relevance gate, trash semantics — testable against an in-memory database.
/// Every arm goes through the same upserts a server refresh uses, so the change-sequence and
/// tombstone triggers fire identically, and the pending-upload marker column stays untouchable.
pub(crate) fn apply_drive_event_local(
	conn_mutex: &Mutex<Connection>,
	event: DecryptedDriveEvent<'static>,
) -> Result<LocalApply, CacheError> {
	use filen_sdk_rs::socket as ev;
	Ok(LocalApply::Applied(match event {
		// A full file record: a new upload, an edit's new head, a restore from the trash, or a
		// move. One arm, because they all carry the same thing and land the same way — the
		// stable tier of the upsert resolves them onto the row we hold.
		DecryptedDriveEvent::FileNew(ev::FileNew(file))
		| DecryptedDriveEvent::FileRestore(ev::FileRestore(file))
		| DecryptedDriveEvent::FileMove(ev::FileMove(file)) => apply_file_record(conn_mutex, file)?,
		// A version restore replacing the current head: `file` IS the new head.
		DecryptedDriveEvent::FileArchiveRestored(e) => apply_file_record(conn_mutex, e.file)?,
		// `new_uuid` set means this is the RETIREMENT half of a versioning-disabled edit, not a
		// user trash — the successor arrives as its own `fileNew` (in no guaranteed order), and
		// trashing here would flicker the live row through the trash.
		DecryptedDriveEvent::FileTrash(e) => {
			if e.new_uuid.is_some() {
				false
			} else {
				apply_file_trash(conn_mutex, e.uuid, e.stable_uuid)?
			}
		}
		// `new_uuid` set is a normal edit's archive of the superseded version — the row already
		// follows the lineage. Absent, the lineage itself was REPLACED (move-with-replace):
		// gone for good.
		DecryptedDriveEvent::FileArchived(e) => {
			if e.new_uuid.is_some() {
				false
			} else {
				return Ok(LocalApply::ForgetFile(e.stable_uuid));
			}
		}
		// `stable_uuid` absent means only archived VERSIONS died; the live head stands.
		DecryptedDriveEvent::FileDeletedPermanent(e) => match e.stable_uuid {
			Some(stable) => return Ok(LocalApply::ForgetFile(stable)),
			None => false,
		},
		// A rename (or other metadata change): no full record comes with it, so the new meta is
		// patched onto the row we hold. Keyed by the whole-life id — a rename arriving after an
		// edit re-minted the uuid still names the same lineage.
		DecryptedDriveEvent::FileMetadataChanged(e) => {
			apply_file_meta(conn_mutex, e.stable_uuid, e.metadata)?
		}
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir))
		| DecryptedDriveEvent::FolderMove(ev::FolderMove(dir))
		| DecryptedDriveEvent::FolderRestore(ev::FolderRestore(dir)) => {
			apply_dir_record(conn_mutex, dir)?
		}
		DecryptedDriveEvent::FolderTrash(e) => apply_dir_trash(conn_mutex, e.uuid)?,
		DecryptedDriveEvent::FolderColorChanged(e) => apply_dir_patch(conn_mutex, e.uuid, |dir| {
			dir.color = e.color.into_owned_cow();
		})?,
		DecryptedDriveEvent::FolderMetadataChanged(e) => {
			apply_dir_patch(conn_mutex, e.uuid, |dir| {
				dir.meta = e.meta.into_owned_cow();
			})?
		}
		DecryptedDriveEvent::FolderDeletedPermanent(e) => {
			return Ok(LocalApply::ForgetDir(e.uuid));
		}
		// Carries the full record, so it lands like any other — including the favorite flag,
		// which the per-type upsert folds into the local rank.
		DecryptedDriveEvent::ItemFavorite(ev::ItemFavorite(item)) => match item {
			NonRootItemType::File(file) => apply_file_record(conn_mutex, file.into_owned())?,
			NonRootItemType::Dir(dir) => apply_dir_record(conn_mutex, dir.into_owned())?,
		},
		// The trash emptied elsewhere. The rows to retire are exactly the trashed rows we hold,
		// and the trash relist already reconciles them properly (per-row probes, pending markers
		// released on definitive not-found) — reuse it rather than guess. Gated on actually
		// holding trashed rows, so an account that never trashed anything pays nothing.
		DecryptedDriveEvent::TrashEmpty => {
			if holds_trashed_rows(&conn(conn_mutex))? {
				return Ok(LocalApply::RelistTrash);
			}
			false
		}
		// Everything on the drive is gone. Deliberately NOT applied as a bulk delete: an unsent
		// local edit somewhere in the tree outranks the event, and the reconciling paths
		// (browse, the launch pass) already tell "gone" from "unreachable" per item. Rare
		// enough that correctness wins over immediacy.
		DecryptedDriveEvent::DeleteAll => {
			tracing::warn!("deleteAll received; leaving reconciliation to the refresh paths");
			false
		}
		// Only archived versions died; this cache holds heads only.
		DecryptedDriveEvent::DeleteVersioned => false,
	}))
}

/// The relevance gate for a full file record: the row (by whole-life id, then uuid), or its
/// parent.
fn file_record_is_relevant(conn: &Connection, file: &RemoteFile) -> Result<bool, rusqlite::Error> {
	Ok(
		RawDBItem::select_by_stable(conn, file.stable_uuid.into())?.is_some()
			|| RawDBItem::select(conn, file.uuid())?.is_some()
			|| parent_row_exists(conn, &file.parent)?,
	)
}

fn apply_file_record(conn_mutex: &Mutex<Connection>, file: RemoteFile) -> Result<bool, CacheError> {
	if !file_record_is_relevant(&conn(conn_mutex), &file)? {
		return Ok(false);
	}
	DBFile::upsert_from_remote(&mut conn(conn_mutex), file)?;
	Ok(true)
}

fn apply_dir_record(
	conn_mutex: &Mutex<Connection>,
	dir: RemoteDirectory,
) -> Result<bool, CacheError> {
	let relevant = {
		let conn = conn(conn_mutex);
		RawDBItem::select(&conn, dir.uuid())?.is_some() || parent_row_exists(&conn, &dir.parent)?
	};
	if !relevant {
		return Ok(false);
	}
	DBDir::upsert_from_remote(&mut conn(conn_mutex), dir)?;
	Ok(true)
}

/// A remote TRASH is a trash, not a delete: the row stays, marked trashed with its original
/// parent (where a restore puts it back), and its local bytes stay with it — this cache's own
/// trash model. Goes through `upsert_from_remote` with a `Trash` parent, exactly how a trash
/// listing delivers the same fact, so the change-seq trigger fires and no tombstone does.
fn apply_file_trash(
	conn_mutex: &Mutex<Connection>,
	uuid: Uuid,
	stable: StableUuid,
) -> Result<bool, CacheError> {
	let held = {
		let conn = conn(conn_mutex);
		match RawDBItem::select_by_stable(&conn, stable.into())? {
			Some(item) => DBFile::select(&conn, item.uuid).optional()?,
			None => DBFile::select(&conn, uuid).optional()?,
		}
	};
	let Some(file) = held else {
		return Ok(false);
	};
	let mut remote = RemoteFile::try_from(file)?;
	match remote.parent {
		// Already trashed: the row is where it belongs.
		ParentUuid::Trash(_) => Ok(false),
		ParentUuid::Uuid(parent) => {
			remote.parent = ParentUuid::Trash(parent);
			DBFile::upsert_from_remote(&mut conn(conn_mutex), remote)?;
			Ok(true)
		}
		// A row parented to a virtual container has no original parent to restore to; leave it
		// for the next refresh rather than invent one.
		other => {
			tracing::warn!("trashed file {uuid} has no restorable parent ({other:?})");
			Ok(false)
		}
	}
}

fn apply_dir_trash(conn_mutex: &Mutex<Connection>, uuid: Uuid) -> Result<bool, CacheError> {
	let held = { DBObject::select(&conn(conn_mutex), uuid).optional()? };
	let Some(DBObject::Dir(dir)) = held else {
		return Ok(false);
	};
	let mut remote = RemoteDirectory::from(dir);
	match remote.parent {
		ParentUuid::Trash(_) => Ok(false),
		ParentUuid::Uuid(parent) => {
			remote.parent = ParentUuid::Trash(parent);
			DBDir::upsert_from_remote(&mut conn(conn_mutex), remote)?;
			Ok(true)
		}
		other => {
			tracing::warn!("trashed dir {uuid} has no restorable parent ({other:?})");
			Ok(false)
		}
	}
}

fn apply_file_meta(
	conn_mutex: &Mutex<Connection>,
	stable: StableUuid,
	meta: FileMeta<'static>,
) -> Result<bool, CacheError> {
	let held = {
		let conn = conn(conn_mutex);
		match RawDBItem::select_by_stable(&conn, stable.into())? {
			Some(item) => DBFile::select(&conn, item.uuid).optional()?,
			None => None,
		}
	};
	let Some(file) = held else {
		return Ok(false);
	};
	let mut remote = RemoteFile::try_from(file)?;
	remote.meta = meta;
	DBFile::upsert_from_remote(&mut conn(conn_mutex), remote)?;
	Ok(true)
}

fn apply_dir_patch(
	conn_mutex: &Mutex<Connection>,
	uuid: Uuid,
	patch: impl FnOnce(&mut RemoteDirectory),
) -> Result<bool, CacheError> {
	let held = { DBObject::select(&conn(conn_mutex), uuid).optional()? };
	let Some(DBObject::Dir(dir)) = held else {
		return Ok(false);
	};
	let mut remote = RemoteDirectory::from(dir);
	patch(&mut remote);
	DBDir::upsert_from_remote(&mut conn(conn_mutex), remote)?;
	Ok(true)
}

fn holds_trashed_rows(conn: &Connection) -> Result<bool, rusqlite::Error> {
	conn.query_one(
		"SELECT EXISTS (SELECT 1 FROM items WHERE trashed = TRUE);",
		[],
		|row| row.get(0),
	)
}

/// Whether the cache holds a row for the container an incoming record is parented to.
fn parent_row_exists(conn: &Connection, parent: &ParentUuid) -> Result<bool, rusqlite::Error> {
	let uuid = match parent {
		ParentUuid::Uuid(uuid) | ParentUuid::Trash(uuid) => *uuid,
		_ => return Ok(false),
	};
	Ok(RawDBItem::select(conn, uuid)?.is_some())
}

impl AuthCacheState {
	/// Applies one decrypted drive event to the cache. `Ok(true)` when a row changed — the cue
	/// to signal the working set. The local dispatch does everything that stays inside the
	/// database; the follow-ups it names run here, where the io and the client live.
	pub(crate) async fn apply_drive_event(
		&self,
		event: DecryptedDriveEvent<'static>,
	) -> Result<bool, CacheError> {
		// The applier is the WRITER on the listing barrier (see `listing_barrier`): an event
		// waits out every listing already in flight, so a pre-event snapshot can never be
		// applied on top of it and revert it. SQL only under the write guard — the follow-ups
		// below reach past the database, and the barrier is the one lock every browse listing
		// queues on.
		let outcome = {
			let _apply = self.listing_barrier.write().await;
			apply_drive_event_local(self.conn_mutex(), event)?
		};
		// The follow-ups run UNGUARDED, all three of them. A forget takes the item's transfer
		// lock (`io_delete_local`), which a download or upload of that same item holds for its
		// whole duration — under the write guard that would park every browse listing, and the
		// drainer itself, behind a transfer that can take minutes. There is nothing for the
		// barrier to order here anyway: a forget fetches nothing, and the trash relist takes the
		// barrier as a reader of its own (and must not nest inside a write guard).
		//
		// The guard cannot instead be scoped to the row delete inside `forget_item`: that
		// function is also reached from `refresh_file`/`refresh_dir` while they hold the barrier
		// as READERS, and a write guard requested from inside a read guard deadlocks on itself.
		// The cost of going unguarded is that a listing already in flight can re-add the row it
		// snapshotted before the delete — a stale row with no local bytes, which the next
		// listing of its parent sweeps.
		match outcome {
			LocalApply::Applied(applied) => Ok(applied),
			LocalApply::ForgetFile(stable) => self.forget_by_stable(stable).await,
			LocalApply::ForgetDir(uuid) => self.forget_dir(uuid).await,
			LocalApply::RelistTrash => {
				self.update_trash().await?;
				Ok(true)
			}
		}
	}

	/// Retires a file lineage the server says is gone for good, through the same guarded path a
	/// refresh uses: an unsent local edit keeps the row.
	async fn forget_by_stable(&self, stable: StableUuid) -> Result<bool, CacheError> {
		let held = {
			let conn = self.conn();
			match RawDBItem::select_by_stable(&conn, stable.into())? {
				Some(item) => DBFile::select(&conn, item.uuid).optional()?,
				None => None,
			}
		};
		let Some(file) = held else {
			return Ok(false);
		};
		Ok(self.forget_item(DBObject::File(file)).await?.is_none())
	}

	async fn forget_dir(&self, uuid: Uuid) -> Result<bool, CacheError> {
		let held = { DBObject::select(&self.conn(), uuid).optional()? };
		let Some(obj @ DBObject::Dir(_)) = held else {
			return Ok(false);
		};
		Ok(self.forget_item(obj).await?.is_none())
	}
}

impl AuthCacheState {
	/// The container set the working-set predicate binds: every reported materialized container
	/// plus — unconditionally — the account root. The root is `type = 0` and excluded from the
	/// working set itself, so without this injection every top-level item would be a non-member
	/// and the root folder would stay exactly as stale as an unreported container.
	pub(crate) fn working_set_containers(&self) -> Vec<UuidStr> {
		let set = self
			.materialized_containers
			.read()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		set.iter()
			.copied()
			.chain(std::iter::once(self.client.root().uuid()))
			.map(UuidStr::from)
			.collect()
	}

	pub(crate) fn drive_watermark(&self) -> Option<u64> {
		*lock(&self.drive_watermark)
	}

	/// Advances the drive watermark to `id` (monotonic; a lower id is a no-op) and mirrors it to
	/// `db_state.json`. Called AFTER an apply commits, never before — the difference between a
	/// crash re-delivering a change and skipping it forever.
	pub(crate) async fn advance_drive_watermark(&self, id: u64) {
		{
			let mut watermark = lock(&self.drive_watermark);
			if watermark.is_some_and(|current| current >= id) {
				return;
			}
			*watermark = Some(id);
		}
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			// Monotonic against a concurrent writer of the same file too.
			state.drive_watermark = Some(state.drive_watermark.unwrap_or(0).max(id));
		})
		.await
		{
			tracing::warn!("failed to persist the drive watermark: {e}");
		}
	}

	/// The reconcile debt this cache carries (see [`crate::auth::SavedDBState::pending_reconcile`]):
	/// zero means none. Any other value means a pass is owed, and IDENTIFIES that debt — a pass
	/// clears only the value it saw.
	pub(crate) fn pending_reconcile(&self) -> u64 {
		self.pending_reconcile.load(Ordering::Acquire)
	}

	/// Records that a reconcile pass is owed, durably: the flag has to outlive both the process
	/// and the watermark advances that follow it.
	pub(crate) async fn mark_pending_reconcile(&self) {
		if self.pending_reconcile.fetch_add(1, Ordering::AcqRel) > 0 {
			// Already owed and already persisted; the bump alone invalidates any pass in flight.
			return;
		}
		self.persist_pending_reconcile(true).await;
	}

	/// Clears the debt `owed` — and only that one. A mark that landed since (a report of a
	/// container this pass never listed) leaves it standing for the next pass.
	pub(crate) async fn clear_pending_reconcile(&self, owed: u64) {
		if owed == 0
			|| self
				.pending_reconcile
				.compare_exchange(owed, 0, Ordering::AcqRel, Ordering::Acquire)
				.is_err()
		{
			return;
		}
		self.persist_pending_reconcile(false).await;
	}

	async fn persist_pending_reconcile(&self, pending: bool) {
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			state.pending_reconcile = Some(pending);
		})
		.await
		{
			tracing::warn!("failed to persist the pending-reconcile flag: {e}");
		}
	}

	/// One container's share of a reconcile pass: relist it in place, `true` when it converged.
	/// A container the cache holds no dir row for converges trivially — the seeding probes own
	/// it, and its children cannot be staler than unknown. A listing that fails is classified
	/// with one probe: definitively gone converges through the guarded forget; anything else is
	/// a transient failure the next pass retries.
	pub(crate) async fn refresh_container(&self, uuid: Uuid) -> bool {
		let held = match DBObject::select(&self.conn(), uuid).optional() {
			Ok(held) => held,
			Err(e) => {
				tracing::warn!("could not read container {uuid}: {e}");
				return false;
			}
		};
		let mut dir = match held {
			Some(DBObject::Dir(dir)) => crate::sql::DBDirObject::Dir(dir),
			Some(DBObject::Root(root)) => crate::sql::DBDirObject::Root(root),
			// A file id in the set (a mis-filtered report) has nothing to list, ever.
			Some(DBObject::File(_)) => return true,
			// No row: the report's seeding probe failed (an offline launch). NOT convergence —
			// claiming it would advance the watermark past a container that was never listed
			// and re-create the stale-forever bug. Seed it here, or hold the watermark back so
			// the next connect retries.
			None => match self.client.get_dir(uuid).await {
				Ok(remote) => match DBDir::upsert_from_remote(&mut self.conn(), remote) {
					Ok(dir) => crate::sql::DBDirObject::Dir(dir),
					Err(e) => {
						tracing::warn!("could not seed container {uuid} for the pass: {e}");
						return false;
					}
				},
				// Definitively gone (or never a dir): nothing below it can be stale.
				Err(e) if e.kind() == filen_sdk_rs::ErrorKind::FolderNotFound => return true,
				Err(e) => {
					tracing::debug!("could not seed container {uuid} for the pass: {e}");
					return false;
				}
			},
		};
		match self.inner_update_dir(&mut dir).await {
			Ok(()) => true,
			Err(e) => {
				// `CacheError` folds the SDK error kind away, so classify with one probe.
				if matches!(
					self.client.get_dir(uuid).await,
					Err(probe) if probe.kind() == filen_sdk_rs::ErrorKind::FolderNotFound
				) {
					tracing::debug!("container {uuid} is gone; forgetting it");
					self.forget_dir(uuid).await.is_ok()
				} else {
					tracing::warn!("re-listing container {uuid} failed: {e}");
					false
				}
			}
		}
	}

	/// Replaces the reported materialized-container set (see the FFI wrapper below).
	///
	/// The ORDER inside is load-bearing: the set is stored — in memory and in `db_state.json` —
	/// BEFORE any probe runs, because the probes are recovery, not a gate. An offline launch
	/// whose probes all fail must still hold the full set, or it silently reproduces the
	/// stale-forever bug for the process lifetime.
	pub(crate) async fn set_materialized_containers(
		&self,
		ids: Vec<String>,
	) -> Result<(), CacheError> {
		let root = self.client.root().uuid();
		let mut parsed: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
		for id in &ids {
			// The platform reports the identifiers it holds, which are `stable/<uuid>` for
			// directories; take the bare form too.
			let bare = id.strip_prefix(crate::local::STABLE_PREFIX).unwrap_or(id);
			match bare.parse::<Uuid>() {
				// The root is injected at query time, always; storing it would only duplicate.
				Ok(uuid) if uuid != root => {
					parsed.insert(uuid);
				}
				Ok(_) => {}
				Err(e) => {
					tracing::warn!("ignoring unparseable materialized container id {id}: {e}");
				}
			}
		}
		tracing::debug!("materialized containers reported: {} ids", parsed.len());

		let mirrored: Vec<UuidStr> = parsed.iter().copied().map(UuidStr::from).collect();
		let added = {
			let mut held = self
				.materialized_containers
				.write()
				.unwrap_or_else(|poisoned| poisoned.into_inner());
			let added = parsed.iter().any(|uuid| !held.contains(uuid));
			*held = parsed.clone();
			added
		};
		// A container reported for the first time has never been kept fresh by this cache, and
		// the watermark says nothing about it — it may sit at the remote counter while this
		// container's contents are months old. Owe a pass for it. (After the set is stored, so
		// the pass that answers the debt lists it.)
		if added {
			self.mark_pending_reconcile().await;
		}
		// Mirrored durably, best-effort: a failed write costs the NEXT launch's pre-drain
		// freshness, never this session's.
		if let Err(e) = crate::auth::update_saved_db_state(&self.cache_state_file, move |state| {
			state.materialized_containers = Some(mirrored);
		})
		.await
		{
			tracing::warn!("failed to mirror the materialized containers to db_state.json: {e}");
		}

		// Best-effort probes seed rows for containers the (possibly wiped) cache has never
		// heard of, so their children can resolve. A failed probe never drops the id.
		let missing: Vec<Uuid> = {
			let conn = self.conn();
			parsed
				.into_iter()
				.filter(|uuid| !matches!(RawDBItem::select(&conn, *uuid), Ok(Some(_))))
				.collect()
		};
		futures::stream::iter(missing.into_iter().map(|uuid| async move {
			match self.client.get_dir(uuid).await {
				Ok(dir) => {
					if let Err(e) = DBDir::upsert_from_remote(&mut self.conn(), dir) {
						tracing::warn!("failed to seed materialized container {uuid}: {e}");
					}
				}
				// Recovery only: the id stays in the set whatever this said.
				Err(e) => {
					tracing::debug!("could not seed materialized container {uuid}: {e}");
				}
			}
		}))
		.buffer_unordered(CONTAINER_PROBE_CONCURRENCY)
		.collect::<Vec<()>>()
		.await;
		Ok(())
	}
}

/// Concurrency for the container seeding probes (and the gap pass's listings): 4 keeps the
/// worst case around a few MB of listing buffers inside a ~20 MB extension.
const CONTAINER_PROBE_CONCURRENCY: usize = 4;

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	/// Replaces the set of MATERIALIZED CONTAINERS the platform reports — the directories the
	/// system has synced to disk and will never re-enumerate on its own, whose contents are
	/// therefore this cache's to keep fresh through the working set. Wholesale replace on every
	/// call (idempotent, self-healing after a missed report); ids are container uuids in either
	/// bare or `stable/<uuid>` form. Call it with the drained
	/// `enumeratorForMaterializedItems` set at launch and again (debounced) from
	/// `materializedItemsDidChange`.
	pub async fn set_materialized_containers(&self, ids: Vec<String>) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.set_materialized_containers(ids).await
		})
		.await
	}
}

impl CacheState {
	/// Tell the replica something in its working set moved. Bounced off the runtime like the
	/// search update callback, so a slow consumer cannot hold up the apply that woke it.
	pub(crate) fn notify_working_set(&self) {
		let listener = lock(&self.working_set_listener).clone();
		if let Some(listener) = listener {
			tokio::task::spawn_blocking(move || listener.working_set_changed());
		}
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	/// Brings the live socket path up for the current authenticated state, if it is not up
	/// already. Idempotent, and the production auth paths call it on their own — this export
	/// exists for explicit platform control and for tests (the in-memory constructor
	/// deliberately never auto-starts).
	pub fn start_live_updates(&self) {
		ensure_started(&self.state);
	}

	/// Tears the live path down: unsubscribes and stops applying. On a production (auth-file)
	/// state the next authenticated FFI call brings it back up on its own; an in-memory test
	/// state stays down until an explicit [`Self::start_live_updates`]. Calling this from a
	/// teardown (`invalidate()` on iOS) is safe: nothing is lost that the next launch does not
	/// rebuild.
	pub fn stop_live_updates(&self) {
		let state = self.sync_get_cache_state_borrowed();
		if let AuthStatus::Authenticated(auth) = &state.status {
			if let Some(handle) = lock(&auth.live.handle).take() {
				handle.drainer.abort();
			}
			auth.live.started.store(false, Ordering::Release);
		}
	}
	/// Registers who to tell when the live path has changed something — the replica's cue to
	/// ask for a diff (`signalEnumerator(.workingSet)` on iOS). `None` clears it.
	///
	/// Outlives auth changes, unlike the subscription itself: a listener set at startup keeps
	/// working after a re-auth without the caller having to notice.
	pub fn set_working_set_listener(&self, listener: Option<Arc<dyn WorkingSetUpdateListener>>) {
		let state = self.sync_get_cache_state_borrowed();
		*lock(&state.working_set_listener) = listener;
	}
}

#[cfg(test)]
mod applier_tests {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};
	use filen_sdk_rs::{
		fs::{dir::meta::DirectoryMeta, file::meta::FileMeta},
		socket as ev,
	};
	use filen_types::{api::v3::dir::color::DirColor, crypto::EncryptedString};

	use super::*;

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	const ROOT: u8 = 9;

	fn db() -> Mutex<Connection> {
		let conn = Connection::open_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(crate::sql::statements::INIT).unwrap();
		conn.execute(
			"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
			[uuid(ROOT)],
		)
		.unwrap();
		Mutex::new(conn)
	}

	fn file_record(uuid_: u8, stable_: u8, parent: u8) -> RemoteFile {
		RemoteFile {
			uuid: uuid(uuid_),
			stable_uuid: stable(stable_),
			meta: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
			parent: ParentUuid::Uuid(uuid(parent)),
			size: 3,
			favorited: false,
			region: "de-1".into(),
			bucket: "b".into(),
			timestamp: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap(),
			chunks: 1,
		}
	}

	fn dir_record(uuid_: u8, parent: u8) -> RemoteDirectory {
		RemoteDirectory {
			uuid: uuid(uuid_),
			parent: ParentUuid::Uuid(uuid(parent)),
			color: DirColor::Default,
			favorited: false,
			timestamp: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap(),
			meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-dir"))),
		}
	}

	fn apply(conn: &Mutex<Connection>, event: DecryptedDriveEvent<'static>) -> LocalApply {
		apply_drive_event_local(conn, event).unwrap()
	}

	fn row(conn: &Mutex<Connection>, uuid_: u8) -> Option<(Option<Uuid>, bool)> {
		super::conn(conn)
			.query_row(
				"SELECT parent, trashed FROM items WHERE uuid = ?1;",
				[uuid(uuid_)],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.optional()
			.unwrap()
	}

	fn item_count(conn: &Mutex<Connection>) -> i64 {
		super::conn(conn)
			.query_row("SELECT COUNT(*) FROM items WHERE type != 0;", [], |r| {
				r.get(0)
			})
			.unwrap()
	}

	/// The relevance gate, both ways: an event for a container this cache has never listed is a
	/// no-op drop; once the container is held, the same event applies. This is what keeps the
	/// live path O(1) per event and the cache from growing on its own.
	#[test]
	fn the_gate_drops_what_we_do_not_hold_and_applies_what_we_do() {
		let conn = db();

		// A file under a dir nobody here has listed: dropped.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
			),
			LocalApply::Applied(false)
		);
		assert_eq!(item_count(&conn), 0);

		// The dir arrives under the root, which IS held — so it lands...
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT)))
			),
			LocalApply::Applied(true)
		);
		// ...and the same file event is now relevant through its parent.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
			),
			LocalApply::Applied(true)
		);
		assert_eq!(item_count(&conn), 2);
	}

	/// A remote edit re-mints the file's uuid; the stable tier must re-file the row IN PLACE —
	/// one row, new uuid — and the archive of the superseded version (which names a successor)
	/// must change nothing.
	#[test]
	fn a_remote_edit_refiles_the_row_in_place() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		// The edit, as the socket delivers it: fileNew with a fresh uuid and the same stable id,
		// plus the fileArchived retiring the old version.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileNew(ev::FileNew(file_record(11, 2, 8)))
			),
			LocalApply::Applied(true)
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileArchived(ev::FileArchived {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: Some(uuid(11)),
				})
			),
			LocalApply::Applied(false),
			"an archive naming a successor is the edit's own bookkeeping, not a removal"
		);

		assert_eq!(
			item_count(&conn),
			2,
			"the same row, re-filed — never a second one"
		);
		assert!(row(&conn, 1).is_none());
		assert!(row(&conn, 11).is_some());
	}

	/// A remote trash keeps the row — marked trashed, original parent preserved for the restore —
	/// and the retirement half of a versioning-disabled edit (which names a successor) must not
	/// flicker the live row through the trash.
	#[test]
	fn a_remote_trash_marks_the_row_and_a_supersede_does_not() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		// The supersede: never a trash.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileTrash(ev::FileTrash {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: Some(uuid(11)),
				})
			),
			LocalApply::Applied(false)
		);
		assert_eq!(row(&conn, 1), Some((Some(uuid(8)), false)));

		// The real trash: marked, parent kept.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileTrash(ev::FileTrash {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: None,
				})
			),
			LocalApply::Applied(true)
		);
		assert_eq!(row(&conn, 1), Some((Some(uuid(8)), true)));

		// The replay another device's relist could produce: already where it belongs.
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileTrash(ev::FileTrash {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: None,
				})
			),
			LocalApply::Applied(false)
		);
	}

	/// A rename arriving AFTER an edit re-minted the uuid still lands: the patch is keyed by the
	/// whole-life id, which is the only name that survives the edit.
	#[test]
	fn a_rename_lands_across_a_uuid_remint() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(11, 2, 8))),
		);

		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileMetadataChanged(ev::FileMetadataChanged {
					// The event may still carry the OLD uuid; the stable id is what resolves.
					uuid: uuid(1),
					stable_uuid: stable(2),
					metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("renamed-enc"))),
				})
			),
			LocalApply::Applied(true)
		);
		let raw: String = super::conn(&conn)
			.query_row(
				"SELECT files.raw_metadata FROM files
				JOIN items ON items.id = files.id WHERE items.uuid = ?1;",
				[uuid(11)],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(raw, "renamed-enc");
	}

	/// The destructive arms answer with the follow-up they need rather than deleting here: the
	/// forget path owns the pending-edit guard, and the trash relist owns phantom reconciliation.
	/// The folds that mean "nothing died" must stay local no-ops.
	#[test]
	fn destructive_arms_defer_to_the_guarded_paths() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileDeletedPermanent(ev::FileDeletedPermanent {
					uuid: uuid(1),
					stable_uuid: None,
				})
			),
			LocalApply::Applied(false),
			"a versions-only purge must not touch the live head"
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileDeletedPermanent(ev::FileDeletedPermanent {
					uuid: uuid(1),
					stable_uuid: Some(stable(2)),
				})
			),
			LocalApply::ForgetFile(stable(2))
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileArchived(ev::FileArchived {
					uuid: uuid(1),
					stable_uuid: stable(2),
					new_uuid: None,
				})
			),
			LocalApply::ForgetFile(stable(2)),
			"an archive with NO successor is the lineage's death"
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderDeletedPermanent(ev::FolderDeletedPermanent {
					uuid: uuid(8),
				})
			),
			LocalApply::ForgetDir(uuid(8))
		);

		// TrashEmpty: nothing trashed, nothing to do; something trashed, relist.
		assert_eq!(
			apply(&conn, DecryptedDriveEvent::TrashEmpty),
			LocalApply::Applied(false)
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileTrash(ev::FileTrash {
				uuid: uuid(1),
				stable_uuid: stable(2),
				new_uuid: None,
			}),
		);
		assert_eq!(
			apply(&conn, DecryptedDriveEvent::TrashEmpty),
			LocalApply::RelistTrash
		);
	}

	/// A move into a directory this cache has never listed still applies (the row is held), and
	/// the row ends up naming a dangling parent the provider resolves by asking about it — the
	/// documented one-level-per-ask behaviour.
	#[test]
	fn a_move_into_an_unheld_parent_applies_with_a_dangling_parent() {
		let conn = db();
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
		);

		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FileMove(ev::FileMove(file_record(1, 2, 77)))
			),
			LocalApply::Applied(true)
		);
		assert_eq!(row(&conn, 1), Some((Some(uuid(77)), false)));
		assert!(
			row(&conn, 77).is_none(),
			"the destination itself stays unknown"
		);
	}

	/// Folder metadata and color changes patch rows we hold and drop for rows we do not.
	#[test]
	fn folder_patches_respect_the_gate() {
		let conn = db();
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderMetadataChanged(ev::FolderMetadataChanged {
					uuid: uuid(8),
					meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("new-enc"))),
				})
			),
			LocalApply::Applied(false)
		);
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
		);
		assert_eq!(
			apply(
				&conn,
				DecryptedDriveEvent::FolderMetadataChanged(ev::FolderMetadataChanged {
					uuid: uuid(8),
					meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("new-enc"))),
				})
			),
			LocalApply::Applied(true)
		);
		let raw: String = super::conn(&conn)
			.query_row(
				"SELECT dirs.raw_metadata FROM dirs
				JOIN items ON items.id = dirs.id WHERE items.uuid = ?1;",
				[uuid(8)],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(raw, "new-enc");
	}
}

#[cfg(test)]
mod gap_gate_tests {
	use super::pass_owed;

	/// The regression the durable debt exists for: a pass that failed leaves the watermark
	/// behind, the next events carry it past the remote counter this cache ever compared
	/// against, and from then on the counters alone say "caught up" forever. The debt must
	/// outrank them — and, once a pass converges and clears it, stop forcing passes.
	#[test]
	fn an_owed_pass_runs_even_when_the_watermark_says_caught_up() {
		assert!(
			pass_owed(true, Some(50), 50),
			"a pass owed by a failed container (or apply) must run past a caught-up watermark"
		);
		assert!(
			pass_owed(true, Some(9000), 50),
			"a watermark advanced by later events must not swallow the debt"
		);
		assert!(
			!pass_owed(false, Some(50), 50),
			"nothing owed and nothing missed: no pass"
		);
		assert!(
			pass_owed(false, Some(49), 50),
			"a moved remote counter still runs a pass on its own"
		);
		assert!(
			pass_owed(false, None, 0),
			"a fresh cache has missed everything by definition"
		);
	}
}
