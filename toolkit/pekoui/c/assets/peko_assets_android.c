/* PEKOUI platform gate: this reader compiles only on its target OS. */
#if defined(__ANDROID__)

/*
 * peko_assets_android.c
 * Android platform asset layer for the assets package.
 *
 * On Android the bundle is the APK and assets live under its assets/
 * directory, which is not a real filesystem path: they must be read through
 * the NDK AAssetManager. The Java/Kotlin layer obtains an AAssetManager with
 * AAssetManager_fromJava and passes it to peko_asset_set_android_manager once
 * at startup, before any asset is opened.
 *
 * AAsset supports seek and ranged reads, so the lazy chunked read used by the
 * other platforms maps directly onto AAsset_seek / AAsset_read, and large
 * assets are streamed rather than loaded fully into memory.
 *
 * Asset names are forward-slash separated and relative; a name that tries to
 * escape the assets root (leading slash, backslash, or "..") is rejected.
 */

#include "peko_assets.h"

#include <android/asset_manager.h>
#include <android/asset_manager_jni.h>

#include <stdio.h>     /* SEEK_SET */
#include <stdlib.h>
#include <string.h>

/* The asset manager supplied by the Java layer at startup. */
static AAssetManager *g_asset_manager = NULL;

/* The opaque handle wraps an open AAsset and its size. */
struct peko_asset {
    AAsset *asset;
    int64_t size;
};

void peko_asset_set_android_manager(void *asset_manager)
{
    g_asset_manager = (AAssetManager *)asset_manager;
}

/* The AAssetManager reached through the running activity, provided by the std
 * Android entry (which records the activity as `gapp`). */
extern AAssetManager *peko_android_asset_manager(void);

/* Resolve the asset manager. It is set once, either pushed in by the Java layer
 * or, failing that, pulled from the activity. Without this the manager stays
 * NULL and every bundled asset fails to open, leaving the web UI blank. */
static AAssetManager *ensure_asset_manager(void)
{
    if (g_asset_manager == NULL)
        g_asset_manager = peko_android_asset_manager();
    return g_asset_manager;
}

/* -------------------------------------------------------------------------
 * Name safety
 * ---------------------------------------------------------------------- */

static int name_is_safe(const char *name)
{
    if (name == NULL || name[0] == '\0')
        return 0;
    if (name[0] == '/' || name[0] == '\\')
        return 0;
    for (size_t i = 0; name[i] != '\0'; i++) {
        if (name[i] == '\\')
            return 0;
        if (name[i] == '.' && name[i + 1] == '.')
            return 0;
    }
    return 1;
}

/* -------------------------------------------------------------------------
 * Open
 *
 * Open the asset through the asset manager. AASSET_MODE_RANDOM allows seeking,
 * which peko_asset_read relies on for ranged reads.
 * ---------------------------------------------------------------------- */

peko_asset *peko_asset_open(const char *name)
{
    AAssetManager *manager = ensure_asset_manager();
    if (manager == NULL || !name_is_safe(name))
        return NULL;

    AAsset *a = AAssetManager_open(manager, name, AASSET_MODE_RANDOM);
    if (a == NULL)
        return NULL;

    peko_asset *handle = (peko_asset *)malloc(sizeof(*handle));
    if (handle == NULL) {
        AAsset_close(a);
        return NULL;
    }
    handle->asset = a;
    handle->size  = (int64_t)AAsset_getLength64(a);
    return handle;
}

/*
 * In debug mode the desktop tooling serves assets from a directory on disk.
 * On a device there is no such directory, so the debug path falls back to the
 * normal asset manager lookup by name, ignoring the directory argument.
 */
peko_asset *peko_asset_open_dir(const char *dir, const char *name)
{
    (void)dir;
    return peko_asset_open(name);
}

int64_t peko_asset_size(peko_asset *handle)
{
    return (handle != NULL) ? handle->size : -1;
}

int64_t peko_asset_read(peko_asset *handle, int64_t offset,
                        int64_t length, void *buffer)
{
    if (handle == NULL || buffer == NULL || offset < 0 || length < 0)
        return -1;

    if (AAsset_seek64(handle->asset, (off64_t)offset, SEEK_SET) == (off64_t)-1)
        return -1;

    int got = AAsset_read(handle->asset, buffer, (size_t)length);
    if (got < 0)
        return -1;
    return (int64_t)got;   /* 0 at end of asset */
}

void peko_asset_close(peko_asset *handle)
{
    if (handle != NULL) {
        if (handle->asset != NULL)
            AAsset_close(handle->asset);
        free(handle);
    }
}

const char *peko_asset_mime_type(const char *name)
{
    const char *dot = (name != NULL) ? strrchr(name, '.') : NULL;
    if (dot == NULL)
        return "application/octet-stream";

    if (strcmp(dot, ".html") == 0 || strcmp(dot, ".htm") == 0)
        return "text/html; charset=utf-8";
    if (strcmp(dot, ".css") == 0)
        return "text/css; charset=utf-8";
    if (strcmp(dot, ".js") == 0 || strcmp(dot, ".mjs") == 0)
        return "text/javascript; charset=utf-8";
    if (strcmp(dot, ".json") == 0)
        return "application/json";
    if (strcmp(dot, ".txt") == 0)
        return "text/plain; charset=utf-8";
    if (strcmp(dot, ".svg") == 0)
        return "image/svg+xml";
    if (strcmp(dot, ".png") == 0)
        return "image/png";
    if (strcmp(dot, ".jpg") == 0 || strcmp(dot, ".jpeg") == 0)
        return "image/jpeg";
    if (strcmp(dot, ".gif") == 0)
        return "image/gif";
    if (strcmp(dot, ".webp") == 0)
        return "image/webp";
    if (strcmp(dot, ".ico") == 0)
        return "image/x-icon";
    if (strcmp(dot, ".woff2") == 0)
        return "font/woff2";
    if (strcmp(dot, ".woff") == 0)
        return "font/woff";
    if (strcmp(dot, ".ttf") == 0)
        return "font/ttf";
    if (strcmp(dot, ".otf") == 0)
        return "font/otf";
    if (strcmp(dot, ".wasm") == 0)
        return "application/wasm";
    if (strcmp(dot, ".mp4") == 0)
        return "video/mp4";
    if (strcmp(dot, ".webm") == 0)
        return "video/webm";
    if (strcmp(dot, ".mp3") == 0)
        return "audio/mpeg";
    if (strcmp(dot, ".wav") == 0)
        return "audio/wav";
    if (strcmp(dot, ".pdf") == 0)
        return "application/pdf";

    return "application/octet-stream";
}

#endif /* platform gate */
