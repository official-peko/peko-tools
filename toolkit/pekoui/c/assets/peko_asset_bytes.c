/*
 * peko_asset_bytes.c
 * Platform-independent helpers built on top of the per-platform asset layer
 * (peko_asset_open / peko_asset_open_dir / peko_asset_size / peko_asset_read /
 * peko_asset_close, implemented in peko_assets_<platform>.c).
 *
 * These back get_asset_bytes on the Pekoscript side: read a whole asset into
 * GC-managed memory and read individual bytes back out of that buffer.
 *
 * GC contract: the asset reads happen into a plain C heap scratch buffer with
 * the blocking bracket around nothing here (file/bundle reads are not socket
 * blocking calls, but if a platform implementation blocks it must bracket
 * internally). The single GC allocation (pgc_alloc_atomic) happens after all
 * reads complete, so no managed pointer is ever held across a read.
 */

#include "peko_assets.h"

#include <stdlib.h>
#include <string.h>

void *peko_asset_bytes(const char *dir, const char *name, int64_t *out_len)
{
    if (out_len)
        *out_len = 0;

    peko_asset *asset = (dir && dir[0])
                          ? peko_asset_open_dir(dir, name)
                          : peko_asset_open(name);
    if (!asset)
        return NULL;

    int64_t size = peko_asset_size(asset);
    if (size < 0) {
        peko_asset_close(asset);
        return NULL;
    }

    /* Read the whole asset into a C heap scratch buffer first. Nothing
     * managed is touched during the reads. */
    unsigned char *scratch = (unsigned char *)malloc(size ? (size_t)size : 1);
    if (!scratch) {
        peko_asset_close(asset);
        return NULL;
    }

    int64_t total = 0;
    while (total < size) {
        int64_t n = peko_asset_read(asset, total, size - total, scratch + total);
        if (n <= 0)
            break;
        total += n;
    }
    peko_asset_close(asset);

    if (total != size) {
        free(scratch);
        return NULL;
    }

    /* Now copy into GC-managed atomic memory. This allocation happens with no
     * read in flight, so the GC contract (no managed pointer across a blocking
     * read) holds trivially. */
    void *managed = pgc_alloc_atomic((size_t)size + 1);
    if (!managed) {
        free(scratch);
        return NULL;
    }
    memcpy(managed, scratch, (size_t)size);
    ((unsigned char *)managed)[size] = 0;   /* NUL pad for string-friendly use */
    free(scratch);

    if (out_len)
        *out_len = size;
    return managed;
}

int peko_asset_byte_at(const void *buffer, int64_t length, int64_t index)
{
    if (!buffer || index < 0 || index >= length)
        return -1;
    return (int)((const unsigned char *)buffer)[index];
}
