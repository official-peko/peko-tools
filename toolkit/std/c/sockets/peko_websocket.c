/*
 * peko_websocket.c
 * WebSocket server implementation per RFC 6455.
 *
 * Handles:
 *   - HTTP upgrade handshake (Section 4)
 *   - Frame decoding with masking (Section 5.2, 5.3)
 *   - Fragmented message reassembly (Section 5.4)
 *   - Control frames: Ping/Pong/Close (Section 5.5)
 *   - Frame encoding for server->client (unmasked, Section 5.1)
 *
 * SHA-1 derived from RFC 3174 reference code.
 * Base64 is standard MIME alphabet (no line breaks).
 */

#include "peko_sockets.h"
#include <pgc.h>
#include <string.h>
#include <signal.h>

/* Per-connection send mutex - serializes peko_ws_send_text calls from
 * the write thread against internal ping/close sends from the recv loop.
 * Without this, interleaved bytes from concurrent sends corrupt WebSocket
 * frames and cause the browser to close the connection. */
#ifdef _WIN32
#  include <windows.h>
typedef CRITICAL_SECTION ws_send_mutex_t;
static void ws_send_mutex_init(ws_send_mutex_t *m)  { InitializeCriticalSection(m); }
static void ws_send_mutex_lock(ws_send_mutex_t *m)  { EnterCriticalSection(m); }
static void ws_send_mutex_unlock(ws_send_mutex_t *m){ LeaveCriticalSection(m); }
static void ws_send_mutex_destroy(ws_send_mutex_t *m){ DeleteCriticalSection(m); }
#else
#  include <pthread.h>
typedef pthread_mutex_t ws_send_mutex_t;
static void ws_send_mutex_init(ws_send_mutex_t *m)  { pthread_mutex_init(m, NULL); }
static void ws_send_mutex_lock(ws_send_mutex_t *m)  { pthread_mutex_lock(m); }
static void ws_send_mutex_unlock(ws_send_mutex_t *m){ pthread_mutex_unlock(m); }
static void ws_send_mutex_destroy(ws_send_mutex_t *m){ pthread_mutex_destroy(m); }
#endif


/* =========================================================================
 * Global socket send-mutex table
 * Maps socket fd -> send mutex so peko_ws_send_text (called from the write
 * thread) uses the same mutex as the internal sends in peko_ws_accept_connection.
 * ====================================================================== */

#define WS_MAX_CONNECTIONS 1024

typedef struct {
    peko_socket_t   fd;
    ws_send_mutex_t mutex;
    int             in_use;
    int             refcount;  /* held by each active ws_locked_send call */
} ws_conn_entry_t;

static ws_conn_entry_t  g_ws_conns[WS_MAX_CONNECTIONS];
static ws_send_mutex_t  g_ws_table_lock;
static int              g_ws_table_init = 0;

#ifndef _WIN32
static void ws_table_do_init(void)
{
    ws_send_mutex_init(&g_ws_table_lock);
    memset(g_ws_conns, 0, sizeof(g_ws_conns));
}
#endif

static void ws_table_ensure_init(void)
{
#ifdef _WIN32
    if (InterlockedCompareExchange((LONG volatile*)&g_ws_table_init, 1, 0) == 0) {
        ws_send_mutex_init(&g_ws_table_lock);
        memset(g_ws_conns, 0, sizeof(g_ws_conns));
    }
#else
    static pthread_once_t once = PTHREAD_ONCE_INIT;
    pthread_once(&once, ws_table_do_init);
#endif
}

static ws_send_mutex_t *ws_conn_register(peko_socket_t fd)
{
    ws_table_ensure_init();
    ws_send_mutex_lock(&g_ws_table_lock);
    for (int i = 0; i < WS_MAX_CONNECTIONS; i++) {
        if (!g_ws_conns[i].in_use) {
            g_ws_conns[i].fd       = fd;
            g_ws_conns[i].in_use   = 1;
            g_ws_conns[i].refcount = 0;
            ws_send_mutex_init(&g_ws_conns[i].mutex);
            ws_send_mutex_unlock(&g_ws_table_lock);
            return &g_ws_conns[i].mutex;
        }
    }
    ws_send_mutex_unlock(&g_ws_table_lock);
    return NULL; /* table full */
}

/* Acquire a reference to the connection entry for fd.
 * Returns the entry index (>=0) with refcount incremented, or -1 if not found.
 * Caller must call ws_conn_release() when done. */
static int ws_conn_acquire(peko_socket_t fd)
{
    ws_table_ensure_init();
    ws_send_mutex_lock(&g_ws_table_lock);
    for (int i = 0; i < WS_MAX_CONNECTIONS; i++) {
        if (g_ws_conns[i].in_use && g_ws_conns[i].fd == fd) {
            g_ws_conns[i].refcount++;
            ws_send_mutex_unlock(&g_ws_table_lock);
            return i;
        }
    }
    ws_send_mutex_unlock(&g_ws_table_lock);
    return -1;
}

/* Release a reference acquired by ws_conn_acquire.
 * If the entry was unregistered while we held a ref, destroy it now. */
static void ws_conn_release(int idx)
{
    if (idx < 0) return;
    ws_send_mutex_lock(&g_ws_table_lock);
    g_ws_conns[idx].refcount--;
    if (!g_ws_conns[idx].in_use && g_ws_conns[idx].refcount == 0) {
        ws_send_mutex_destroy(&g_ws_conns[idx].mutex);
        memset(&g_ws_conns[idx], 0, sizeof(ws_conn_entry_t));
    }
    ws_send_mutex_unlock(&g_ws_table_lock);
}

static void ws_conn_unregister(peko_socket_t fd)
{
    ws_table_ensure_init();
    ws_send_mutex_lock(&g_ws_table_lock);
    for (int i = 0; i < WS_MAX_CONNECTIONS; i++) {
        if (g_ws_conns[i].in_use && g_ws_conns[i].fd == fd) {
            g_ws_conns[i].in_use = 0;
            g_ws_conns[i].fd     = 0;
            if (g_ws_conns[i].refcount == 0) {
                /* No active senders - safe to destroy immediately */
                ws_send_mutex_destroy(&g_ws_conns[i].mutex);
                memset(&g_ws_conns[i], 0, sizeof(ws_conn_entry_t));
            }
            /* If refcount > 0, ws_conn_release will destroy it when last
             * sender finishes. */
            break;
        }
    }
    ws_send_mutex_unlock(&g_ws_table_lock);
}

/* =========================================================================
 * Locked send helper
 * All sends on a WebSocket connection go through ws_locked_send to prevent
 * concurrent writes from interleaving frame bytes.
 * ====================================================================== */

static int ws_locked_send(peko_socket_t sock, ws_send_mutex_t *mu,
                          const char *data, size_t len)
{
    /* Bracket the mutex acquisition so the GC can proceed if this thread
     * blocks waiting for another send to complete. */
    pgc_begin_blocking();
    ws_send_mutex_lock(mu);
    pgc_end_blocking();
    size_t sent = 0;
    int    rc   = 0;
    while (sent < len) {
        /* peko_send (send syscall) can block when the kernel TCP send
         * buffer is full. Declare parked so GC collections are not
         * blocked waiting for this thread to reach a safepoint. */
        pgc_begin_blocking();
        int n = (int)peko_send(sock, data + sent, len - sent);
        pgc_end_blocking();
        if (n <= 0) { rc = -1; break; }
        sent += (size_t)n;
    }
    ws_send_mutex_unlock(mu);
    return rc;
}

/* =========================================================================
 * SHA-1  (RFC 3174)
 * ====================================================================== */

#define SHA1_HASH_SIZE 20

typedef struct {
    uint32_t      intermediate[SHA1_HASH_SIZE / 4];
    uint32_t      length_low;
    uint32_t      length_high;
    int_least16_t block_index;
    uint8_t       block[64];
    int           computed;
    int           corrupted;
} sha1_ctx_t;

#define SHA1_ROTL(bits, word) \
    (((word) << (bits)) | ((word) >> (32 - (bits))))

static void sha1_process_block(sha1_ctx_t *ctx);
static void sha1_pad(sha1_ctx_t *ctx);

static void sha1_reset(sha1_ctx_t *ctx)
{
    ctx->length_low  = 0;
    ctx->length_high = 0;
    ctx->block_index = 0;
    ctx->computed    = 0;
    ctx->corrupted   = 0;

    ctx->intermediate[0] = 0x67452301;
    ctx->intermediate[1] = 0xEFCDAB89;
    ctx->intermediate[2] = 0x98BADCFE;
    ctx->intermediate[3] = 0x10325476;
    ctx->intermediate[4] = 0xC3D2E1F0;
}

static void sha1_input(sha1_ctx_t *ctx, const uint8_t *data, unsigned len)
{
    if (!len || !ctx || !data || ctx->computed || ctx->corrupted)
        return;

    while (len--) {
        ctx->block[ctx->block_index++] = (*data & 0xFF);
        ctx->length_low += 8;
        if (ctx->length_low == 0) {
            ctx->length_high++;
            if (ctx->length_high == 0)
                ctx->corrupted = 1;
        }
        if (ctx->block_index == 64)
            sha1_process_block(ctx);
        data++;
    }
}

static void sha1_result(sha1_ctx_t *ctx, uint8_t digest[SHA1_HASH_SIZE])
{
    int i;
    if (!ctx->computed) {
        sha1_pad(ctx);
        for (i = 0; i < 64; i++)
            ctx->block[i] = 0;
        ctx->length_low  = 0;
        ctx->length_high = 0;
        ctx->computed    = 1;
    }
    for (i = 0; i < SHA1_HASH_SIZE; i++)
        digest[i] = (uint8_t)(ctx->intermediate[i >> 2] >> (8 * (3 - (i & 3))));
}

static void sha1_process_block(sha1_ctx_t *ctx)
{
    static const uint32_t K[4] = {
        0x5A827999, 0x6ED9EBA1, 0x8F1BBCDC, 0xCA62C1D6
    };
    uint32_t W[80];
    uint32_t A, B, C, D, E, temp;
    int      t;

    for (t = 0; t < 16; t++) {
        W[t]  = (uint32_t)ctx->block[t * 4]     << 24;
        W[t] |= (uint32_t)ctx->block[t * 4 + 1] << 16;
        W[t] |= (uint32_t)ctx->block[t * 4 + 2] << 8;
        W[t] |= (uint32_t)ctx->block[t * 4 + 3];
    }
    for (t = 16; t < 80; t++)
        W[t] = SHA1_ROTL(1, W[t-3] ^ W[t-8] ^ W[t-14] ^ W[t-16]);

    A = ctx->intermediate[0];
    B = ctx->intermediate[1];
    C = ctx->intermediate[2];
    D = ctx->intermediate[3];
    E = ctx->intermediate[4];

    for (t = 0; t < 20; t++) {
        temp = SHA1_ROTL(5,A) + ((B&C)|((~B)&D)) + E + W[t] + K[0];
        E=D; D=C; C=SHA1_ROTL(30,B); B=A; A=temp;
    }
    for (t = 20; t < 40; t++) {
        temp = SHA1_ROTL(5,A) + (B^C^D) + E + W[t] + K[1];
        E=D; D=C; C=SHA1_ROTL(30,B); B=A; A=temp;
    }
    for (t = 40; t < 60; t++) {
        temp = SHA1_ROTL(5,A) + ((B&C)|(B&D)|(C&D)) + E + W[t] + K[2];
        E=D; D=C; C=SHA1_ROTL(30,B); B=A; A=temp;
    }
    for (t = 60; t < 80; t++) {
        temp = SHA1_ROTL(5,A) + (B^C^D) + E + W[t] + K[3];
        E=D; D=C; C=SHA1_ROTL(30,B); B=A; A=temp;
    }

    ctx->intermediate[0] += A;
    ctx->intermediate[1] += B;
    ctx->intermediate[2] += C;
    ctx->intermediate[3] += D;
    ctx->intermediate[4] += E;
    ctx->block_index = 0;
}

static void sha1_pad(sha1_ctx_t *ctx)
{
    if (ctx->block_index > 55) {
        ctx->block[ctx->block_index++] = 0x80;
        while (ctx->block_index < 64)
            ctx->block[ctx->block_index++] = 0;
        sha1_process_block(ctx);
        while (ctx->block_index < 56)
            ctx->block[ctx->block_index++] = 0;
    } else {
        ctx->block[ctx->block_index++] = 0x80;
        while (ctx->block_index < 56)
            ctx->block[ctx->block_index++] = 0;
    }

    ctx->block[56] = (uint8_t)(ctx->length_high >> 24);
    ctx->block[57] = (uint8_t)(ctx->length_high >> 16);
    ctx->block[58] = (uint8_t)(ctx->length_high >>  8);
    ctx->block[59] = (uint8_t)(ctx->length_high      );
    ctx->block[60] = (uint8_t)(ctx->length_low  >> 24);
    ctx->block[61] = (uint8_t)(ctx->length_low  >> 16);
    ctx->block[62] = (uint8_t)(ctx->length_low  >>  8);
    ctx->block[63] = (uint8_t)(ctx->length_low       );

    sha1_process_block(ctx);
}

/* =========================================================================
 * Base64 encode (standard MIME alphabet, no line breaks)
 * Returns malloc'd null-terminated string. Caller must free().
 * ====================================================================== */

static const char B64_ALPHA[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

static char *base64_encode(const uint8_t *src, size_t len)
{
    size_t  olen = 4 * ((len + 2) / 3) + 1;
    char   *out  = (char *)malloc(olen);
    char   *pos;
    const uint8_t *in  = src;
    const uint8_t *end = src + len;

    if (!out)
        return NULL;

    pos = out;
    while (end - in >= 3) {
        *pos++ = B64_ALPHA[in[0] >> 2];
        *pos++ = B64_ALPHA[((in[0] & 0x03) << 4) | (in[1] >> 4)];
        *pos++ = B64_ALPHA[((in[1] & 0x0F) << 2) | (in[2] >> 6)];
        *pos++ = B64_ALPHA[  in[2] & 0x3F];
        in += 3;
    }
    if (end - in == 2) {
        *pos++ = B64_ALPHA[in[0] >> 2];
        *pos++ = B64_ALPHA[((in[0] & 0x03) << 4) | (in[1] >> 4)];
        *pos++ = B64_ALPHA[(in[1] & 0x0F) << 2];
        *pos++ = '=';
    } else if (end - in == 1) {
        *pos++ = B64_ALPHA[in[0] >> 2];
        *pos++ = B64_ALPHA[(in[0] & 0x03) << 4];
        *pos++ = '=';
        *pos++ = '=';
    }
    *pos = '\0';
    return out;
}

/* =========================================================================
 * WebSocket handshake (RFC 6455 Section 4)
 * ====================================================================== */

#define WS_MAGIC "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
/* Sec-WebSocket-Key is always 24 base64 characters */
#define WS_KEY_LEN 24

/*
 * Computes Sec-WebSocket-Accept and returns a complete HTTP 101 response
 * in a malloc'd buffer. Caller must free(). Returns NULL on failure.
 *
 * RFC 6455 s4.2.2: accept = base64( SHA-1( key + WS_MAGIC ) )
 */
static char *ws_build_handshake(const char *client_key)
{
    /* key (24) + magic (36) + NUL */
    char     combined[WS_KEY_LEN + sizeof(WS_MAGIC)];
    uint8_t  hash[SHA1_HASH_SIZE];
    char    *accept_key;
    char    *response;
    size_t   response_len;
    sha1_ctx_t ctx;

    snprintf(combined, sizeof(combined), "%.*s%s", WS_KEY_LEN, client_key,
             WS_MAGIC);
    sha1_reset(&ctx);
    sha1_input(&ctx, (const uint8_t *)combined, (unsigned)strlen(combined));
    sha1_result(&ctx, hash);

    accept_key = base64_encode(hash, SHA1_HASH_SIZE);
    if (!accept_key)
        return NULL;

    response_len = 256 + strlen(accept_key);
    response     = (char *)malloc(response_len);
    if (!response) {
        free(accept_key);
        return NULL;
    }

    /* RFC 6455 s4.2.2: required response headers */
    snprintf(response, response_len,
             "HTTP/1.1 101 Switching Protocols\r\n"
             "Upgrade: websocket\r\n"
             "Connection: Upgrade\r\n"
             "Sec-WebSocket-Accept: %s\r\n\r\n",
             accept_key);

    free(accept_key);
    return response;
}

/*
 * Extracts the Sec-WebSocket-Key value from an HTTP upgrade request.
 * *key_out points into buf (do not free separately).
 * Returns 0 on success, -1 if not found.
 */
static int ws_extract_key(char *buf, char **key_out)
{
    static const char HEADER[] = "Sec-WebSocket-Key:";
    char *p = strstr(buf, HEADER);
    if (!p)
        return -1;

    p += sizeof(HEADER) - 1;
    while (*p == ' ')
        p++;

    *key_out = p;

    /* Null-terminate at end of line (handle \r\n and \n) */
    while (*p && *p != '\r' && *p != '\n')
        p++;
    *p = '\0';

    return 0;
}

/* =========================================================================
 * Frame encoding (server -> client, RFC 6455 Section 5.2)
 *
 * Server MUST NOT mask frames sent to client (RFC 6455 s5.1).
 * FIN=1, RSV1-3=0, opcode in low nibble of byte 0.
 * Payload length: 0-125 direct, 126 -> 2-byte extended, 127 -> 8-byte.
 * ====================================================================== */

static char *ws_encode_frame(const char *text, size_t text_len,
                             size_t *frame_len)
{
    char  header[10];
    int   header_len;
    char *frame;

    /* FIN=1, RSV=0, opcode=text(1) */
    header[0] = (char)(0x80 | WS_OPCODE_TXT);

    if (text_len <= 125) {
        /* RFC 6455 s5.2: 7-bit length, mask bit=0 (server must not mask) */
        header[1]  = (char)(text_len & 0x7F);
        header_len = 2;
    } else if (text_len <= 65535) {
        /* 16-bit extended payload length */
        header[1]  = 126;
        header[2]  = (char)((text_len >> 8) & 0xFF);
        header[3]  = (char)( text_len       & 0xFF);
        header_len = 4;
    } else {
        /* 64-bit extended payload length */
        header[1]  = 127;
        header[2]  = (char)((text_len >> 56) & 0xFF);
        header[3]  = (char)((text_len >> 48) & 0xFF);
        header[4]  = (char)((text_len >> 40) & 0xFF);
        header[5]  = (char)((text_len >> 32) & 0xFF);
        header[6]  = (char)((text_len >> 24) & 0xFF);
        header[7]  = (char)((text_len >> 16) & 0xFF);
        header[8]  = (char)((text_len >>  8) & 0xFF);
        header[9]  = (char)( text_len        & 0xFF);
        header_len = 10;
    }

    *frame_len = (size_t)header_len + text_len;
    frame      = (char *)malloc(*frame_len);
    if (!frame)
        return NULL;

    memcpy(frame,              header, (size_t)header_len);
    memcpy(frame + header_len, text,   text_len);
    return frame;
}

/*
 * Encodes a Close frame (RFC 6455 s5.5.1).
 * Optionally includes a 2-byte status code in network byte order.
 */
static char *ws_encode_close(uint16_t code, size_t *frame_len)
{
    char *frame = (char *)malloc(4);
    if (!frame)
        return NULL;
    frame[0] = (char)(0x80 | WS_OPCODE_CLOSE);
    frame[1] = 2; /* payload length = 2 (status code only) */
    frame[2] = (char)((code >> 8) & 0xFF);
    frame[3] = (char)( code       & 0xFF);
    *frame_len = 4;
    return frame;
}

/*
 * Encodes a Pong frame (RFC 6455 s5.5.3).
 * Must echo the ping payload verbatim.
 */
static char *ws_encode_pong(const char *payload, size_t payload_len,
                            size_t *frame_len)
{
    char *frame;

    /* RFC 6455 s5.5: control frames MUST have payload <= 125 bytes */
    if (payload_len > 125)
        payload_len = 125;

    *frame_len = 2 + payload_len;
    frame      = (char *)malloc(*frame_len);
    if (!frame)
        return NULL;

    frame[0] = (char)(0x80 | WS_OPCODE_PONG);
    frame[1] = (char)(payload_len & 0x7F); /* mask bit=0 */
    if (payload_len > 0)
        memcpy(frame + 2, payload, payload_len);
    return frame;
}

/* =========================================================================
 * Frame decoding (RFC 6455 Section 5.2, 5.3)
 *
 * ws_parse_frame_header: reads frame header fields without touching payload.
 * ws_unmask_payload: XORs payload bytes with the 4-byte masking key.
 *
 * These are kept separate: ws_recv_one_frame checks completeness without
 * unmasking, then ws_unmask_payload is called exactly once per frame.
 * ====================================================================== */

typedef struct {
    uint8_t  opcode;
    int      fin;       /* FIN bit */
    int      masked;
    uint8_t  mask[4];
    size_t   header_len;
    size_t   payload_len;
} ws_frame_header_t;

/*
 * Parses the frame header from data[0..data_len). Does NOT unmask.
 * Returns 1 if a complete frame is present, 0 if more data is needed.
 */
static int ws_parse_frame_header(const uint8_t *data, size_t data_len,
                                 ws_frame_header_t *hdr)
{
    size_t i;

    if (data_len < 2)
        return 0;

    hdr->fin    = (data[0] & 0x80) != 0;
    hdr->opcode = (uint8_t)(data[0] & 0x0F);
    hdr->masked = (data[1] & 0x80) != 0;

    hdr->payload_len = (size_t)(data[1] & 0x7F);
    hdr->header_len  = 2;

    /* Extended payload length */
    if (hdr->payload_len == 126) {
        if (data_len < 4)
            return 0;
        hdr->payload_len = ((size_t)data[2] << 8) | data[3];
        hdr->header_len  = 4;
    } else if (hdr->payload_len == 127) {
        if (data_len < 10)
            return 0;
        hdr->payload_len = 0;
        for (i = 0; i < 8; i++)
            hdr->payload_len = (hdr->payload_len << 8) | data[2 + i];
        hdr->header_len = 10;
    }

    /* Masking key immediately follows base header */
    if (hdr->masked) {
        if (data_len < hdr->header_len + 4)
            return 0;
        memcpy(hdr->mask, data + hdr->header_len, 4);
        hdr->header_len += 4;
    }

    /* Check we have the full payload */
    if (data_len < hdr->header_len + hdr->payload_len)
        return 0;

    return 1;
}

/*
 * Unmasks payload bytes in place using the 4-byte mask key.
 * RFC 6455 s5.3: payload[i] ^= mask[i % 4]
 */
static void ws_unmask_payload(uint8_t *payload, size_t payload_len,
                              const uint8_t mask[4])
{
    size_t i;
    for (i = 0; i < payload_len; i++)
        payload[i] ^= mask[i % 4];
}

/* =========================================================================
 * Fragmented message reassembly buffer
 * RFC 6455 s5.4: a message may be split across multiple frames.
 * Continuation frames (opcode=0) carry fragments after the first frame.
 * ====================================================================== */

typedef struct {
    uint8_t  opcode;    /* opcode of the first fragment */
    uint8_t *data;      /* accumulated payload */
    size_t   length;
    size_t   capacity;
} ws_frag_buf_t;

static void ws_frag_init(ws_frag_buf_t *f)
{
    f->opcode   = 0;
    f->data     = NULL;
    f->length   = 0;
    f->capacity = 0;
}

static int ws_frag_append(ws_frag_buf_t *f, const uint8_t *payload,
                          size_t payload_len)
{
    if (f->length + payload_len + 1 > f->capacity) {
        size_t   new_cap = (f->capacity == 0) ? payload_len + 1
                                              : f->capacity * 2;
        if (new_cap < f->length + payload_len + 1)
            new_cap = f->length + payload_len + 1;
        uint8_t *tmp = (uint8_t *)realloc(f->data, new_cap);
        if (!tmp)
            return 0;
        f->data     = tmp;
        f->capacity = new_cap;
    }
    memcpy(f->data + f->length, payload, payload_len);
    f->length += payload_len;
    f->data[f->length] = '\0'; /* keep null-terminated */
    return 1;
}

static void ws_frag_reset(ws_frag_buf_t *f)
{
    free(f->data);
    ws_frag_init(f);
}

/* =========================================================================
 * Frame receive helper
 *
 * Reads exactly one complete WebSocket frame from sock into a malloc'd
 * buffer. Uses header-only length inspection to know when to stop reading,
 * so the payload is never unmasked during the read phase.
 * ====================================================================== */

/*
 * A persistent receive buffer. A single recv can return several coalesced
 * frames, so the reader keeps leftover bytes between calls and returns one
 * frame at a time. Without this, every frame after the first in a recv is lost.
 */
typedef struct {
    uint8_t *buf;
    size_t   length;   /* bytes currently buffered */
    size_t   capacity;
} ws_read_buf_t;

static int ws_read_buf_init(ws_read_buf_t *rb)
{
    rb->capacity = PEKO_WS_READ_CHUNK;
    rb->length   = 0;
    /* +1 extra byte so a payload can be null-terminated in place if needed */
    rb->buf      = (uint8_t *)malloc(rb->capacity + 1);
    return rb->buf != NULL;
}

static void ws_read_buf_free(ws_read_buf_t *rb)
{
    free(rb->buf);
    rb->buf = NULL;
}

/*
 * Inspect the buffered bytes for one complete frame using a header-only parse
 * (no unmasking). Returns 1 and sets *total_len to the frame's header+payload
 * length when a full frame is buffered; 0 when more bytes are needed.
 */
static int ws_buffered_frame_len(const ws_read_buf_t *rb, size_t *total_len)
{
    if (rb->length < 2)
        return 0;

    size_t plen = (size_t)(rb->buf[1] & 0x7F);
    size_t hlen = 2;

    if (plen == 126) {
        if (rb->length < 4) return 0;
        plen = ((size_t)rb->buf[2] << 8) | rb->buf[3];
        hlen = 4;
    } else if (plen == 127) {
        if (rb->length < 10) return 0;
        size_t p = 0;
        for (int i = 0; i < 8; i++)
            p = (p << 8) | rb->buf[2 + i];
        plen = p;
        hlen = 10;
    }

    if (rb->buf[1] & 0x80)
        hlen += 4;

    if (rb->length < hlen + plen)
        return 0;

    *total_len = hlen + plen;
    return 1;
}

/*
 * Read one complete frame. On success returns rb->buf with the frame at
 * offset 0 and sets *frame_len to its header+payload length. Bytes past the
 * frame belong to following frames and stay buffered for the next call.
 * Returns NULL on close or error.
 */
static uint8_t *ws_recv_next_frame(peko_socket_t sock, ws_read_buf_t *rb,
                                   size_t *frame_len)
{
    for (;;) {
        if (ws_buffered_frame_len(rb, frame_len))
            return rb->buf;

        if (rb->length + PEKO_WS_READ_CHUNK + 1 > rb->capacity) {
            size_t newcap = rb->capacity * 2;
            while (rb->length + PEKO_WS_READ_CHUNK + 1 > newcap)
                newcap *= 2;
            uint8_t *tmp = (uint8_t *)realloc(rb->buf, newcap + 1);
            if (!tmp)
                return NULL;
            rb->buf      = tmp;
            rb->capacity = newcap;
        }

        pgc_begin_blocking();
        int n = (int)peko_recv(sock, (char *)(rb->buf + rb->length),
                               PEKO_WS_READ_CHUNK);
        pgc_end_blocking();
        if (n <= 0)
            return NULL; /* error or connection closed */

        rb->length += (size_t)n;
    }
}

/*
 * Drop the leading frame_len bytes, shifting any buffered following frames to
 * the front so the next read starts at a frame boundary.
 */
static void ws_consume_frame(ws_read_buf_t *rb, size_t frame_len)
{
    if (frame_len >= rb->length) {
        rb->length = 0;
    } else {
        memmove(rb->buf, rb->buf + frame_len, rb->length - frame_len);
        rb->length -= frame_len;
    }
}

/* =========================================================================
 * Public WebSocket API
 * ====================================================================== */

/* Event codes passed to the serve handler. An empty text accompanies open and
 * close. */
#define PEKO_WS_EVENT_OPEN    0
#define PEKO_WS_EVENT_MESSAGE 1
#define PEKO_WS_EVENT_CLOSE   2

/*
 * Accept one connection on listen_socket. Returns the client fd, or -1 on
 * failure. The thread parks during accept() so a collection can proceed while
 * it waits.
 */
int peko_ws_accept(peko_socket_t listen_socket)
{
    struct sockaddr_in client_addr;
    socklen_t          client_len = sizeof(client_addr);
    peko_socket_t      client;

    pgc_begin_blocking();
    client = accept(listen_socket, (struct sockaddr *)&client_addr, &client_len);
    pgc_end_blocking();

    if (client == PEKO_INVALID_SOCKET)
        return -1;
    return (int)client;
}

/*
 * Serve one accepted connection: perform the WebSocket upgrade, then dispatch
 * every inbound text message to handler until the connection closes. handler
 * receives an event code (open, message, close), the connection fd, and the
 * message text (empty for open and close). The caller runs this on a dedicated
 * thread, so many connections are served at once. Returns 0 on a clean close, 1
 * when the handshake failed.
 */
int peko_ws_serve(peko_socket_t   client,
                  void          (*handler)(void *, int, peko_socket_t, char *),
                  void           *data)
{
    /* Keep the GC-managed closure context alive via a handle for the duration
     * of this call. Re-resolve via pgc_handle_get before each use since GC
     * collections during blocking recv calls may move it. */
    pgc_handle data_handle = pgc_handle_create(data);

    /* Register a send mutex for this connection so peko_ws_send_text and the
     * internal sends here are serialized. */
    ws_send_mutex_t *send_mu = ws_conn_register(client);

    /* Suppress SIGPIPE so a broken connection returns EPIPE instead of killing
     * the process. */
#if defined(SO_NOSIGPIPE)
    int nosig = 1;
    setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &nosig, sizeof(nosig));
#elif !defined(_WIN32)
    signal(SIGPIPE, SIG_IGN);
#endif

    /* ------------------------------------------------------------------ */
    /* 1. Read the HTTP Upgrade request (RFC 6455 s4.2.1)                  */
    /* ------------------------------------------------------------------ */
    char  *raw_request = NULL;
    {
        size_t  capacity = PEKO_WS_READ_CHUNK;
        size_t  length   = 0;
        char   *buf      = (char *)malloc(capacity + 1);

        if (!buf) {
            ws_conn_unregister(client);
            peko_close_socket(client);
            pgc_handle_release(data_handle);
            return 1;
        }

        for (;;) {
            if (length + PEKO_WS_READ_CHUNK + 1 > capacity) {
                capacity *= 2;
                char *tmp = (char *)realloc(buf, capacity + 1);
                if (!tmp) {
                    free(buf);
                    ws_conn_unregister(client);
                    peko_close_socket(client);
                    pgc_handle_release(data_handle);
                    return 1;
                }
                buf = tmp;
            }

            pgc_begin_blocking();
            int n = (int)peko_recv(client, buf + length, PEKO_WS_READ_CHUNK);
            pgc_end_blocking();
            if (n <= 0) {
                free(buf);
                ws_conn_unregister(client);
                peko_close_socket(client);
                pgc_handle_release(data_handle);
                return 1;
            }

            length += (size_t)n;
            buf[length] = '\0';

            /* HTTP header block ends at \r\n\r\n */
            if (strstr(buf, "\r\n\r\n"))
                break;
        }

        raw_request = buf;
    }

    /* ------------------------------------------------------------------ */
    /* 2. Build and send HTTP 101 response (RFC 6455 s4.2.2)               */
    /* ------------------------------------------------------------------ */
    char *ws_key    = NULL;
    char *handshake = NULL;

    if (ws_extract_key(raw_request, &ws_key) != 0) {
        free(raw_request);
        ws_conn_unregister(client);
        peko_close_socket(client);
        pgc_handle_release(data_handle);
        return 1;
    }

    handshake = ws_build_handshake(ws_key);
    free(raw_request);
    raw_request = NULL;

    if (!handshake) {
        ws_conn_unregister(client);
        peko_close_socket(client);
        pgc_handle_release(data_handle);
        return 1;
    }

    {
        size_t hlen = strlen(handshake);
        int hok;
        if (send_mu) {
            hok = ws_locked_send(client, send_mu, handshake, hlen) == 0;
        } else {
            pgc_begin_blocking();
            hok = (peko_send(client, handshake, hlen) > 0);
            pgc_end_blocking();
        }
        free(handshake);
        if (!hok) {
            ws_conn_unregister(client);
            peko_close_socket(client);
            pgc_handle_release(data_handle);
            return 1;
        }
    }

    /* The connection is open. Signal the handler so the managed side can track
     * it. The text is empty for a lifecycle event. */
    {
        void *live_data = pgc_handle_get(data_handle);
        handler(live_data, PEKO_WS_EVENT_OPEN, client, "");
    }

    /* ------------------------------------------------------------------ */
    /* 3. Message receive loop with fragmentation support (RFC 6455 s5.4)  */
    /* ------------------------------------------------------------------ */
    ws_frag_buf_t frag;
    ws_frag_init(&frag);

    ws_read_buf_t rb;
    if (!ws_read_buf_init(&rb)) {
        ws_frag_reset(&frag);
        void *live_data = pgc_handle_get(data_handle);
        handler(live_data, PEKO_WS_EVENT_CLOSE, client, "");
        ws_conn_unregister(client);
        peko_close_socket(client);
        pgc_handle_release(data_handle);
        return 1;
    }

    for (;;) {
        size_t   frame_len  = 0;
        uint8_t *frame_data = ws_recv_next_frame(client, &rb, &frame_len);

        if (!frame_data)
            break; /* connection error or closed */

        ws_frame_header_t hdr;
        if (!ws_parse_frame_header(frame_data, frame_len, &hdr)) {
            break;
        }

        uint8_t *payload = frame_data + hdr.header_len;

        if (hdr.masked)
            ws_unmask_payload(payload, hdr.payload_len, hdr.mask);

        /* The payload is not null-terminated in place: the byte after it
         * belongs to the next buffered frame. Ping and text paths use the
         * explicit payload length, and ws_frag_append copies and terminates. */

        if (hdr.opcode == WS_OPCODE_PING) {
            size_t pong_len = 0;
            char  *pong     = ws_encode_pong((char *)payload,
                                              hdr.payload_len, &pong_len);
            if (pong) {
                if (send_mu)
                    ws_locked_send(client, send_mu, pong, pong_len);
                free(pong);
            }
            ws_consume_frame(&rb, frame_len);
            continue;

        } else if (hdr.opcode == WS_OPCODE_PONG) {
            ws_consume_frame(&rb, frame_len);
            continue;

        } else if (hdr.opcode == WS_OPCODE_CLOSE) {
            size_t close_len = 0;
            char  *close_frame = ws_encode_close(1000, &close_len);
            if (close_frame) {
                if (send_mu)
                    ws_locked_send(client, send_mu, close_frame, close_len);
                free(close_frame);
            }
            break;
        }

        if (hdr.opcode == WS_OPCODE_TXT || hdr.opcode == WS_OPCODE_BIN) {
            if (frag.data) {
                ws_frag_reset(&frag);
            }
            frag.opcode = hdr.opcode;
        }

        if (!ws_frag_append(&frag, payload, hdr.payload_len)) {
            break; /* OOM */
        }

        int     is_fin      = hdr.fin;
        uint8_t frag_opcode = frag.opcode;

        /* Consume this frame before dispatch. The message is delivered from
         * frag.data, a separate buffer, so following frames stay intact. */
        ws_consume_frame(&rb, frame_len);

        if (is_fin) {
            if (frag_opcode == WS_OPCODE_TXT) {
                void *live_data = pgc_handle_get(data_handle);
                handler(live_data, PEKO_WS_EVENT_MESSAGE, client,
                        (char *)frag.data);
            }
            ws_frag_reset(&frag);
        }
    }

    ws_read_buf_free(&rb);
    ws_frag_reset(&frag);

    /* The connection closed. Signal the handler so the managed side can drop
     * it, then tear down. */
    {
        void *live_data = pgc_handle_get(data_handle);
        handler(live_data, PEKO_WS_EVENT_CLOSE, client, "");
    }

    ws_conn_unregister(client);
    peko_close_socket(client);
    pgc_handle_release(data_handle);
    return 0;
}

/*
 * Accept and serve one connection on the calling thread. Retained for
 * single-connection callers. Returns 0 on a clean close, 1 on accept or
 * handshake failure.
 */
int peko_ws_accept_connection(peko_socket_t   listen_socket,
                              void          (*handler)(void *, int,
                                                       peko_socket_t, char *),
                              void           *data)
{
    int client = peko_ws_accept(listen_socket);
    if (client < 0)
        return 1;
    return peko_ws_serve((peko_socket_t)client, handler, data);
}

int peko_ws_send_text(peko_socket_t socket, const char *text)
{
    size_t  text_len  = strlen(text);
    size_t  frame_len = 0;
    char   *frame     = ws_encode_frame(text, text_len, &frame_len);
    int     result    = -1;

    if (!frame)
        return -1;

    int idx = ws_conn_acquire(socket);
    if (idx >= 0) {
        result = ws_locked_send(socket, &g_ws_conns[idx].mutex,
                                frame, frame_len) == 0
                 ? (int)frame_len : -1;
        ws_conn_release(idx);
    }
    /* If idx < 0: connection closed, drop the frame silently. */

    free(frame);
    return result;
}
