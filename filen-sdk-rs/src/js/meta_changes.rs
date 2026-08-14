use chrono::{DateTime, Utc};
use filen_macros::js_type;

// FFI-facing twins of the `fs::{dir,file}::meta` *MetaChanges builders.
// tsify's from_wasm_abi and uniffi's record lifting cannot surface an error to
// the caller, so a failed ValidatedName parse at the boundary aborts instead
// of rejecting. These twins carry the name as a plain String and are converted
// fallibly (TryFrom, in the meta modules) inside the exported functions,
// turning an invalid name into a normal Error.

// One shared uniffi enum for both twins (a second enum of the same name would
// collide in the flattened uniffi namespace).
#[cfg(feature = "uniffi")]
#[derive(Debug, PartialEq, Eq, Clone, Default, serde::Deserialize, uniffi::Enum)]
pub enum CreatedTime {
	#[default]
	Keep,
	Unset,
	Set(DateTime<Utc>),
}

#[derive(Default)]
#[js_type(import)]
pub struct DirectoryMetaChanges {
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub name: Option<String>,
	// double option because we need to distinguish between "not set" and "set to
	// None". uniffi collapses nested nullability (T?? == T?), so it uses the
	// CreatedTime enum instead — matching the FileMetaChanges twin.
	#[cfg(not(feature = "uniffi"))]
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint | null"),
		serde(
			default,
			deserialize_with = "crate::serde::deserialize_double_option_timestamp"
		)
	)]
	pub created: Option<Option<DateTime<Utc>>>,
	#[cfg(feature = "uniffi")]
	pub created: CreatedTime,
}

#[derive(Default)]
#[js_type(import)]
pub struct FileMetaChanges {
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub name: Option<String>,
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub mime: Option<String>,
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint"),
		serde(default, with = "filen_types::serde::time::optional")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub last_modified: Option<DateTime<Utc>>,
	#[cfg(not(feature = "uniffi"))]
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint | null"),
		serde(
			default,
			deserialize_with = "crate::serde::deserialize_double_option_timestamp"
		)
	)]
	pub created: Option<Option<DateTime<Utc>>>,
	#[cfg(feature = "uniffi")]
	pub created: CreatedTime,
}
