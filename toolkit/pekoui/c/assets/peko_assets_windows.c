/* PEKOUI platform gate: this reader compiles only on its target OS. */
#if defined(_WIN32)

/*
 * peko_assets_windows.c
 * Windows platform asset layer for the assets package.
 *
 * On Windows the assets are compiled into the executable as embedded resources
 * via a resource script (.rc). Each asset is declared under a custom resource
 * type (PEKO_ASSET) with the asset's hierarchical name as the resource name,
 * for example:
 *
 *     icons/home.png  PEKO_ASSET  "assets\\icons\\home.png"
 *
 * with the resource name given as the (uppercased) asset path. The asset is
 * located with FindResource, loaded with LoadResource, and its bytes are
 * obtained with LockResource. Resource data is memory-resident and read-only
 * for the life of the module, so the handle simply wraps a pointer and a size
 * and peko_asset_read copies from the requested offset. Nothing needs freeing
 * on close beyond the small handle.
 *
 * Asset names are forward-slash separated and relative; a name that tries to
 * escape (leading slash or a ".." component) is rejected. Forward slashes in
 * the name are converted to the resource naming convention before lookup.
 */

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

#include "peko_assets.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>

/* The custom resource type all assets are declared under in the .rc file.
 * Must match the type used in the resource script. */
#define PEKO_ASSET_RESTYPE "PEKO_ASSET"

/* The opaque handle wraps the asset bytes and size. For resource-backed assets
 * the bytes point into module memory (owned by the module, never freed). For
 * debug (open_dir) assets the bytes are a heap buffer this handle owns. The
 * owns_data flag records which, so close frees only what it should. */
struct peko_asset {
    const unsigned char *data;
    int64_t              size;
    int                  owns_data;   /* 1 if data is a heap buffer to free */
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
        if (name[i] == '.' && name[i + 1] == '.')
            return 0;
    }
    return 1;
}

/* -------------------------------------------------------------------------
 * Open
 *
 * Resolve the asset name to its embedded resource and wrap its bytes. The
 * resource name convention is the asset path uppercased; adjust to match the
 * naming used in the .rc file if a different convention is chosen.
 * ---------------------------------------------------------------------- */

peko_asset *peko_asset_open(const char *name)
{
    if (!name_is_safe(name))
        return NULL;

    /* Build the resource name from the asset name. */
    char resname[1024];
    size_t i = 0;
    for (; name[i] != '\0' && i + 1 < sizeof(resname); i++)
        resname[i] = (char)toupper((unsigned char)name[i]);
    resname[i] = '\0';

    HMODULE module = GetModuleHandle(NULL);

    HRSRC info = FindResourceA(module, resname, PEKO_ASSET_RESTYPE);
    if (info == NULL)
        return NULL;

    DWORD   size   = SizeofResource(module, info);
    HGLOBAL loaded = LoadResource(module, info);
    if (loaded == NULL)
        return NULL;

    const void *bytes = LockResource(loaded);
    if (bytes == NULL)
        return NULL;

    peko_asset *handle = (peko_asset *)malloc(sizeof(*handle));
    if (handle == NULL)
        return NULL;

    handle->data = (const unsigned char *)bytes;
    handle->size = (int64_t)size;
    handle->owns_data = 0;   /* module-owned resource memory */
    return handle;
}

/*
 * In debug mode the desktop tooling serves assets from a directory on disk.
 * Read the file directly so hot-reload works without rebuilding resources.
 */
peko_asset *peko_asset_open_dir(const char *dir, const char *name)
{
    if (dir == NULL || !name_is_safe(name))
        return NULL;

    char path[4096];
    int  written = snprintf(path, sizeof(path), "%s\\%s", dir, name);
    if (written < 0 || (size_t)written >= sizeof(path))
        return NULL;

    /* Convert forward slashes in the asset name to backslashes for Windows. */
    for (char *p = path; *p != '\0'; p++) {
        if (*p == '/')
            *p = '\\';
    }

    HANDLE file = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ, NULL,
                              OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE)
        return NULL;

    LARGE_INTEGER li;
    if (!GetFileSizeEx(file, &li)) {
        CloseHandle(file);
        return NULL;
    }

    /* Read the whole file into a heap buffer the handle owns. Debug builds are
     * not memory-critical, and this keeps peko_asset_read uniform. */
    unsigned char *buf = (unsigned char *)malloc(li.QuadPart ? (size_t)li.QuadPart : 1);
    if (buf == NULL) {
        CloseHandle(file);
        return NULL;
    }

    int64_t total = 0;
    while (total < li.QuadPart) {
        DWORD got = 0;
        if (!ReadFile(file, buf + total, (DWORD)(li.QuadPart - total), &got, NULL) || got == 0)
            break;
        total += got;
    }
    CloseHandle(file);

    if (total != li.QuadPart) {
        free(buf);
        return NULL;
    }

    peko_asset *handle = (peko_asset *)malloc(sizeof(*handle));
    if (handle == NULL) {
        free(buf);
        return NULL;
    }
    handle->data      = buf;
    handle->size      = total;
    handle->owns_data = 1;   /* heap buffer owned by this handle */
    return handle;
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
    if (offset >= handle->size)
        return 0;   /* at or past end */

    int64_t available = handle->size - offset;
    int64_t to_copy   = (length < available) ? length : available;
    memcpy(buffer, handle->data + offset, (size_t)to_copy);
    return to_copy;
}

void peko_asset_close(peko_asset *handle)
{
    if (handle != NULL) {
        /* Resource-backed handles point into module memory and must not be
         * freed; debug (open_dir) handles own a heap buffer that must be. */
        if (handle->owns_data)
            free((void *)handle->data);
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
