// These are all we need for decoding HEIC images.
#include <libheif/heif_decoding.h>
#include <libheif/heif_context.h>
#include <libheif/heif_image_handle.h>
// Embedded-thumbnail access (heif_image_handle_get_thumbnail and friends).
#include <libheif/heif_aux_images.h>
// Tile-wise decoding of grid images (heif_image_handle_get_image_tiling /
// heif_image_handle_decode_image_tile) — the memory-bounded HEIF path.
#include <libheif/heif_tiling.h>
