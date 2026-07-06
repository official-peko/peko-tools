/*
 * peko_asset_server.c
 * A small local HTTP server dedicated to serving bundled assets to a webview.
 *
 * It binds a dynamic loopback port, accepts connections on a background
 * thread, and serves GET requests under the /_assets/ prefix by streaming the
 * named asset in fixed-size chunks. It honors HTTP Range requests so media
 * elements can seek without downloading the whole asset, and keeps
 * connections alive so a page can pull many assets over few sockets.
 *
 * Byte source: the platform asset layer (peko_asset_open) in release builds,
 * or a directory on disk (peko_asset_open_dir) in debug builds. The byte
 * source is the only difference between the two; the URL pattern is identical.
 *
 * Memory model:
 *   - Large assets are streamed straight from the handle, a chunk at a time,
 *     so a multi-hundred-megabyte video never sits in memory.
 *   - Small assets are cached in a bounded LRU so repeated lookups (icons,
 *     css, fonts) avoid re-opening the bundle. The cache has a hard byte cap;
 *     anything larger than the per-entry cap is always streamed, never cached.
 *
 * GC contract (identical to the sockets and threads packages):
 *   - The listener and per-connection work run on threads that attach to the
 *     collector (pgc_thread_attach) so their stacks are scannable.
 *   - Every blocking socket call (accept, recv, send) is wrapped with
 *     pgc_begin_blocking / pgc_end_blocking so a collection can proceed while
 *     this thread is parked in the kernel.
 *   - This server never allocates or holds managed (GC) pointers. Asset bytes
 *     live in plain C heap and handle reads, so no managed pointer is ever
 *     held across a blocking call.
 */

#include "peko_assets.h"
#include "peko_threads.h"   /* peko_mutex_t and the cross-platform thread API */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* -------------------------------------------------------------------------
 * Platform socket layer
 * Mirrors the portability shims used by the sockets package so the two agree
 * on socket types and the blocking-call wrappers.
 * ---------------------------------------------------------------------- */

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <winsock2.h>
#  include <ws2tcpip.h>
#  pragma comment(lib, "ws2_32.lib")
   typedef SOCKET asset_socket_t;
#  define ASSET_INVALID_SOCKET INVALID_SOCKET
#  define asset_close_socket(s) closesocket(s)
#  define asset_send(s, buf, len) send((s), (buf), (int)(len), 0)
#  define asset_recv(s, buf, len) recv((s), (buf), (int)(len), 0)
#  define strncasecmp _strnicmp
#  define strcasecmp  _stricmp
#else
#  include <arpa/inet.h>
#  include <netinet/in.h>
#  include <sys/socket.h>
#  include <sys/types.h>
#  include <strings.h>          /* strcasecmp, strncasecmp */
#  include <unistd.h>
   typedef int asset_socket_t;
#  define ASSET_INVALID_SOCKET (-1)
#  define asset_close_socket(s) do { shutdown((s), SHUT_RDWR); close((s)); } while (0)
#  define asset_send(s, buf, len) send((s), (buf), (len), 0)
#  define asset_recv(s, buf, len) recv((s), (buf), (len), 0)
#endif

#include <stdint.h>
#include <stdbool.h>

/* -------------------------------------------------------------------------
 * Tunables
 * ---------------------------------------------------------------------- */

/* Bytes moved per streaming iteration. 64 KiB balances syscall count against
 * the size of the transient C heap buffer each connection holds. */
#define ASSET_STREAM_CHUNK     ((size_t)64 * 1024)

/* Largest request head (request line + headers) we will read. A request with
 * a head bigger than this is rejected; asset GETs are tiny. */
#define ASSET_REQ_HEAD_MAX     ((size_t)16 * 1024)

/* LRU cache caps. An asset larger than ASSET_CACHE_ENTRY_MAX is never cached
 * (always streamed). The cache holds at most ASSET_CACHE_TOTAL_MAX bytes and
 * ASSET_CACHE_SLOTS entries, whichever binds first. */
#define ASSET_CACHE_ENTRY_MAX  ((size_t)1 * 1024 * 1024)
#define ASSET_CACHE_TOTAL_MAX  ((size_t)16 * 1024 * 1024)
#define ASSET_CACHE_SLOTS      64

/* The reserved URL prefix all asset routes live under. */
#define ASSET_PREFIX           "/_assets/"
#define ASSET_PREFIX_LEN       (sizeof(ASSET_PREFIX) - 1)

/* =========================================================================
 * Bounded LRU cache of small assets
 *
 * Keyed by asset name. Each entry owns a C heap copy of the asset bytes and
 * its MIME type. A monotonically increasing tick records last use; eviction
 * drops the smallest tick. Protected by a single mutex because the listener
 * may serve connections from more than one thread over the server's life.
 * ====================================================================== */

typedef struct {
    char    *name;          /* owned copy of the asset name (key)        */
    unsigned char *bytes;   /* owned copy of the asset bytes             */
    size_t   length;        /* number of bytes                           */
    const char *mime;       /* static MIME string (not owned)            */
    uint64_t tick;          /* last-use counter for LRU                  */
    bool     used;          /* slot occupied                            */
} asset_cache_entry;

typedef struct {
    asset_cache_entry slots[ASSET_CACHE_SLOTS];
    size_t            total_bytes;
    uint64_t          clock;
    peko_mutex_t      lock;
    bool              ready;
} asset_cache;

static asset_cache g_cache;

static void cache_init(void)
{
    if (g_cache.ready)
        return;
    memset(&g_cache, 0, sizeof(g_cache));
    peko_mutex_init(&g_cache.lock);
    g_cache.ready = true;
}

/* Evict the least-recently-used entry. Caller holds the lock. */
static void cache_evict_one(void)
{
    int      victim = -1;
    uint64_t oldest = UINT64_MAX;
    for (int i = 0; i < ASSET_CACHE_SLOTS; i++) {
        if (g_cache.slots[i].used && g_cache.slots[i].tick < oldest) {
            oldest = g_cache.slots[i].tick;
            victim = i;
        }
    }
    if (victim < 0)
        return;
    asset_cache_entry *e = &g_cache.slots[victim];
    g_cache.total_bytes -= e->length;
    free(e->name);
    free(e->bytes);
    memset(e, 0, sizeof(*e));
}

/*
 * Look the name up in the cache. On a hit, copies the byte pointer/length/mime
 * into the out params (the pointer remains owned by the cache; the caller must
 * use it only while holding nothing that could evict it, so we copy the bytes
 * out under the lock below instead). Returns true on hit.
 *
 * To keep the lock hold short and avoid handing out a pointer that a
 * concurrent eviction could free, a hit copies the bytes into a fresh caller
 * buffer. Small assets only, so the copy is cheap.
 */
static unsigned char *cache_get_copy(const char *name, size_t *out_len,
                                     const char **out_mime)
{
    if (!g_cache.ready)
        return NULL;

    unsigned char *copy = NULL;
    peko_mutex_lock(&g_cache.lock);
    for (int i = 0; i < ASSET_CACHE_SLOTS; i++) {
        asset_cache_entry *e = &g_cache.slots[i];
        if (e->used && strcmp(e->name, name) == 0) {
            copy = (unsigned char *)malloc(e->length ? e->length : 1);
            if (copy) {
                memcpy(copy, e->bytes, e->length);
                *out_len  = e->length;
                *out_mime = e->mime;
                e->tick   = ++g_cache.clock;
            }
            break;
        }
    }
    peko_mutex_unlock(&g_cache.lock);
    return copy;
}

/* Insert a copy of the bytes under name. Caller passes ownership of nothing;
 * this makes its own copies. Skips assets larger than the per-entry cap. */
static void cache_put(const char *name, const unsigned char *bytes,
                      size_t length, const char *mime)
{
    if (!g_cache.ready || length > ASSET_CACHE_ENTRY_MAX)
        return;

    peko_mutex_lock(&g_cache.lock);

    /* If already present, refresh in place. */
    for (int i = 0; i < ASSET_CACHE_SLOTS; i++) {
        asset_cache_entry *e = &g_cache.slots[i];
        if (e->used && strcmp(e->name, name) == 0) {
            e->tick = ++g_cache.clock;
            peko_mutex_unlock(&g_cache.lock);
            return;
        }
    }

    /* Make room: by total bytes and by free slot. */
    while (g_cache.total_bytes + length > ASSET_CACHE_TOTAL_MAX)
        cache_evict_one();

    int slot = -1;
    for (int i = 0; i < ASSET_CACHE_SLOTS; i++) {
        if (!g_cache.slots[i].used) { slot = i; break; }
    }
    if (slot < 0) {
        cache_evict_one();
        for (int i = 0; i < ASSET_CACHE_SLOTS; i++) {
            if (!g_cache.slots[i].used) { slot = i; break; }
        }
    }
    if (slot < 0) {
        peko_mutex_unlock(&g_cache.lock);
        return;
    }

    char          *name_copy  = NULL;
    unsigned char *bytes_copy = NULL;
    size_t         name_len   = strlen(name) + 1;
    name_copy  = (char *)malloc(name_len);
    bytes_copy = (unsigned char *)malloc(length ? length : 1);
    if (!name_copy || !bytes_copy) {
        free(name_copy);
        free(bytes_copy);
        peko_mutex_unlock(&g_cache.lock);
        return;
    }
    memcpy(name_copy, name, name_len);
    memcpy(bytes_copy, bytes, length);

    asset_cache_entry *e = &g_cache.slots[slot];
    e->name   = name_copy;
    e->bytes  = bytes_copy;
    e->length = length;
    e->mime   = mime;
    e->tick   = ++g_cache.clock;
    e->used   = true;
    g_cache.total_bytes += length;

    peko_mutex_unlock(&g_cache.lock);
}

static void cache_clear(void)
{
    if (!g_cache.ready)
        return;
    peko_mutex_lock(&g_cache.lock);
    for (int i = 0; i < ASSET_CACHE_SLOTS; i++) {
        if (g_cache.slots[i].used) {
            free(g_cache.slots[i].name);
            free(g_cache.slots[i].bytes);
            memset(&g_cache.slots[i], 0, sizeof(g_cache.slots[i]));
        }
    }
    g_cache.total_bytes = 0;
    peko_mutex_unlock(&g_cache.lock);
}

/* =========================================================================
 * Server state
 * ====================================================================== */

typedef struct {
    asset_socket_t listen_sock;
    int            port;
    char          *debug_dir;     /* owned copy, or NULL in release       */
    atomic_int     running;
#ifdef _WIN32
    HANDLE         thread;
#else
    pthread_t      thread;
#endif
} asset_server;

static asset_server g_server;
static atomic_int   g_started = 0;

/* =========================================================================
 * Blocking-wrapped socket primitives
 *
 * Each wraps exactly one blocking syscall with the GC blocking bracket, so a
 * collection can run while this thread waits in the kernel. Nothing managed is
 * touched between begin and end.
 * ====================================================================== */

static int64_t blocking_recv(asset_socket_t s, void *buf, size_t len)
{
    pgc_begin_blocking();
    int64_t n = (int64_t)asset_recv(s, buf, len);
    pgc_end_blocking();
    return n;
}

static int64_t blocking_send(asset_socket_t s, const void *buf, size_t len)
{
    pgc_begin_blocking();
    int64_t n = (int64_t)asset_send(s, buf, len);
    pgc_end_blocking();
    return n;
}

static asset_socket_t blocking_accept(asset_socket_t listen_sock)
{
    struct sockaddr_in addr;
    socklen_t          addr_len = sizeof(addr);
    pgc_begin_blocking();
    asset_socket_t c = accept(listen_sock, (struct sockaddr *)&addr, &addr_len);
    pgc_end_blocking();
    return c;
}

/* Send the whole buffer, looping over partial writes. Returns 0 on success,
 * -1 if the peer went away. */
static int send_all(asset_socket_t s, const void *data, size_t len)
{
    const unsigned char *p = (const unsigned char *)data;
    size_t sent = 0;
    while (sent < len) {
        int64_t n = blocking_send(s, p + sent, len - sent);
        if (n <= 0)
            return -1;
        sent += (size_t)n;
    }
    return 0;
}

/* =========================================================================
 * HTTP request parsing (minimal, asset-specific)
 * ====================================================================== */

typedef struct {
    char    name[1024];      /* decoded asset name (path after the prefix) */
    bool    has_range;
    int64_t range_start;
    int64_t range_end;       /* inclusive; -1 means "to end"               */
    bool    keep_alive;
    bool    is_get;
    bool    is_head;
    bool    valid;
} asset_request;

/* Percent-decode src into dst (dst must hold at least strlen(src)+1). */
static void url_decode(const char *src, char *dst, size_t dst_size)
{
    size_t o = 0;
    for (size_t i = 0; src[i] != '\0' && o + 1 < dst_size; i++) {
        char c = src[i];
        if (c == '%' && src[i+1] && src[i+2]) {
            char hi = src[i+1], lo = src[i+2];
            int  hv = (hi >= '0' && hi <= '9') ? hi - '0'
                    : (hi >= 'a' && hi <= 'f') ? hi - 'a' + 10
                    : (hi >= 'A' && hi <= 'F') ? hi - 'A' + 10 : -1;
            int  lv = (lo >= '0' && lo <= '9') ? lo - '0'
                    : (lo >= 'a' && lo <= 'f') ? lo - 'a' + 10
                    : (lo >= 'A' && lo <= 'F') ? lo - 'A' + 10 : -1;
            if (hv >= 0 && lv >= 0) {
                dst[o++] = (char)((hv << 4) | lv);
                i += 2;
                continue;
            }
        }
        dst[o++] = c;
    }
    dst[o] = '\0';
}

/*
 * Reject names that try to escape the asset root via "..", a leading slash,
 * or a backslash. Asset names are relative, forward-slash separated.
 */
static bool name_is_safe(const char *name)
{
    if (name[0] == '\0' || name[0] == '/' || name[0] == '\\')
        return false;
    for (size_t i = 0; name[i] != '\0'; i++) {
        if (name[i] == '\\')
            return false;
        if (name[i] == '.' && name[i+1] == '.')
            return false;
    }
    return true;
}

/* Parse Range: bytes=START-END forms. Sets has_range and the bounds. */
static void parse_range(const char *value, asset_request *req)
{
    /* Skip optional "bytes=" unit prefix. */
    const char *p = value;
    while (*p == ' ' || *p == '\t') p++;
    if (strncasecmp(p, "bytes=", 6) == 0)
        p += 6;

    int64_t start = -1, end = -1;
    bool have_start = false, have_end = false;

    /* start */
    if (*p >= '0' && *p <= '9') {
        start = 0;
        while (*p >= '0' && *p <= '9') { start = start * 10 + (*p - '0'); p++; }
        have_start = true;
    }
    if (*p != '-')
        return;            /* malformed; ignore the range entirely */
    p++;
    /* end */
    if (*p >= '0' && *p <= '9') {
        end = 0;
        while (*p >= '0' && *p <= '9') { end = end * 10 + (*p - '0'); p++; }
        have_end = true;
    }

    if (!have_start && !have_end)
        return;

    req->has_range = true;
    if (!have_start) {
        /* suffix form: bytes=-N means the last N bytes */
        req->range_start = -end;   /* negative marks suffix; resolved later */
        req->range_end   = -1;
    } else {
        req->range_start = start;
        req->range_end   = have_end ? end : -1;
    }
}

/*
 * Read and parse one HTTP request head from the socket. Returns true if a
 * complete head was read and parsed, false on connection close or error.
 * Leaves any body unread; asset GETs have no body.
 */
static bool read_request(asset_socket_t sock, asset_request *req)
{
    char   head[ASSET_REQ_HEAD_MAX + 1];
    size_t len = 0;

    memset(req, 0, sizeof(*req));
    req->range_start = 0;
    req->range_end   = -1;

    /* Read until the end-of-headers marker. */
    bool got_head = false;
    while (len < ASSET_REQ_HEAD_MAX) {
        int64_t n = blocking_recv(sock, head + len, ASSET_REQ_HEAD_MAX - len);
        if (n <= 0)
            return false;                /* closed or error */
        len += (size_t)n;
        head[len] = '\0';
        if (strstr(head, "\r\n\r\n") != NULL) {
            got_head = true;
            break;
        }
    }
    if (!got_head)
        return false;

    /* --- request line: METHOD SP target SP version --- */
    char *line_end = strstr(head, "\r\n");
    if (!line_end)
        return false;
    *line_end = '\0';

    char *method = head;
    char *sp1 = strchr(method, ' ');
    if (!sp1)
        return false;
    *sp1 = '\0';
    char *target = sp1 + 1;
    char *sp2 = strchr(target, ' ');
    if (sp2)
        *sp2 = '\0';

    req->is_head = (strcmp(method, "HEAD") == 0);
    req->is_get  = (strcmp(method, "GET") == 0) || req->is_head;

    /* Strip a query string from the target, assets ignore it. */
    char *q = strchr(target, '?');
    if (q)
        *q = '\0';

    /* The target must start with the asset prefix. */
    if (strncmp(target, ASSET_PREFIX, ASSET_PREFIX_LEN) != 0) {
        req->valid = false;
    } else {
        char decoded[1024];
        url_decode(target + ASSET_PREFIX_LEN, decoded, sizeof(decoded));
        if (name_is_safe(decoded)) {
            strncpy(req->name, decoded, sizeof(req->name) - 1);
            req->name[sizeof(req->name) - 1] = '\0';
            req->valid = true;
        } else {
            req->valid = false;
        }
    }

    /* --- headers (after the request line, before the blank line) --- */
    char *h = line_end + 2;
    char *headers_end = strstr(h, "\r\n\r\n");
    req->keep_alive = true;   /* default for HTTP/1.1 */
    while (h && headers_end && h < headers_end) {
        char *eol = strstr(h, "\r\n");
        if (!eol || eol > headers_end)
            break;
        *eol = '\0';

        char *colon = strchr(h, ':');
        if (colon) {
            *colon = '\0';
            char *val = colon + 1;
            while (*val == ' ' || *val == '\t') val++;
            if (strcasecmp(h, "Range") == 0) {
                parse_range(val, req);
            } else if (strcasecmp(h, "Connection") == 0) {
                if (strcasecmp(val, "close") == 0)
                    req->keep_alive = false;
            }
        }
        h = eol + 2;
    }

    return true;
}

/* =========================================================================
 * Response helpers
 * ====================================================================== */

static void send_simple(asset_socket_t sock, int code, const char *reason,
                        bool keep_alive)
{
    char body[256];
    int  body_len = snprintf(body, sizeof(body),
                             "<html><body><h1>%d %s</h1></body></html>",
                             code, reason);
    char head[512];
    int  head_len = snprintf(head, sizeof(head),
        "HTTP/1.1 %d %s\r\n"
        "Content-Type: text/html; charset=utf-8\r\n"
        "Content-Length: %d\r\n"
        "Connection: %s\r\n"
        "\r\n",
        code, reason, body_len, keep_alive ? "keep-alive" : "close");
    if (head_len > 0)
        send_all(sock, head, (size_t)head_len);
    if (body_len > 0)
        send_all(sock, body, (size_t)body_len);
}

/*
 * Stream an open asset to the socket, honoring an optional byte range.
 * head_only sends headers without the body (HTTP HEAD). Returns 0 on success.
 */
static int serve_asset_stream(asset_socket_t sock, peko_asset *asset,
                              const char *mime, asset_request *req,
                              bool head_only)
{
    int64_t total = peko_asset_size(asset);
    if (total < 0)
        total = 0;

    int64_t start = 0;
    int64_t end   = total - 1;   /* inclusive */
    bool    partial = false;

    if (req->has_range) {
        if (req->range_start < 0) {
            /* suffix: last N bytes */
            int64_t n = -req->range_start;
            if (n > total) n = total;
            start = total - n;
            end   = total - 1;
        } else {
            start = req->range_start;
            end   = (req->range_end >= 0) ? req->range_end : total - 1;
        }
        if (end > total - 1)
            end = total - 1;

        /* Unsatisfiable range. */
        if (total == 0 || start >= total || start > end) {
            char head[256];
            int  hl = snprintf(head, sizeof(head),
                "HTTP/1.1 416 Range Not Satisfiable\r\n"
                "Content-Range: bytes */%lld\r\n"
                "Content-Length: 0\r\n"
                "Connection: %s\r\n"
                "\r\n",
                (long long)total, req->keep_alive ? "keep-alive" : "close");
            if (hl > 0)
                send_all(sock, head, (size_t)hl);
            return 0;
        }
        partial = true;
    }

    int64_t content_length = end - start + 1;

    char head[512];
    int  hl;
    if (partial) {
        hl = snprintf(head, sizeof(head),
            "HTTP/1.1 206 Partial Content\r\n"
            "Content-Type: %s\r\n"
            "Accept-Ranges: bytes\r\n"
            "Content-Range: bytes %lld-%lld/%lld\r\n"
            "Content-Length: %lld\r\n"
            "Connection: %s\r\n"
            "\r\n",
            mime, (long long)start, (long long)end, (long long)total,
            (long long)content_length,
            req->keep_alive ? "keep-alive" : "close");
    } else {
        hl = snprintf(head, sizeof(head),
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: %s\r\n"
            "Accept-Ranges: bytes\r\n"
            "Content-Length: %lld\r\n"
            "Connection: %s\r\n"
            "\r\n",
            mime, (long long)content_length,
            req->keep_alive ? "keep-alive" : "close");
    }
    if (hl <= 0)
        return -1;
    if (send_all(sock, head, (size_t)hl) != 0)
        return -1;

    if (head_only)
        return 0;

    /* Stream the body a chunk at a time straight from the asset handle. The
     * buffer is plain C heap, so nothing managed is held across the blocking
     * sends below. */
    unsigned char *chunk = (unsigned char *)malloc(ASSET_STREAM_CHUNK);
    if (!chunk)
        return -1;

    int64_t remaining = content_length;
    int64_t offset    = start;
    int     rc        = 0;
    while (remaining > 0) {
        int64_t want = remaining < (int64_t)ASSET_STREAM_CHUNK
                         ? remaining : (int64_t)ASSET_STREAM_CHUNK;
        int64_t got  = peko_asset_read(asset, offset, want, chunk);
        if (got <= 0) {
            rc = -1;
            break;
        }
        if (send_all(sock, chunk, (size_t)got) != 0) {
            rc = -1;
            break;
        }
        offset    += got;
        remaining -= got;
    }

    free(chunk);
    return rc;
}

/*
 * Serve a small asset from a caller-owned byte buffer (the cache copy),
 * honoring Range. Frees nothing; the caller owns bytes.
 */
static int serve_asset_buffer(asset_socket_t sock, const unsigned char *bytes,
                              size_t total_len, const char *mime,
                              asset_request *req, bool head_only)
{
    int64_t total = (int64_t)total_len;
    int64_t start = 0, end = total - 1;
    bool    partial = false;

    if (req->has_range) {
        if (req->range_start < 0) {
            int64_t n = -req->range_start;
            if (n > total) n = total;
            start = total - n;
            end   = total - 1;
        } else {
            start = req->range_start;
            end   = (req->range_end >= 0) ? req->range_end : total - 1;
        }
        if (end > total - 1)
            end = total - 1;
        if (total == 0 || start >= total || start > end) {
            char head[256];
            int  hl = snprintf(head, sizeof(head),
                "HTTP/1.1 416 Range Not Satisfiable\r\n"
                "Content-Range: bytes */%lld\r\n"
                "Content-Length: 0\r\n"
                "Connection: %s\r\n"
                "\r\n",
                (long long)total, req->keep_alive ? "keep-alive" : "close");
            if (hl > 0)
                send_all(sock, head, (size_t)hl);
            return 0;
        }
        partial = true;
    }

    int64_t content_length = end - start + 1;
    char head[512];
    int  hl;
    if (partial) {
        hl = snprintf(head, sizeof(head),
            "HTTP/1.1 206 Partial Content\r\n"
            "Content-Type: %s\r\n"
            "Accept-Ranges: bytes\r\n"
            "Content-Range: bytes %lld-%lld/%lld\r\n"
            "Content-Length: %lld\r\n"
            "Connection: %s\r\n"
            "\r\n",
            mime, (long long)start, (long long)end, (long long)total,
            (long long)content_length,
            req->keep_alive ? "keep-alive" : "close");
    } else {
        hl = snprintf(head, sizeof(head),
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: %s\r\n"
            "Accept-Ranges: bytes\r\n"
            "Content-Length: %lld\r\n"
            "Connection: %s\r\n"
            "\r\n",
            mime, (long long)content_length,
            req->keep_alive ? "keep-alive" : "close");
    }
    if (hl <= 0)
        return -1;
    if (send_all(sock, head, (size_t)hl) != 0)
        return -1;
    if (head_only)
        return 0;

    return send_all(sock, bytes + start, (size_t)content_length);
}

/* =========================================================================
 * Connection handling
 * ====================================================================== */

/* Whether the last path segment of name contains a dot, so it looks like a
 * file rather than a client-side route. An empty name has no extension. */
static int name_has_extension(const char *name)
{
    const char *slash   = strrchr(name, '/');
    const char *segment = slash ? slash + 1 : name;
    return strchr(segment, '.') != NULL;
}

static void handle_connection(asset_socket_t sock)
{
    for (;;) {
        asset_request req;
        if (!read_request(sock, &req))
            break;                       /* peer closed or bad head */

        if (!req.is_get || !req.valid) {
            send_simple(sock, req.valid ? 405 : 404,
                        req.valid ? "Method Not Allowed" : "Not Found",
                        req.keep_alive);
            if (!req.keep_alive)
                break;
            continue;
        }

        bool head_only = req.is_head;

        /* Try the cache first (small assets). */
        size_t         clen = 0;
        const char    *cmime = NULL;
        unsigned char *cbytes = cache_get_copy(req.name, &clen, &cmime);
        if (cbytes) {
            serve_asset_buffer(sock, cbytes, clen, cmime, &req, head_only);
            free(cbytes);
            if (!req.keep_alive)
                break;
            continue;
        }

        /* Open from the bundle or the debug directory. */
        peko_asset *asset = (g_server.debug_dir && g_server.debug_dir[0])
                              ? peko_asset_open_dir(g_server.debug_dir, req.name)
                              : peko_asset_open(req.name);

        /* Single-page-app fallback: a request for a path with no file extension
         * is a client-side route, not a file, so serve index.html and let the
         * app router resolve it. This makes history-mode routes deep-link and
         * reload correctly. */
        const char *serve_name = req.name;
        if (!asset && !name_has_extension(req.name)) {
            serve_name = "index.html";
            unsigned char *ibytes = cache_get_copy(serve_name, &clen, &cmime);
            if (ibytes) {
                serve_asset_buffer(sock, ibytes, clen, cmime, &req, head_only);
                free(ibytes);
                if (!req.keep_alive)
                    break;
                continue;
            }
            asset = (g_server.debug_dir && g_server.debug_dir[0])
                      ? peko_asset_open_dir(g_server.debug_dir, serve_name)
                      : peko_asset_open(serve_name);
        }
        if (!asset) {
            send_simple(sock, 404, "Not Found", req.keep_alive);
            if (!req.keep_alive)
                break;
            continue;
        }

        const char *mime  = peko_asset_mime_type(serve_name);
        int64_t     total = peko_asset_size(asset);

        /* Small and not ranged: read once, cache, serve from the copy.
         * Large or ranged: stream straight from the handle (never cached). */
        if (!req.has_range && total >= 0 &&
            (size_t)total <= ASSET_CACHE_ENTRY_MAX) {
            unsigned char *buf = (unsigned char *)malloc(total ? (size_t)total : 1);
            if (buf) {
                int64_t got = peko_asset_read(asset, 0, total, buf);
                if (got == total) {
                    cache_put(serve_name, buf, (size_t)total, mime);
                    serve_asset_buffer(sock, buf, (size_t)total, mime,
                                       &req, head_only);
                } else {
                    send_simple(sock, 500, "Internal Server Error",
                                req.keep_alive);
                }
                free(buf);
            } else {
                /* Allocation failed: fall back to streaming. */
                serve_asset_stream(sock, asset, mime, &req, head_only);
            }
        } else {
            serve_asset_stream(sock, asset, mime, &req, head_only);
        }

        peko_asset_close(asset);

        if (!req.keep_alive)
            break;
    }

    asset_close_socket(sock);
}

/* =========================================================================
 * Listener thread
 * ====================================================================== */

/* Each accepted connection runs on its own detached thread. A browser opens
 * several connections in parallel to fetch a page and its subresources, and
 * keep-alive holds each open. Serving connections one at a time would block
 * every parallel connection behind the first one's keep-alive read loop, so
 * subresources such as stylesheets would never be fetched. This thread touches
 * no managed memory but makes blocking socket calls, so it attaches for the
 * blocking brackets in handle_connection. handle_connection closes the socket. */
#ifdef _WIN32
static DWORD WINAPI connection_thread(LPVOID arg)
#else
static void *connection_thread(void *arg)
#endif
{
    asset_socket_t sock = (asset_socket_t)(intptr_t)arg;
    pgc_thread_attach();
    handle_connection(sock);
    pgc_thread_detach();
#ifdef _WIN32
    return 0;
#else
    return NULL;
#endif
}

/* Spawn a detached thread to serve one connection. On spawn failure the
 * connection is served inline so it is not dropped. */
static void spawn_connection(asset_socket_t sock)
{
#ifdef _WIN32
    HANDLE t = CreateThread(NULL, 0, connection_thread,
                            (LPVOID)(intptr_t)sock, 0, NULL);
    if (t) {
        CloseHandle(t);
        return;
    }
#else
    pthread_t t;
    if (pthread_create(&t, NULL, connection_thread,
                       (void *)(intptr_t)sock) == 0) {
        pthread_detach(t);
        return;
    }
#endif
    handle_connection(sock);
}

#ifdef _WIN32
static DWORD WINAPI server_thread_main(LPVOID arg)
#else
static void *server_thread_main(void *arg)
#endif
{
    (void)arg;

    /* This thread touches no managed memory, but it makes blocking socket
     * calls; attach it so the collector can account for it and so the
     * blocking brackets behave like the rest of the runtime. */
    pgc_thread_attach();

    while (atomic_load(&g_server.running)) {
        asset_socket_t client = blocking_accept(g_server.listen_sock);
        if (client == ASSET_INVALID_SOCKET) {
            if (!atomic_load(&g_server.running))
                break;
            continue;                    /* transient accept error */
        }
        spawn_connection(client);
    }

    pgc_thread_detach();
#ifdef _WIN32
    return 0;
#else
    return NULL;
#endif
}

/* =========================================================================
 * Listen socket creation (loopback only)
 * ====================================================================== */

static asset_socket_t create_loopback_listener(int *out_port)
{
    asset_socket_t sock = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (sock == ASSET_INVALID_SOCKET)
        return ASSET_INVALID_SOCKET;

    int yes = 1;
    setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, (const char *)&yes, sizeof(yes));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family      = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);   /* 127.0.0.1 only */
    addr.sin_port        = htons(0);                 /* dynamic port */

    if (bind(sock, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        asset_close_socket(sock);
        return ASSET_INVALID_SOCKET;
    }

    struct sockaddr_in bound;
    socklen_t          blen = sizeof(bound);
    if (getsockname(sock, (struct sockaddr *)&bound, &blen) != 0) {
        asset_close_socket(sock);
        return ASSET_INVALID_SOCKET;
    }
    *out_port = (int)ntohs(bound.sin_port);

    if (listen(sock, 64) != 0) {
        asset_close_socket(sock);
        return ASSET_INVALID_SOCKET;
    }
    return sock;
}

/* =========================================================================
 * Public API
 * ====================================================================== */

int peko_asset_server_start(const char *debug_dir)
{
    /* Start once. */
    int expected = 0;
    if (!atomic_compare_exchange_strong(&g_started, &expected, 1))
        return g_server.port;

    cache_init();
    memset(&g_server, 0, sizeof(g_server));

    int            port = 0;
    asset_socket_t sock = create_loopback_listener(&port);
    if (sock == ASSET_INVALID_SOCKET) {
        atomic_store(&g_started, 0);
        return 0;
    }

    g_server.listen_sock = sock;
    g_server.port        = port;
    if (debug_dir && debug_dir[0]) {
        size_t n = strlen(debug_dir) + 1;
        g_server.debug_dir = (char *)malloc(n);
        if (g_server.debug_dir)
            memcpy(g_server.debug_dir, debug_dir, n);
    }
    atomic_store(&g_server.running, 1);

#ifdef _WIN32
    g_server.thread = CreateThread(NULL, 0, server_thread_main, NULL, 0, NULL);
    if (g_server.thread == NULL) {
        atomic_store(&g_server.running, 0);
        asset_close_socket(g_server.listen_sock);
        free(g_server.debug_dir);
        g_server.debug_dir = NULL;
        atomic_store(&g_started, 0);
        return 0;
    }
#else
    if (pthread_create(&g_server.thread, NULL, server_thread_main, NULL) != 0) {
        atomic_store(&g_server.running, 0);
        asset_close_socket(g_server.listen_sock);
        free(g_server.debug_dir);
        g_server.debug_dir = NULL;
        atomic_store(&g_started, 0);
        return 0;
    }
#endif

    return g_server.port;
}

int peko_asset_server_port(void)
{
    return atomic_load(&g_started) ? g_server.port : 0;
}

void peko_asset_server_stop(void)
{
    if (!atomic_load(&g_started))
        return;

    atomic_store(&g_server.running, 0);

    /* Closing the listen socket unblocks the accept in the server thread. */
    asset_close_socket(g_server.listen_sock);

#ifdef _WIN32
    if (g_server.thread) {
        WaitForSingleObject(g_server.thread, INFINITE);
        CloseHandle(g_server.thread);
    }
#else
    pthread_join(g_server.thread, NULL);
#endif

    free(g_server.debug_dir);
    g_server.debug_dir = NULL;
    cache_clear();

    atomic_store(&g_started, 0);
}
