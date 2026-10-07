# gen_svt_ffi

Regenerates `../../src/svt/ffi.rs` (the SVT-AV1 C bindings). Only needed when the supported SVT-AV1
version changes; normal builds use the committed file and need neither libclang nor the SVT headers.

```sh
# headers: `apt download libsvtav1enc-dev && dpkg -x libsvtav1enc-dev_*.deb root`  (no root needed)
BINDGEN_EXTRA_CLANG_ARGS="-isystem $(dirname $(gcc -print-file-name=include/stddef.h))" \
  cargo run --release -- /path/to/root/usr/include/svt-av1 > /tmp/ffi.rs
# then prepend the 5-line "@generated" header that the committed file has.
```

The bindings are loaded with `dlopen` (bindgen `dynamic_library_name`), so nothing links against
SVT-AV1 at build time and a machine without the library gets a clear run-time error.
