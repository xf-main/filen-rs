// Force-included (-include) into every libheif/libde265 translation unit when
// cross-compiling for wasm32 with wasi-sdk. wasi-libc has no temp directories,
// so its headers hide mkstemp and its libc.a lacks the symbol; libheif only
// reaches it on the encode side (Box_iloc::set_use_tmp_file, never called when
// decoding), so satisfy the reference with an always-failing stand-in instead
// of leaving an unresolvable symbol.
#pragma once

#ifdef __wasi__

#ifdef __cplusplus
extern "C" {
#endif

static inline int mkstemp(char* template_path) {
	(void)template_path;
	return -1;
}

#ifdef __cplusplus
}
#endif

#endif // __wasi__
