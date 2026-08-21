//! Which codecs this build can actually decode.
//!
//! A tripwire, not a feature test: HEIC thumbnails silently stop working if the
//! libde265 wiring regresses, and `microthumb`'s HEIF brand list deliberately
//! omits AVIF on the strength of the AV1 answer here. If AV1 ever becomes
//! available, this test fails and whoever enabled it has to go add the
//! `avif`/`avis` brands and the `avif` extension to the thumbnail gate.

#[test]
fn hevc_decodes_avif_does_not() {
	// SAFETY: both are pure lookups over libheif's static plugin registry.
	let (hevc, av1) = unsafe {
		(
			heif_decoder::heif_have_decoder_for_format(
				heif_decoder::heif_compression_format_heif_compression_HEVC,
			),
			heif_decoder::heif_have_decoder_for_format(
				heif_decoder::heif_compression_format_heif_compression_AV1,
			),
		)
	};
	assert_ne!(
		hevc, 0,
		"libde265 missing — every HEIC thumbnail would fail"
	);
	assert_eq!(
		av1, 0,
		"an AV1 decoder appeared: add the avif/avis brands in microthumb's \
		 formats/heif.rs and `avif` to the mobile thumbnail extension gate"
	);
}
