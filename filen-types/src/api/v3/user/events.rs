use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{
	Deserialize, Deserializer, Serialize,
	de::{SeqAccess, Visitor},
};
use serde_json::value::RawValue;

use crate::{
	api::v3::dir::color::DirColor,
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ObjectType, Uuid},
	traits::CowHelpers,
};

pub const ENDPOINT: &str = "v3/user/events";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub filter: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

/// Per-event deserialization failure. Stored alongside successfully-parsed
/// events in the response so that one malformed/unknown variant doesn't fail
/// the whole list.
#[derive(Debug, Clone)]
pub struct UserEventDeserializeError {
	pub message: String,
	pub raw: String,
}

impl std::fmt::Display for UserEventDeserializeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.message)
	}
}

impl std::error::Error for UserEventDeserializeError {}

/// Response for `POST /v3/user/events`. Each event is presented as a
/// `Result` so individual malformed/unknown events can be inspected (or
/// skipped) without losing the rest.
#[derive(Deserialize, Debug)]
pub struct Response<'a> {
	#[serde(deserialize_with = "deserialize_events")]
	pub events: Vec<Result<UserEvent<'a>, UserEventDeserializeError>>,
}

/// Borrows each element as a `&RawValue` (zero heap allocation per event),
/// then re-parses it as `UserEvent<'static>`; failures are captured as `Err`.
fn deserialize_events<'de, D>(
	deserializer: D,
) -> Result<Vec<Result<UserEvent<'static>, UserEventDeserializeError>>, D::Error>
where
	D: Deserializer<'de>,
{
	struct EventsVisitor;

	impl<'de> Visitor<'de> for EventsVisitor {
		type Value = Vec<Result<UserEvent<'static>, UserEventDeserializeError>>;

		fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
			f.write_str("a sequence of user events")
		}

		fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
			let mut results = Vec::with_capacity(seq.size_hint().unwrap_or(0));
			while let Some(raw) = seq.next_element::<&'de RawValue>()? {
				match serde_json::from_str::<UserEvent<'static>>(raw.get()) {
					Ok(event) => results.push(Ok(event)),
					Err(e) => results.push(Err(UserEventDeserializeError {
						message: e.to_string(),
						raw: raw.get().to_string(),
					})),
				}
			}
			Ok(results)
		}
	}

	deserializer.deserialize_seq(EventsVisitor)
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct UserEvent<'a> {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub uuid: Uuid,
	#[serde(flatten)]
	pub kind: UserEventKind<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(tag = "type", content = "info", rename_all = "camelCase")]
pub enum UserEventKind<'a> {
	FileUploaded(FileMetadataInfo<'a>),
	FileVersioned(FileMetadataInfo<'a>),
	FileRestored(FileMetadataInfo<'a>),
	VersionedFileRestored(FileMetadataInfo<'a>),
	FileMoved(FileMetadataInfo<'a>),
	FileRenamed(FileMetadataPairInfo<'a>),
	FileMetadataChanged(FileMetadataPairInfo<'a>),
	FileTrash(FileMetadataInfo<'a>),
	FileRm(FileMetadataInfo<'a>),
	FileShared(FileSharedInfo<'a>),
	FileLinkEdited(FileLinkEditedInfo<'a>),
	DeleteFilePermanently(FileMetadataInfo<'a>),

	FolderTrash(FolderNameInfo<'a>),
	FolderShared(FolderSharedInfo<'a>),
	FolderMoved(FolderNameInfo<'a>),
	FolderRenamed(FolderNamePairInfo<'a>),
	FolderMetadataChanged(FolderNamePairInfo<'a>),
	SubFolderCreated(FolderNameInfo<'a>),
	BaseFolderCreated(FolderNameInfo<'a>),
	FolderRestored(FolderNameInfo<'a>),
	FolderColorChanged(FolderColorChangedInfo<'a>),
	DeleteFolderPermanently(FolderNameInfo<'a>),

	Login(BaseInfo<'a>),
	FailedLogin(BaseInfo<'a>),
	PasswordChanged(BaseInfo<'a>),
	#[serde(rename = "2faEnabled")]
	TwoFaEnabled(BaseInfo<'a>),
	#[serde(rename = "2faDisabled")]
	TwoFaDisabled(BaseInfo<'a>),
	RequestAccountDeletion(BaseInfo<'a>),
	TrashEmptied(BaseInfo<'a>),
	DeleteAll(BaseInfo<'a>),
	DeleteVersioned(BaseInfo<'a>),
	DeleteUnfinished(BaseInfo<'a>),

	CodeRedeemed(CodeRedeemedInfo<'a>),
	EmailChanged(EmailChangedInfo<'a>),
	EmailChangeAttempt(EmailChangeAttemptInfo<'a>),
	RemovedSharedInItems(RemovedSharedInItemsInfo<'a>),
	RemovedSharedOutItems(RemovedSharedOutItemsInfo<'a>),
	FolderLinkEdited(FolderLinkEditedInfo<'a>),
	ItemFavorite(ItemFavoriteInfo<'a>),
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct BaseInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
}

/// Shared by the single-metadata file events. Which of the optional fields are
/// actually populated varies per kind; the captured fixtures in
/// `tests/fixtures/user_events/` are the reference:
///
/// - `fileUploaded`: everything except `favorited` and `current_uuid`
/// - `fileMoved` / `fileRestored`: everything except `current_uuid`
/// - `versionedFileRestored`: everything, `current_uuid` being the uuid that
///   was current before the old version was restored over it
/// - `fileVersioned` / `fileTrash`: only `uuid` (of the superseded / trashed
///   file — there is no `newUUID`-style field pointing at a successor)
/// - `deleteFilePermanently`: none of them, not even `uuid`
/// - `fileRm`: unverified (never observed live)
#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadataInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	#[serde(default)]
	pub parent: Option<Uuid>,
	#[serde(default)]
	pub bucket: Option<Cow<'a, str>>,
	#[serde(default)]
	pub region: Option<Cow<'a, str>>,
	#[serde(default)]
	pub rm: Option<Cow<'a, str>>,
	#[serde(default, with = "crate::serde::number::permissive_u64_opt")]
	pub chunks: Option<u64>,
	#[serde(default)]
	pub version: Option<FileEncryptionVersion>,
	#[serde(default, with = "crate::serde::boolean::maybe_number")]
	pub favorited: Option<bool>,
	/// The file's own (creation/upload) timestamp; the event time is the
	/// outer [`UserEvent::timestamp`].
	#[serde(default, with = "crate::serde::time::optional")]
	pub timestamp: Option<DateTime<Utc>>,
	/// `versionedFileRestored` only: the uuid that was current before the
	/// restore replaced it with [`Self::uuid`].
	#[serde(default, rename = "currentUUID")]
	pub current_uuid: Option<Uuid>,
}

/// `fileRenamed` and `fileMetadataChanged` (observed identical): both carry
/// the file uuid and the standalone encrypted-name blob next to the old/new
/// full metadata.
#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadataPairInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
	pub old_metadata: EncryptedString<'a>,
	/// The new name, encrypted with the file key (the blob stored server-side
	/// for public-link consumers), unlike `metadata` which uses the master key.
	#[serde(default)]
	pub name: Option<EncryptedString<'a>>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileSharedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
	pub receiver_email: Cow<'a, str>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	/// Observed as an explicit `null` when the item was shared from the root.
	#[serde(default)]
	pub parent: Option<Uuid>,
}

/// `fileLinkEdited`: emitted for both enabling and disabling a public file
/// link (the payloads are identical, so the two cannot be told apart).
#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FileLinkEditedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	#[serde(default, rename = "linkUUID")]
	pub link_uuid: Option<Uuid>,
}

/// Shared by the single-name folder events. Per-kind population (see the
/// fixtures):
///
/// - `subFolderCreated` / `folderMoved` / `folderRestored`: `uuid`, `parent`
///   (the *new* parent for a move) and `timestamp`
/// - `folderTrash`: `uuid` and `parent`
/// - `deleteFolderPermanently`: none of the optionals
/// - `baseFolderCreated`: unverified (never observed live)
#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderNameInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	#[serde(default)]
	pub parent: Option<Uuid>,
	/// The folder's own creation timestamp; the event time is the outer
	/// [`UserEvent::timestamp`].
	#[serde(default, with = "crate::serde::time::optional")]
	pub timestamp: Option<DateTime<Utc>>,
}

/// `folderRenamed` and `folderMetadataChanged` (observed identical).
#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderNamePairInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
	pub old_name: EncryptedString<'a>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderColorChangedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	#[serde(default)]
	pub color: Option<DirColor<'a>>,
	/// Observed as an explicit `null` when the folder still had the default
	/// colour.
	#[serde(default)]
	pub old_color: Option<DirColor<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderSharedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub name: EncryptedString<'a>,
	pub receiver_email: Cow<'a, str>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	/// Observed as an explicit `null` when the item was shared from the root.
	#[serde(default)]
	pub parent: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct CodeRedeemedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub code: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct EmailChangedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct EmailChangeAttemptInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	pub email: Cow<'a, str>,
	pub new_email: Cow<'a, str>,
	pub old_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct RemovedSharedInItemsInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub count: u64,
	pub sharer_email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct RemovedSharedOutItemsInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub count: u64,
	pub receiver_email: Cow<'a, str>,
}

/// `folderLinkEdited`: emitted for both enabling and disabling a public
/// folder link (identical payloads). Unlike `fileLinkEdited` it carries no
/// metadata/name blob at all — only the two uuids.
#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct FolderLinkEditedInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(rename = "linkUUID")]
	pub link_uuid: Uuid,
	/// The linked folder's uuid.
	#[serde(default)]
	pub uuid: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Debug, Clone, CowHelpers)]
#[serde(rename_all = "camelCase")]
pub struct ItemFavoriteInfo<'a> {
	pub ip: Cow<'a, str>,
	pub user_agent: Cow<'a, str>,
	#[serde(with = "crate::serde::boolean::number")]
	pub value: bool,
	/// Encrypted file metadata for files, encrypted `{"name": …}` for folders
	/// — discriminated by [`Self::item_type`].
	pub metadata: EncryptedString<'a>,
	#[serde(default)]
	pub uuid: Option<Uuid>,
	#[serde(default, rename = "type")]
	pub item_type: Option<ObjectType>,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn login_event_json(id: u64) -> String {
		format!(
			r#"{{"id":{id},"timestamp":1700000,"uuid":"11111111-1111-1111-1111-111111111111","type":"login","info":{{"ip":"1.2.3.4","userAgent":"ua"}}}}"#
		)
	}

	#[test]
	fn known_event_deserializes_to_ok() {
		let json = format!(r#"{{"events":[{}]}}"#, login_event_json(1));
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 1);
		let event = resp.events[0].as_ref().expect("login should parse");
		assert_eq!(event.id, 1);
		assert!(matches!(event.kind, UserEventKind::Login(_)));
	}

	#[test]
	fn unknown_event_type_is_captured_as_err() {
		let raw = r#"{"id":2,"timestamp":1700000,"uuid":"22222222-2222-2222-2222-222222222222","type":"futureVariantWeDontKnowAbout","info":{"ip":"1.2.3.4","userAgent":"ua"}}"#;
		let json = format!(r#"{{"events":[{raw}]}}"#);
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 1);
		let err = resp
			.events
			.into_iter()
			.next()
			.unwrap()
			.expect_err("unknown variant should land in Err");
		// Raw JSON of the failing event must be preserved verbatim for diagnostics.
		assert_eq!(err.raw, raw);
		assert!(!err.message.is_empty(), "error message should be populated");
	}

	#[test]
	fn malformed_event_does_not_poison_neighbours() {
		// First and third events are valid logins; second is malformed (missing
		// `info` for a known variant). The list must still contain three
		// entries, with Ok/Err/Ok in order.
		let malformed = r#"{"id":99,"timestamp":1700000,"uuid":"00000000-0000-0000-0000-000000000000","type":"login"}"#;
		let json = format!(
			r#"{{"events":[{},{malformed},{}]}}"#,
			login_event_json(1),
			login_event_json(3)
		);
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 3);
		assert!(resp.events[0].is_ok(), "first event should parse");
		let err = resp.events[1]
			.as_ref()
			.expect_err("middle event should be Err");
		assert_eq!(err.raw, malformed);
		assert!(resp.events[2].is_ok(), "last event should parse");
	}

	#[test]
	fn empty_events_array_yields_empty_vec() {
		let json = r#"{"events":[]}"#;
		let resp: Response = serde_json::from_str(json).unwrap();
		assert!(resp.events.is_empty());
	}

	#[test]
	fn unknown_top_level_fields_are_ignored() {
		// Forward-compat: if the server adds e.g. a `cursor` field at the top
		// level, our deserializer must not fail.
		let json = format!(
			r#"{{"cursor":"abc","events":[{}],"extra":42}}"#,
			login_event_json(1)
		);
		let resp: Response = serde_json::from_str(&json).unwrap();
		assert_eq!(resp.events.len(), 1);
	}
}
