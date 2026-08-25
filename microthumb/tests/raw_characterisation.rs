//! What the pipeline does with real camera RAW files.
//!
//! This is a characterisation suite: it pins the outcome for every sample so
//! that a change in RAW handling has to rewrite a written-down baseline
//! instead of moving silently. It runs over 100 CC0 samples spanning ten
//! formats and ten vendors.
//!
//! The pipeline does not decode sensor mosaics and never will — demosaicing is
//! not this crate's job, and the `tiff` crate's attempt at it was actively
//! harmful (on a NEF it decoded IFD0, a 160x120 postage stamp, and returned it
//! for a 512 px request as though it were the photograph). What it does
//! instead is find the JPEG the camera embedded next to the mosaic and decode
//! *that*, through the ordinary JPEG path and the ordinary budget check.
//!
//! Where that leaves each format:
//!
//! * **CR2, ARW, NEF, PEF, SRW, ORF, RW2 — all ten samples each** yield a
//!   preview, on both the full and the preview-only spec.
//! * **DNG splits five and five.** Five carry a real preview; the other five
//!   are cinema and CFA files whose only image data IS the mosaic
//!   (PhotometricInterpretation 32803), so there is nothing to find and they
//!   error rather than returning a demosaiced-looking lie.
//! * **RAF — all ten** yield a preview: Fujifilm's header points straight at
//!   one, no directory walk needed.
//! * **CR3 — all ten**, at 1620x1080, from its ISO-BMFF `PRVW` box.
//!
//! Sizes are honest about the source: a Nikon D1H stores nothing but a 160x120
//! uncompressed strip, so 160x120 is what comes back. 81 of the 100 samples
//! reach the full 512 px request.
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

/// Per-format baseline: how many of the ten samples end in a preview, in
/// `Ok(None)`, and in an error. Any movement here is a deliberate change to
/// RAW support, not a detail.
struct Baseline {
	format: &'static str,
	previews: usize,
	nothing: usize,
	errors: usize,
}

const BASELINE: &[Baseline] = &[
	// TIFF-family containers, all of which hide a real JPEG (or, on the
	// oldest Nikons, a small uncompressed RGB strip).
	Baseline {
		format: "CR2",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	Baseline {
		format: "ARW",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	Baseline {
		format: "NEF",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	Baseline {
		format: "PEF",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	Baseline {
		format: "SRW",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	// Olympus and Panasonic use their own container magic and hide the
	// preview in a maker note / private tag respectively.
	Baseline {
		format: "ORF",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	Baseline {
		format: "RW2",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	// Half the DNGs are cinema/CFA files carrying no displayable image.
	Baseline {
		format: "DNG",
		previews: 5,
		nothing: 0,
		errors: 5,
	},
	// Fujifilm's header points straight at its preview.
	Baseline {
		format: "RAF",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
	// Not a TIFF container at all: an ISO-BMFF box tree.
	Baseline {
		format: "CR3",
		previews: 10,
		nothing: 0,
		errors: 0,
	},
];

/// The smallest preview any sample yields — a Canon PowerShot DNG's 128x96
/// uncompressed thumbnail. Below this something has gone wrong.
const SMALLEST_PREVIEW_EDGE: u32 = 128;

/// Samples whose preview reaches the full requested size. The rest are
/// bounded by what the camera stored, not by anything this crate does.
const SAMPLES_MEETING_THE_REQUEST: usize = 81;

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
		Ok(microthumb::ThumbOutcome::Thumbnail(t)) => {
			Outcome::Thumb(t.image.width, t.image.height, t.source)
		}
		Ok(microthumb::ThumbOutcome::Unsupported | microthumb::ThumbOutcome::OverBudget) => {
			Outcome::Nothing
		}
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

/// THE property RAW support exists for: the embedded preview is found. It
/// must be found on the preview-only spec too, because a RAW file is tens of
/// megabytes and a remote-backed caller passes exactly that spec.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn the_embedded_preview_is_found_on_both_specs() {
	with_records(|records| {
		let mut found = 0;
		for r in records {
			let Outcome::Thumb(w, h, source) = r.full else {
				continue;
			};
			found += 1;
			assert_eq!(
				source,
				ThumbSource::EmbeddedPreview,
				"{} produced a {w}x{h} thumbnail by decoding the mosaic, which is \
				 never what this pipeline should do",
				r.fixture.cache_name
			);
			assert_eq!(
				r.preview_only.label(),
				r.full.label(),
				"{} ({}) yields {:?} to a local caller but {:?} to a remote one; the \
				 preview path must not depend on allow_full_decode",
				r.fixture.cache_name,
				r.fixture.format,
				r.full,
				r.preview_only,
			);
		}
		assert!(
			found >= records.len() * 3 / 4,
			"only {found} of {} samples yielded a preview",
			records.len()
		);
	});
}

/// Per-format outcome buckets. The counts are the baseline; a format moving
/// between columns is the whole point of this file.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn per_format_outcomes_match_baseline() {
	with_records(|records| {
		for expected in BASELINE {
			let rs: Vec<_> = records
				.iter()
				.filter(|r| r.fixture.format == expected.format)
				.collect();
			if rs.is_empty() {
				continue;
			}
			let count = |label: &str| rs.iter().filter(|r| r.full.label() == label).count();
			let (previews, nothing, errors) =
				(count("embedded preview"), count("Ok(None)"), count("Err"));
			assert_eq!(
				(previews, nothing, errors),
				(expected.previews, expected.nothing, expected.errors),
				"{} baseline is {} preview / {} none / {} err, got {previews} / {nothing} / \
				 {errors} over {} samples",
				expected.format,
				expected.previews,
				expected.nothing,
				expected.errors,
				rs.len(),
			);
		}
		let unlisted: Vec<_> = records
			.iter()
			.map(|r| r.fixture.format)
			.filter(|f| !BASELINE.iter().any(|b| b.format == *f))
			.collect();
		assert!(
			unlisted.is_empty(),
			"formats with no baseline row: {unlisted:?}"
		);
	});
}

/// What the previews are actually worth. Nothing here can be asserted as a
/// flat minimum — a Nikon D1H stores a 160x120 strip and no more, so 160x120
/// is the honest answer for it — but the SHAPE of the distribution is a
/// baseline: how many samples reach the size that was asked for, and that
/// none comes back degenerate.
#[test]
#[ignore = "downloads ~1 GiB of pinned RAW samples; run with --ignored"]
fn preview_sizes_match_baseline() {
	with_records(|records| {
		let mut met = 0;
		for r in records {
			let Some((w, h)) = r.full.dims() else {
				continue;
			};
			assert!(
				w.max(h) >= SMALLEST_PREVIEW_EDGE,
				"{} came back at {w}x{h}, smaller than anything in the pinned set",
				r.fixture.cache_name
			);
			// `canvas_dims` caps the long side; a result above it would mean
			// the orchestrator's ceiling was bypassed.
			assert!(
				w.max(h) <= 1600,
				"{} came back at {w}x{h}, past the canvas ceiling",
				r.fixture.cache_name
			);
			if w.max(h) >= TARGET {
				met += 1;
			}
		}
		assert_eq!(
			met, SAMPLES_MEETING_THE_REQUEST,
			"{met} samples reach the {TARGET}px request; baseline is \
			 {SAMPLES_MEETING_THE_REQUEST}"
		);
	});
}

/// The memory contract's other half: bytes pulled. A RAW file is tens of
/// megabytes and may be remote-backed, so discovering that we cannot use one
/// must not cost a full fetch.
///
/// Only the give-up paths are bounded. A sample that does yield a preview may
/// legitimately read to the end of the file — Pentax parks its full-size
/// preview in the last IFD — and bounding that would be asserting the wrong
/// thing.
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
			"every sample yielded a preview; the give-up paths went uncharacterised"
		);
	});
}
