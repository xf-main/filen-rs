// These are all we need for decoding HEIC images.
#include <libheif/heif_decoding.h>
#include <libheif/heif_context.h>
#include <libheif/heif_image_handle.h>
// Embedded-thumbnail access (heif_image_handle_get_thumbnail and friends).
#include <libheif/heif_aux_images.h>
// Tile-wise decoding of grid images (heif_image_handle_get_image_tiling /
// heif_image_handle_decode_image_tile) — the memory-bounded HEIF path.
#include <libheif/heif_tiling.h>
// Item types (heif_item_get_item_type) — how the wasm build tells an HEVC
// file from an AV1 one, which decides the codec thread count it may ask for
// (see DecodeOptions in src/lib.rs).
#include <libheif/heif_items.h>
// Per-context security limits (heif_context_get_security_limits) — the only
// pre-allocation guard on the tile-decode path, which skips libheif's
// whole-image dimension check.
#include <libheif/heif_security.h>
