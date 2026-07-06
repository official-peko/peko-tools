/*
 * peko_assets.h
 * Shared types and declarations for the Peko assets package.
 * Include this header in every assets C implementation file.
 *
 * Two layers are declared here:
 *   1. The platform asset layer: a uniform, handle-based interface that
 *      retrieves raw asset bytes from each platform's native bundle
 *      location (NSBundle, AssetManager, AppImage squashfs, embedded
 *      resources). Implemented per platform in peko_assets_<platform>.c.
 *   2. The asset HTTP server: a small local server that streams asset
 *      bytes to a webview over HTTP, with Range support for media seeking.
 *      Implemented in peko_asset_server.c.
 */

#ifndef PEKO_ASSETS_H
#define PEKO_ASSETS_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* -------------------------------------------------------------------------
 * Peko GC interface (precise pgc runtime, linked from the runtime package)
 *
 * The asset server runs its own threads and makes blocking socket calls, so
 * it must follow the same GC contract as the sockets and threads packages:
 * attach threads that touch managed memory, bracket every blocking call with
 * pgc_begin_blocking/pgc_end_blocking, and allocate GC memory only outside a
 * blocking region. The asset layer itself returns plain C heap buffers and
 * never touches managed memory, so it needs no GC calls.
 * ---------------------------------------------------------------------- */

void  pgc_thread_attach(void);
void  pgc_thread_detach(void);
void  pgc_begin_blocking(void);
void  pgc_end_blocking(void);
void *pgc_alloc_atomic(size_t size);

/* -------------------------------------------------------------------------
 * Platform asset layer
 *
 * A handle is an opaque pointer to an open asset. The handle owns whatever
 * the platform needs to read the asset lazily (a file descriptor, a mapped
 * region, an AssetManager stream, a squashfs offset). Names are hierarchical
 * and forward-slash separated, e.g. "icons/home.png"; the subdirectory
 * structure is preserved and resolved against the platform container at open
 * time. No precomputed manifest is used.
 * ---------------------------------------------------------------------- */

typedef struct peko_asset peko_asset;

/*
 * Opens the named asset from the platform bundle.
 * Returns an opaque handle, or NULL if the asset does not exist.
 * The caller owns the handle and must close it with peko_asset_close.
 */
peko_asset *peko_asset_open(const char *name);

/*
 * Opens the named asset from a directory on disk instead of the bundle.
 * Used in debug builds, where dir is the project's assets directory and name
 * is resolved relative to it. Returns NULL if the file does not exist.
 */
peko_asset *peko_asset_open_dir(const char *dir, const char *name);

/* Returns the total size of the asset in bytes. */
int64_t peko_asset_size(peko_asset *handle);

/*
 * Reads up to length bytes starting at offset into buffer.
 * Returns the number of bytes read, 0 at end of asset, or -1 on error.
 * Reading a chunk at a time keeps large assets out of memory.
 */
int64_t peko_asset_read(peko_asset *handle, int64_t offset,
                        int64_t length, void *buffer);

/* Closes the asset and releases its handle. */
void peko_asset_close(peko_asset *handle);

/*
 * Returns the MIME type for a name based on its file extension
 * (".png" -> "image/png", ".css" -> "text/css", and so on).
 * Returns "application/octet-stream" when the extension is unknown.
 * The returned pointer is a static string and must not be freed.
 */
const char *peko_asset_mime_type(const char *name);

/* -------------------------------------------------------------------------
 * One-shot asset read (whole asset into a GC-managed buffer)
 *
 * Reads the entire named asset and returns a pointer to GC-managed atomic
 * memory holding its bytes, writing the byte count to *out_len. Returns NULL
 * and sets *out_len to 0 if the asset does not exist. The buffer is owned by
 * the Peko GC; the caller does not free it. When dir is non-NULL the asset is
 * read from that directory (debug mode) rather than the bundle.
 *
 * This is the backing call for get_asset_bytes. It allocates the GC buffer
 * outside any blocking region, as the GC contract requires.
 * ---------------------------------------------------------------------- */

void *peko_asset_bytes(const char *dir, const char *name, int64_t *out_len);

/*
 * Reads a single byte (0..255) at index from a buffer returned by
 * peko_asset_bytes. Returns -1 if index is out of range. This gives the
 * Pekoscript layer an unambiguous way to copy the raw bytes into a managed
 * Array<int> without relying on raw pointer indexing across the boundary.
 */
int peko_asset_byte_at(const void *buffer, int64_t length, int64_t index);

/* -------------------------------------------------------------------------
 * Android initialization
 *
 * On Android the bundle is the APK and assets are read through the NDK
 * AAssetManager rather than the filesystem. The Java/Kotlin layer must pass
 * the AAssetManager (obtained from AAssetManager_fromJava) to this hook once
 * at startup, before any asset is opened. The argument is a void * so the
 * header has no NDK dependency; on Android it is an AAssetManager *. On other
 * platforms this function is not provided and not needed.
 * ---------------------------------------------------------------------- */

void peko_asset_set_android_manager(void *asset_manager);

/* -------------------------------------------------------------------------
 * Asset HTTP server
 *
 * A small local HTTP server dedicated to serving assets. It binds a dynamic
 * loopback port, then serves GET requests under the reserved /_assets/ prefix
 * by streaming the named asset in chunks. It honors HTTP Range requests so
 * media elements can seek without downloading the whole asset.
 *
 * The server reads from the bundle via the asset layer, or from debug_dir on
 * disk when debug_dir is non-NULL and non-empty (debug builds). The byte
 * source is the only difference between debug and release; the URL pattern is
 * identical.
 * ---------------------------------------------------------------------- */

/*
 * Starts the asset HTTP server on a fresh background thread bound to a
 * dynamic loopback port. debug_dir is the project assets directory in debug
 * builds, or NULL/"" in release builds (serve from the bundle). Returns the
 * bound port on success, or 0 on failure. Safe to call once at startup.
 */
int peko_asset_server_start(const char *debug_dir);

/* Returns the port the asset server is bound to, or 0 if not started. */
int peko_asset_server_port(void);

/* Stops the asset server and releases its resources. */
void peko_asset_server_stop(void);

#endif /* PEKO_ASSETS_H */
