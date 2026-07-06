/* PEKOUI platform gate: this reader compiles only on its target OS. */
#if defined(__linux__) && !defined(__ANDROID__)

/*
 * peko_assets_linux.c
 * Linux platform asset layer for the assets package.
 *
 * On Linux the app ships as an AppImage: the bundle is a squashfs image that
 * the AppImage runtime mounts read-only and exposes through the APPDIR
 * environment variable. Because the mount is an ordinary read-only filesystem,
 * reading a bundled asset is plain file IO rooted at the bundle's asset
 * directory; no squashfs library is needed.
 *
 * Asset names are hierarchical and forward-slash separated (e.g.
 * "icons/home.png"). They are resolved against the bundle's asset root. A name
 * that tries to escape the root (a leading slash, a backslash, or a ".."
 * component) is rejected so a request cannot read outside the bundle.
 *
 * The bundle asset root is determined once:
 *   1. If APPDIR is set (running from a mounted AppImage), assets live at
 *      $APPDIR/usr/share/assets.
 *   2. Otherwise, for a plain install or development run, resolve relative to
 *      the executable: <dir of /proc/self/exe>/assets.
 *   3. As a last resort, the relative path "assets".
 * Adjust ASSET_SUBPATH below if assets are bundled at a different location.
 */

#include "peko_assets.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <limits.h>

/* Location of the asset directory inside the AppImage, relative to APPDIR. */
#define ASSET_SUBPATH "/usr/share/assets"

/* The opaque handle: a standard buffered file plus its total size. */
struct peko_asset {
    FILE   *fp;
    int64_t size;
};

/* -------------------------------------------------------------------------
 * Name safety
 *
 * Reject names that could escape the asset root. The server already screens
 * requests, but the platform layer guards independently so any caller is safe.
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
 * Bundle root resolution (computed once, then cached)
 * ---------------------------------------------------------------------- */

static const char *asset_root(void)
{
    static char root[PATH_MAX];
    static int  resolved = 0;
    if (resolved)
        return root;
    resolved = 1;

    const char *appdir = getenv("APPDIR");
    if (appdir != NULL && appdir[0] != '\0') {
        snprintf(root, sizeof(root), "%s%s", appdir, ASSET_SUBPATH);
        return root;
    }

    /* Fall back to the directory containing the executable. */
    char    exe[PATH_MAX];
    ssize_t n = readlink("/proc/self/exe", exe, sizeof(exe) - 1);
    if (n > 0) {
        exe[n] = '\0';
        char *slash = strrchr(exe, '/');
        if (slash != NULL)
            *slash = '\0';
        int w = snprintf(root, sizeof(root), "%s/assets", exe);
        if (w > 0 && (size_t)w < sizeof(root))
            return root;
        /* If the executable path was too long to append to, fall through. */
    }

    snprintf(root, sizeof(root), "assets");
    return root;
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

/* -------------------------------------------------------------------------
 * Public platform interface
 * ---------------------------------------------------------------------- */

peko_asset *peko_asset_open(const char *name)
{
    if (!name_is_safe(name))
        return NULL;

    char path[PATH_MAX];
    int  written = snprintf(path, sizeof(path), "%s/%s", asset_root(), name);
    if (written < 0 || (size_t)written >= sizeof(path))
        return NULL;   /* path too long */

    return open_full_path(path);
}

peko_asset *peko_asset_open_dir(const char *dir, const char *name)
{
    if (dir == NULL || !name_is_safe(name))
        return NULL;

    char path[PATH_MAX];
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
