//! What the pipeline does with real camera RAW files, TODAY.
//!
//! This is a characterisation suite, not a feature suite. RAW preview
//! extraction has NOT been implemented; the extension gate upstream merely
//! lets RAW files reach `generate`. These tests record the current outcome for
//! every pinned sample so that implementing RAW support changes a written-down
//! baseline deliberately instead of silently.
//!
//! Nothing here asserts that RAW works — it does not. As of this commit, over
//! 100 CC0 samples spanning ten formats and ten vendors:
//!
//! * **No sample yields an embedded preview.** Not one, in any format. Real
//!   RAW previews live in SubIFDs (tag 0x014A) or in maker-note offsets, and
//!   `exif::embedded_thumbnail` walks only IFD0 -> IFD1, so it never reaches
//!   them.
//! * **CR3, ORF, RAF and RW2 are not recognised at all** — their magic is not
//!   TIFF's (`ftypcrx`, `IIRO`/`MMOR`, `FUJIFILM`, `II\x55\x00`), no decoder
//!   claims them, and `generate` answers `Ok(None)` after reading 64 bytes.
//! * **CR2, ARW, PEF and SRW are claimed by the TIFF decoder and then fail** —
//!   IFD0 of those files describes the sensor mosaic, not a displayable image,
//!   so `open` refuses the colour type or the strip table.
//! * **NEF and DNG sometimes "succeed", and that is the worst case**: the TIFF
//!   path decodes IFD0, which on a NEF is a 160x120 postage stamp beside a
//!   12 MP photograph, and hands it back for a 512x512 request. A caller
//!   cannot tell that apart from a real thumbnail. Nine of ten NEFs and four
//!   of ten DNGs do this; the largest result across all 100 samples is
//!   320x218. See [`decoded_raw_never_reaches_the_requested_size`].
//!
//! Ignored by default: the pinned set is ~1 GiB. Run it with
//! `cargo test -p microthumb --test raw_characterisation -- --ignored --nocapture`.

mod raw_fixtures;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use microthumb::{ByteSource, DEFAULT_MEM_BUDGET, FileSource, ThumbSource, ThumbSpec, generate};
use raw_fixtures::{RAW_FIXTURES, RawFixture};

/// The request every case is characterised against.
const TARGET: u32 = 512;

/// Formats whose magic no decoder in the registry claims.
const UNRECOGNISED: &[&str] = &["CR3", "ORF", "RAF", "RW2"];
/// Formats the TIFF decoder claims and then refuses.
const CLAIMED_THEN_REFUSED: &[&str] = &["CR2", "ARW", "PEF", "SRW"];
/// Formats where IFD0 happens to hold a decodable — but tiny and wrong —
/// image, so some samples return a thumbnail that is not of the photograph.
const DECODES_WRONG_SUB_IMAGE: &[&str] = &["NEF", "DNG"];

/// The largest edge any RAW sample currently produces. Well under `TARGET`,
/// which is the tell that these are sub-images rather than downscales of the
/// real frame.
const LARGEST_OBSERVED_EDGE: u32 = 320;

/// How far into a file the pipeline reaches before giving up on input it
/// cannot use. Observed maximum is 21% (a Blackmagic cine DNG).
const PREFIX_BUDGET_PCT: u64 = 25;

/// Tracks how far into the file the pipeline actually reached. For RAW the
/// interesting question is not only "did it produce anything" but "did it have
/// to pull the whole 25 MB to find out".
struct Counting(FileSource, Arc<AtomicU64>);

impl ByteSource for Counting {
	fn len(&self) -> u64 {
		self.0.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.0.read_at(offset, buf)?;
		self.1.fetch_max(offset + n as u64, Ordering::Relaxed);
		Ok(n)
	}
}

#[derive(PartialEq, Eq, Clone, Debug)]
enum Outcome {
	/// A thumbnail, its dimensions, and which path produced it.
	Thumb(u32, u32, ThumbSource),
	Nothing,
	Failed(String),
}

impl Outcome {
	/// Coarse bucket, for counting. Deliberately drops the error text so that
	/// twenty different decode messages still summarise as one `Err` column.
	fn label(&self) -> &'static str {
		match self {
			Outcome::Thumb(_, _, ThumbSource::EmbeddedPreview) => "embedded preview",
			Outcome::Thumb(_, _, ThumbSource::Decoded) => "full decode",
			Outcome::Nothing => "Ok(None)",
			Outcome::Failed(_) => "Err",
		}
	}

	fn dims(&self) -> Option<(u32, u32)> {
		match self {
			Outcome::Thumb(w, h, _) => Some((*w, *h)),
			_ => None,
		}
	}
}

/// One characterised sample.
struct Record {
	fixture: &'static RawFixture,
	/// `ThumbSpec::new` — embedded preview first, then a bounded full decode.
	full: Outcome,
	/// `ThumbSpec::preview_only` — the embedded-preview path alone. This is the
	/// one that RAW support would have to change.
	preview_only: Outcome,
	/// High-water byte offset reached, as a percentage of file length.
	read_pct: u64,
}

fn run(path: &Path, spec: &ThumbSpec) -> (Outcome, u64) {
	let read = Arc::new(AtomicU64::new(0));
	let file = std::fs::File::open(path).expect("fixture opens");
	let src = Counting(FileSource::new(file).expect("file source"), read.clone());
	let outcome = match generate(Box::new(src), spec) {
		Ok(Some(t)) => Outcome::Thumb(t.image.width, t.image.height, t.source),
		Ok(None) => Outcome::Nothing,
		Err(e) => Outcome::Failed(e.to_string()),
	};
	(outcome, read.load(Ordering::Relaxed))
}

/// Characterises every obtainable fixture once, printing the table. Returns the
/// records; the individual `#[test]`s below assert over them.
fn characterise() -> Vec<Record> {
	let full_spec = ThumbSpec::new(TARGET, TARGET, DEFAULT_MEM_BUDGET);
	let preview_spec = ThumbSpec::preview_only(TARGET, TARGET, DEFAULT_MEM_BUDGET);

	let mut records = Vec::new();
	let skipped = raw_fixtures::for_each_available(RAW_FIXTURES, |fixture, path| {
		let (full, read) = run(path, &full_spec);
		let (preview_only, _) = run(path, &preview_spec);
		records.push(Record {
			fixture,
			full,
			preview_only,
			read_pct: read * 100 / fixture.len.max(1),
		});
	});

	report(&records, skipped);
	records
}

/// Runs `check` over the characterised samples.
///
/// When not one sample could be obtained the suite skips rather than fails:
/// a machine with no network and a cold cache must be able to run this. The
/// per-fixture SKIP lines above name what was missing and where it lives.
fn with_records(check: impl FnOnce(&[Record])) {
	let records = characterise();
	if records.is_empty() {
		eprintln!(
			"SKIP raw characterisation: not one pinned sample could be obtained. \
			 Nothing was checked."
		);
		return;
	}
	check(&records);
}

fn report(records: &[Record], skipped: usize) {
	for r in records {
		eprintln!(
			"{:<4} {:<12} {:<32} {:>8} {:>3}% read  full={:<17} preview-only={}",
			r.fixture.format,
			r.fixture.container,
			format!("{} {}", r.fixture.make, r.fixture.model),
			r.full
				.dims()
				.map_or_else(|| "-".into(), |(w, h)| format!("{w}x{h}")),
			r.read_pct,
			r.full.label(),
			r.preview_only.label(),
		);
	}

	eprintln!("\n--- per-format behaviour TODAY (characterisation, not a contract) ---");
	let mut formats: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
	for r in records {
		formats.entry(r.fixture.format).or_default().push(r);
	}
	for (format, rs) in &formats {
		let mut buckets: BTreeMap<&str, usize> = BTreeMap::new();
		for r in rs {
			*buckets.entry(r.full.label()).or_default() += 1;
		}
		let dims: BTreeSet<String> = rs
			.iter()
			.filter_map(|r| r.full.dims())
			.map(|(w, h)| format!("{w}x{h}"))
			.collect();
		eprintln!(
			"{format:<4} n={:<3} {:<34} previews={:<3} sizes={}",
			rs.len(),
			buckets
				.iter()
				.map(|(l, n)| format!("{n}x {l}"))
				.collect::<Vec<_>>()
				.join(", "),
			rs.iter()
				.filter(|r| matches!(r.preview_only, Outcome::Thumb(..)))
				.count(),
			if dims.is_empty() {
				"-".into()
			} else {
				dims.into_iter().collect::<Vec<_>>().join(",")
			},
		);
	}

	let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
	for r in records {
		if let Outcome::Failed(msg) = &r.full {
			*reasons.entry(msg.as_str()).or_default() += 1;
		}
	}
	if !reasons.is_empty() {
		eprintln!("\n--- distinct failure reasons ---");
		for (msg, n) in &reasons {
			eprintln!("{n:>3}x {msg}");
		}
	}
	if skipped > 0 {
		eprintln!(
			"\n{skipped} of {} fixtures unavailable (see SKIP lines above)",
			RAW_FIXTURES.len()
		);
	}
}

/// Every pinned row must be CC0. The sample library also carries CC BY-NC-SA
/// files; this is the check that keeps one from being pasted in later.
#[test]
fn all_fixtures_are_cc0() {
	for f in RAW_FIXTURES {
		assert!(
			f.cc0,
			"{} is pinned without a confirmed CC0 licence",
			f.cache_name
		);
	}
}

/// The pins are internally consistent. Needs no fixture bytes, so it runs in
/// the normal test pass.
#[test]
fn pins_are_well_formed() {
	let mut names = BTreeSet::new();
	let mut hashes = BTreeSet::new();
	for f in RAW_FIXTURES {
		assert!(f.len > 0, "{} pinned with zero length", f.cache_name);
		assert_eq!(
			f.blake3.len(),
			64,
			"{} has a malformed BLAKE3 pin",
			f.cache_name
		);
		assert!(
			f.blake3.bytes().all(|b| b.is_ascii_hexdigit()),
			"{} has a non-hex BLAKE3 pin",
			f.cache_name
		);
		assert!(
			f.url.starts_with("https://raw.pixls.us/getfile.php/"),
			"{} is pinned to an unexpected host: {}",
			f.cache_name,
			f.url
		);
		assert!(
			names.insert(f.cache_name),
			"duplicate cache name {}",
			f.cache_name
		);
		assert!(
			hashes.insert(f.blake3),
			"duplicate pin {} on {}",
			f.blake3,
			f.cache_name
		);
	}
}

/// THE finding, and the one a RAW implementation is meant to flip: across
/// every pinned sample, the embedded-preview path comes back empty. RAW
/// previews sit in SubIFDs; `exif::embedded_thumbnail` walks IFD0 -> IFD1 only.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn no_raw_sample_yields_an_embedded_preview() {
	with_records(|records| {
		let found: Vec<_> = records
			.iter()
			.filter(|r| {
				matches!(r.preview_only, Outcome::Thumb(..))
					|| matches!(r.full, Outcome::Thumb(_, _, ThumbSource::EmbeddedPreview))
			})
			.map(|r| r.fixture.cache_name)
			.collect();
		assert!(
			found.is_empty(),
			"BASELINE CHANGED — these samples now produce an embedded preview: {found:?}. \
			 If RAW preview extraction was implemented, this test is the one to rewrite."
		);
	});
}

/// The dangerous case. When a RAW file does produce a thumbnail today, it is
/// not a thumbnail of the photograph: the TIFF path decodes IFD0, which on
/// these files holds a small embedded preview image. A NEF returning 160x120
/// for a 12 MP frame is the unambiguous case; the general, checkable symptom
/// is that no decode gets anywhere near the size that was asked for.
///
/// A caller has no way to distinguish this from a real thumbnail, which is why
/// it is worth pinning: RAW support must change this test.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn decoded_raw_never_reaches_the_requested_size() {
	with_records(|records| {
		let mut decoded = 0;
		for r in records.iter() {
			let Some((w, h)) = r.full.dims() else {
				continue;
			};
			decoded += 1;
			assert!(
				DECODES_WRONG_SUB_IMAGE.contains(&r.fixture.format),
				"{} ({}) produced a thumbnail, which no {} sample did at baseline",
				r.fixture.cache_name,
				r.fixture.format,
				r.fixture.format
			);
			assert!(
				w.max(h) <= LARGEST_OBSERVED_EDGE,
				"{} decoded to {w}x{h}; at baseline no RAW sample exceeded {LARGEST_OBSERVED_EDGE}px, \
				 so this may now be decoding the real frame rather than IFD0",
				r.fixture.cache_name
			);
		}
		assert!(
			decoded > 0,
			"no sample decoded at all; the TIFF path's reach into RAW changed"
		);
	});
}

/// Per-format outcome buckets. Unsupported input must stay *cleanly*
/// unsupported: nothing recognised gets silently half-handled.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn per_format_outcomes_match_baseline() {
	with_records(|records| {
		for r in records.iter() {
			let format = r.fixture.format;
			if UNRECOGNISED.contains(&format) {
				assert_eq!(
					r.full,
					Outcome::Nothing,
					"{} ({format}) is unrecognised at baseline but gave {:?}",
					r.fixture.cache_name,
					r.full
				);
				assert_eq!(
					r.read_pct, 0,
					"{} ({format}) is rejected on the 64-byte sniff at baseline, yet read {}% of the file",
					r.fixture.cache_name, r.read_pct
				);
			} else if CLAIMED_THEN_REFUSED.contains(&format) {
				assert!(
					matches!(r.full, Outcome::Failed(_)),
					"{} ({format}) fails in the TIFF decoder at baseline but gave {:?}",
					r.fixture.cache_name,
					r.full
				);
			} else {
				assert!(
					DECODES_WRONG_SUB_IMAGE.contains(&format),
					"{format} has no recorded baseline; add it to one of the three lists"
				);
				assert!(
					!matches!(r.full, Outcome::Nothing),
					"{} ({format}) is claimed by the TIFF decoder at baseline, so it either \
					 decodes IFD0 or errors — it does not fall through to Ok(None)",
					r.fixture.cache_name
				);
			}
		}
	});
}

/// The memory contract's other half: bytes pulled. A RAW file is tens of
/// megabytes and may be remote-backed, so discovering that we cannot use one
/// must not cost a full fetch.
///
/// Only the give-up paths are bounded. A sample that actually decodes may
/// legitimately read everything — one 1:1 DNG here decodes a small frame and
/// reads 99% — and bounding that would be asserting the wrong thing.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn raw_we_cannot_use_costs_only_a_prefix() {
	with_records(|records| {
		let mut checked = 0;
		for r in records.iter() {
			if r.full.dims().is_some() {
				continue;
			}
			checked += 1;
			assert!(
				r.read_pct <= PREFIX_BUDGET_PCT,
				"{} ({}) read {}% of a {}-byte file before giving up",
				r.fixture.cache_name,
				r.fixture.format,
				r.read_pct,
				r.fixture.len
			);
		}
		assert!(
			checked > 0,
			"every sample decoded; the give-up paths went uncharacterised"
		);
	});
}
