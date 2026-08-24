//! Which codecs this build can actually decode.
//!
//! A tripwire, not a feature test: HEIC thumbnails silently stop working if the
//! libde265 wiring regresses, and AVIF thumbnails silently stop working if the
//! dav1d wiring does — nothing else in the stack notices, because a missing
//! decoder looks exactly like an unsupported file. `microthumb`'s HEIF brand
//! list and the SDK's thumbnail extension gate are both written against the
//! answers here, so if this build ever gains or loses a codec, those tables
//! have to follow.
//!
//! These run natively; the wasm build carries the same two decoders (dav1d has
//! none of the `setjmp` that kept libaom out — see `build_dav1d`), so the brand
//! and mime tables need no per-target `cfg`.

use std::io::BufReader;

use heif_decoder::HeifSession;

/// `heif_decoder`'s own `hevc_and_av1_decode` asserts that both decoders are
/// registered AND that this build has no plugin loading at all, which is what
/// makes those registrations necessarily static. It has to: libheif defaults
/// dav1d to a separate `.so`, and a build that shipped one still answered
/// "AV1 decoder present" here, because the test binary dlopened it out of the
/// build machine's `OUT_DIR` — while every AVIF on a device failed as
/// unsupported.
///
/// This one puts a real AVIF through the whole container + codec path. It
/// needs a fixture, so it stays an integration test — which means it runs on
/// demand rather than in CI; the lib-side tripwire is the one that guards the
/// build.
#[test]
fn a_real_avif_decodes() {
	// The repo's committed browser test fixture — no new binary lands here.
	let path = concat!(
		env!("CARGO_MANIFEST_DIR"),
		"/../filen-sdk-rs/web/test-assets/imgs/parrot.avif"
	);
	let file = std::fs::File::open(path).expect("committed parrot.avif fixture");
	let len = file.metadata().unwrap().len();
	let session = HeifSession::new(BufReader::new(file), len).unwrap();

	let (width, height) = session.primary_dims().unwrap();
	assert!(
		(64..=8192).contains(&width) && (64..=8192).contains(&height),
		"parrot.avif reported implausible dimensions {width}x{height}"
	);

	let rgba = session
		.decode_primary_rgba()
		.expect("dav1d must decode the AV1 bitstream");
	assert_eq!((rgba.width(), rgba.height()), (width, height));
	assert_eq!(
		rgba.as_raw().len(),
		(width as usize) * (height as usize) * 4
	);
	// Not a blank canvas: a decoder that registered but produced nothing would
	// hand back an all-zero buffer.
	assert!(
		rgba.as_raw().iter().any(|&b| b != 0),
		"decoded AVIF is entirely zero — the plugin returned an empty frame"
	);
}
