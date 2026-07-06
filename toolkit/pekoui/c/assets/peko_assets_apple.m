/* PEKOUI platform gate: this reader compiles only on its target OS. */
#if defined(__APPLE__)

/*
 * peko_assets_apple.m
 * macOS and iOS platform asset layer for the assets package.
 *
 * On Apple platforms the assets are copied into the application bundle as
 * loose resource files (no asset catalog / .car). They are resolved with
 * NSBundle: the hierarchical asset name "icons/home.png" maps to the resource
 * "icons/home" with type "png" inside the main bundle. macOS keeps the
 * bundle's directory structure and iOS flattens it, but NSBundle resolves the
 * resource the same way on both, so this one file serves both platforms.
 *
 * Once the on-disk path is resolved, reading is plain buffered file IO, the
 * same lazy chunked read used by the other platforms, so large assets are
 * never loaded fully into memory.
 *
 * Asset names are forward-slash separated and relative. A name that tries to
 * escape the bundle (a leading slash, a backslash, or a ".." component) is
 * rejected.
 */

#import <Foundation/Foundation.h>

#include "peko_assets.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* The opaque handle: a standard buffered file plus its total size. */
struct peko_asset {
    FILE   *fp;
    int64_t size;
};

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
 * Open helpers
 * ---------------------------------------------------------------------- */

static peko_asset *open_full_path(const char *path)
{
    FILE *fp = fopen(path, "rb");
    if (fp == NULL)
        return NULL;

    if (fseek(fp, 0, SEEK_END) != 0) {
        fclose(fp);
        return NULL;
    }
    long size = ftell(fp);
    if (size < 0) {
        fclose(fp);
        return NULL;
    }
    rewind(fp);

    peko_asset *asset = (peko_asset *)malloc(sizeof(*asset));
    if (asset == NULL) {
        fclose(fp);
        return NULL;
    }
    asset->fp   = fp;
    asset->size = (int64_t)size;
    return asset;
}

/*
 * Resolve an asset name to its on-disk path inside the main bundle. Splits the
 * name into a directory-qualified resource name and an extension so NSBundle
 * can locate it, e.g. "icons/home.png" -> resource "icons/home", type "png".
 * Returns an autoreleased NSString path, or nil if the resource is not found.
 */
static NSString *resolve_bundle_path(const char *name)
{
    NSString *full = [NSString stringWithUTF8String:name];
    if (full == nil)
        return nil;

    NSString *ext  = [full pathExtension];               /* "png" or "" */
    NSString *base = [full stringByDeletingPathExtension];/* "icons/home" */

    NSBundle *bundle = [NSBundle mainBundle];
    NSString *path;
    if ([ext length] > 0)
        path = [bundle pathForResource:base ofType:ext];
    else
        path = [bundle pathForResource:base ofType:nil];

    return path;   /* nil if not present in the bundle */
}

/* -------------------------------------------------------------------------
 * Public platform interface
 * ---------------------------------------------------------------------- */

peko_asset *peko_asset_open(const char *name)
{
    if (!name_is_safe(name))
        return NULL;

    @autoreleasepool {
        NSString *path = resolve_bundle_path(name);
        if (path == nil)
            return NULL;
        return open_full_path([path fileSystemRepresentation]);
    }
}

peko_asset *peko_asset_open_dir(const char *dir, const char *name)
{
    if (dir == NULL || !name_is_safe(name))
        return NULL;

    char path[4096];
    int  written = snprintf(path, sizeof(path), "%s/%s", dir, name);
    if (written < 0 || (size_t)written >= sizeof(path))
        return NULL;

    return open_full_path(path);
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
    if (fseek(handle->fp, (long)offset, SEEK_SET) != 0)
        return -1;

    size_t got = fread(buffer, 1, (size_t)length, handle->fp);
    if (got == 0 && ferror(handle->fp))
        return -1;
    return (int64_t)got;
}

void peko_asset_close(peko_asset *handle)
{
    if (handle != NULL) {
        if (handle->fp != NULL)
            fclose(handle->fp);
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
