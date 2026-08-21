//! `set_decode_limits` has two halves and they are enforced in two different
//! places; both are pinned here.
//!
//! - Whole-image decodes: libheif checks the item's `ispe` against
//!   `max_image_size_pixels` itself, before allocating. Proven against the
//!   vendored example image.
//! - Tile decodes: libheif SKIPS that check (`image_item.cc`,
//!   `if (!decode_tile_only)`), so `decode_tile_rgba` enforces it against the
//!   tile item. Proven against a real tiled device HEIC when one is present.
//!
//! What no test here can build is a container that lies — one whose HEVC
//! bitstream decodes larger than its `ispe` promised — because the vendored
//! libheif is decode-only. That residual gap is documented on
//! `set_decode_limits` itself; what these tests pin is that the ceilings we
//! set are the ceilings actually applied, on both paths.

use std::io::BufReader;

use heif_decoder::HeifSession;

fn open_example() -> HeifSession<BufReader<std::fs::File>> {
	let path = concat!(
		env!("CARGO_MANIFEST_DIR"),
		"/deps/libheif/examples/example.heic"
	);
	let file = std::fs::File::open(path)
		.expect("vendored example.heic — libheif submodule must be initialised to build at all");
	let len = file.metadata().unwrap().len();
	HeifSession::new(BufReader::new(file), len).unwrap()
}

#[test]
fn context_limits_refuse_an_over_limit_decode_before_allocating() {
	let mut session = open_example();
	// Far below the example's ~3 MP: every decode must be refused.
	session.set_decode_limits(1000, 1 << 20);
	assert!(
		session.decode_primary_rgba().is_err(),
		"libheif must refuse a decode past max_image_size_pixels"
	);

	// The same image under sane limits decodes — the refusal above was the
	// limit, not the file.
	let mut session = open_example();
	session.set_decode_limits(64_000_000, 512 << 20);
	assert!(session.decode_primary_rgba().is_ok());
}

/// The tile path's own gate: libheif does not apply `max_image_size_pixels`
/// to a single-tile decode, so `decode_tile_rgba` must refuse an over-limit
/// tile itself — before the codec is handed the bitstream.
#[test]
fn tile_decode_refuses_a_tile_over_the_pixel_limit() {
	let path = std::env::var("MICROTHUMB_HEIF_FIXTURE").unwrap_or_else(|_| {
		format!(
			"{}/../iphone_img.heic",
			env!("CARGO_MANIFEST_DIR").replace('\\', "/")
		)
	});
	let Ok(file) = std::fs::File::open(&path) else {
		eprintln!(
			"SKIP tile-limit test: no tiled HEIC at {path} — the tile gate in \
			 decode_tile_rgba is NOT exercised by this run (set \
			 MICROTHUMB_HEIF_FIXTURE to a device HEIC to cover it)"
		);
		return;
	};
	let len = file.metadata().unwrap().len();
	let mut session = HeifSession::new(BufReader::new(file), len).unwrap();
	let tiling = session
		.tiling()
		.unwrap()
		.expect("the fixture must be a grid image for this test to mean anything");

	// A ceiling under one tile: the gate must refuse without decoding.
	let one_tile = u64::from(tiling.tile_width) * u64::from(tiling.tile_height);
	session.set_decode_limits(one_tile - 1, 512 << 20);
	assert!(
		session.decode_tile_rgba(0, 0).is_err(),
		"a tile declaring more than max_image_size_pixels must be refused"
	);

	// Same tile, ceiling above it: decodes, so the refusal was the limit.
	session.set_decode_limits(one_tile, 512 << 20);
	let tile = session
		.decode_tile_rgba(0, 0)
		.expect("a tile within the limit must still decode");
	assert_eq!(
		(tile.width(), tile.height()),
		(tiling.tile_width, tiling.tile_height)
	);
}
