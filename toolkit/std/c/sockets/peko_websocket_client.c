/*
 * peko_websocket_client.c
 * Outbound WebSocket client per RFC 6455 (the pekoui native bridge dials the
 * hosted `/__peko__` as the device provider).
 *
 * This file owns the handshake and framing and carries NO TLS dependency: the
 * `ws://` transport is a plain socket set up here; the `wss://` transport is
 * built by peko_ws_tls_transport_connect (in peko_sockets_tls.c) and handed back
 * through the ws_transport_t vtable. Client->server frames are masked (RFC 6455
 * s5.3); server->client frames arrive unmasked.
 */

#include "peko_sockets.h"
#include <pgc.h>
#include <string.h>
#include <time.h>

#define WSC_OPCODE_CONT  0x0
#define WSC_OPCODE_TEXT  0x1
#define WSC_OPCODE_BIN   0x2
#define WSC_OPCODE_CLOSE 0x8
#define WSC_OPCODE_PING  0x9
#define WSC_OPCODE_PONG  0xA
#define WSC_READ_CHUNK   4096

static const char WSC_B64[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

typedef struct {
    ws_transport_t tp;
    unsigned char *rbuf;   /* bytes read from the transport, not yet consumed */
    size_t         rlen;
    size_t         rcap;
} ws_client_t;

/* ---- base64 (small, local; the server file's copy is static) ------------ */

static char *wsc_base64(const unsigned char *src, size_t len)
{
    size_t olen = 4 * ((len + 2) / 3) + 1;
    char  *out  = (char *)malloc(olen);
    char  *pos;
    const unsigned char *in = src, *end = src + len;
    if (!out)
        return NULL;
    pos = out;
    while (end - in >= 3) {
        *pos++ = WSC_B64[in[0] >> 2];
        *pos++ = WSC_B64[((in[0] & 0x03) << 4) | (in[1] >> 4)];
        *pos++ = WSC_B64[((in[1] & 0x0F) << 2) | (in[2] >> 6)];
        *pos++ = WSC_B64[in[2] & 0x3F];
        in += 3;
    }
    if (end - in == 2) {
        *pos++ = WSC_B64[in[0] >> 2];
        *pos++ = WSC_B64[((in[0] & 0x03) << 4) | (in[1] >> 4)];
        *pos++ = WSC_B64[(in[1] & 0x0F) << 2];
        *pos++ = '=';
    } else if (end - in == 1) {
        *pos++ = WSC_B64[in[0] >> 2];
        *pos++ = WSC_B64[(in[0] & 0x03) << 4];
        *pos++ = '=';
        *pos++ = '=';
    }
    *pos = '\0';
    return out;
}

/* ---- weak RNG for the client key + mask (masking is anti-cache-poisoning,
 * not a security boundary, so a seeded PRNG is sufficient) ---------------- */

static void wsc_random_bytes(unsigned char *out, size_t n)
{
    static int seeded = 0;
    if (!seeded) {
        unsigned seed = (unsigned)time(NULL) ^ (unsigned)(uintptr_t)out;
        srand(seed);
        seeded = 1;
    }
    for (size_t i = 0; i < n; i++)
        out[i] = (unsigned char)(rand() & 0xFF);
}

/* ---- plain (ws://) transport -------------------------------------------- */

typedef struct {
    peko_socket_t sock;
} wsc_plain_ctx;

static int wsc_plain_read(void *ctx, unsigned char *buf, size_t len)
{
    peko_socket_t s = ((wsc_plain_ctx *)ctx)->sock;
    pgc_begin_blocking();
    int n = (int)peko_recv(s, (char *)buf, len);
    pgc_end_blocking();
    return n;
}

static int wsc_plain_write_all(void *ctx, const unsigned char *buf, size_t len)
{
    peko_socket_t s = ((wsc_plain_ctx *)ctx)->sock;
    size_t off = 0;
    while (off < len) {
        pgc_begin_blocking();
        int n = (int)peko_send(s, (const char *)buf + off, len - off);
        pgc_end_blocking();
        if (n <= 0)
            return -1;
        off += (size_t)n;
    }
    return 0;
}

static void wsc_plain_close(void *ctx)
{
    wsc_plain_ctx *c = (wsc_plain_ctx *)ctx;
    if (c->sock != PEKO_INVALID_SOCKET)
        peko_close_socket(c->sock);
    free(c);
}

static int wsc_plain_transport_connect(const char *host, int port,
                                       ws_transport_t *out)
{
    struct addrinfo hints, *res = NULL;
    char port_str[16];
    peko_socket_t sock = PEKO_INVALID_SOCKET;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    int rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (rc != 0 || !res)
        return -1;

    for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
        peko_socket_t s = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (s == PEKO_INVALID_SOCKET)
            continue;
        pgc_begin_blocking();
        int crc = connect(s, ai->ai_addr, (int)ai->ai_addrlen);
        pgc_end_blocking();
        if (crc == 0) {
            sock = s;
            break;
        }
        peko_close_socket(s);
    }
    freeaddrinfo(res);
    if (sock == PEKO_INVALID_SOCKET)
        return -1;

    wsc_plain_ctx *ctx = (wsc_plain_ctx *)malloc(sizeof(*ctx));
    if (!ctx) {
        peko_close_socket(sock);
        return -1;
    }
    ctx->sock = sock;
    out->ctx = ctx;
    out->read = wsc_plain_read;
    out->write_all = wsc_plain_write_all;
    out->close = wsc_plain_close;
    return 0;
}

/* ---- URL parsing -------------------------------------------------------- */

/* Split url into scheme/host/port/path. Returns 0 on success. host and path are
 * written into caller buffers; *is_tls and *port are set. */
static int wsc_parse_url(const char *url, int *is_tls, char *host, size_t host_cap,
                         int *port, char *path, size_t path_cap)
{
    const char *p = url;
    if (strncmp(p, "wss://", 6) == 0) {
        *is_tls = 1;
        *port = 443;
        p += 6;
    } else if (strncmp(p, "ws://", 5) == 0) {
        *is_tls = 0;
        *port = 80;
        p += 5;
    } else {
        return -1;
    }

    const char *host_start = p;
    while (*p && *p != ':' && *p != '/')
        p++;
    size_t hlen = (size_t)(p - host_start);
    if (hlen == 0 || hlen >= host_cap)
        return -1;
    memcpy(host, host_start, hlen);
    host[hlen] = '\0';

    if (*p == ':') {
        p++;
        int parsed = 0;
        int seen = 0;
        while (*p >= '0' && *p <= '9') {
            parsed = parsed * 10 + (*p - '0');
            seen = 1;
            p++;
        }
        if (seen)
            *port = parsed;
    }

    if (*p == '/') {
        size_t plen = strlen(p);
        if (plen >= path_cap)
            return -1;
        memcpy(path, p, plen + 1);
    } else {
        if (path_cap < 2)
            return -1;
        path[0] = '/';
        path[1] = '\0';
    }
    return 0;
}

/* ---- read buffering ----------------------------------------------------- */

/* Ensure the read buffer holds at least `need` bytes, pulling from the
 * transport. Returns 0 on success, -1 on close/error. */
static int wsc_fill(ws_client_t *c, size_t need)
{
    while (c->rlen < need) {
        if (c->rcap < c->rlen + WSC_READ_CHUNK) {
            size_t ncap = c->rlen + WSC_READ_CHUNK;
            unsigned char *nb = (unsigned char *)realloc(c->rbuf, ncap);
            if (!nb)
                return -1;
            c->rbuf = nb;
            c->rcap = ncap;
        }
        int n = c->tp.read(c->tp.ctx, c->rbuf + c->rlen, c->rcap - c->rlen);
        if (n <= 0)
            return -1;
        c->rlen += (size_t)n;
    }
    return 0;
}

/* Whether `needle` (NUL-terminated) occurs in the first `hay_len` bytes of hay.
 * Portable stand-in for memmem, which is a GNU extension. */
static int wsc_contains(const unsigned char *hay, size_t hay_len, const char *needle)
{
    size_t nlen = strlen(needle);
    if (nlen == 0 || hay_len < nlen)
        return 0;
    for (size_t i = 0; i + nlen <= hay_len; i++) {
        if (memcmp(hay + i, needle, nlen) == 0)
            return 1;
    }
    return 0;
}

/* Drop `count` consumed bytes from the front of the read buffer. */
static void wsc_consume(ws_client_t *c, size_t count)
{
    if (count >= c->rlen) {
        c->rlen = 0;
        return;
    }
    memmove(c->rbuf, c->rbuf + count, c->rlen - count);
    c->rlen -= count;
}

/* ---- frame send --------------------------------------------------------- */

/* Encode and write one masked frame with the given opcode. */
static int wsc_send_frame(ws_client_t *c, int opcode,
                          const unsigned char *payload, size_t len)
{
    size_t header_len = 2;
    if (len > 65535)
        header_len = 10;
    else if (len > 125)
        header_len = 4;
    size_t total = header_len + 4 /* mask */ + len;
    unsigned char *frame = (unsigned char *)malloc(total ? total : 1);
    if (!frame)
        return -1;

    frame[0] = (unsigned char)(0x80 | (opcode & 0x0F));
    if (header_len == 2) {
        frame[1] = (unsigned char)(0x80 | len);
    } else if (header_len == 4) {
        frame[1] = (unsigned char)(0x80 | 126);
        frame[2] = (unsigned char)((len >> 8) & 0xFF);
        frame[3] = (unsigned char)(len & 0xFF);
    } else {
        frame[1] = (unsigned char)(0x80 | 127);
        for (int i = 0; i < 8; i++)
            frame[2 + i] = (unsigned char)((len >> (56 - 8 * i)) & 0xFF);
    }

    unsigned char *mask = frame + header_len;
    wsc_random_bytes(mask, 4);
    unsigned char *out = frame + header_len + 4;
    for (size_t i = 0; i < len; i++)
        out[i] = payload[i] ^ mask[i & 3];

    int rc = c->tp.write_all(c->tp.ctx, frame, total);
    free(frame);
    return rc;
}

/* ---- handshake ---------------------------------------------------------- */

static int wsc_handshake(ws_client_t *c, const char *host, int port, int is_tls,
                         const char *path, const char *subprotocol,
                         const char *extra_headers)
{
    unsigned char keybytes[16];
    wsc_random_bytes(keybytes, sizeof(keybytes));
    char *key = wsc_base64(keybytes, sizeof(keybytes));
    if (!key)
        return -1;

    /* Host header: omit the port when it is the scheme default (443 for wss,
     * 80 for ws). A `host:443` Host does not match a CloudFront/vhost alias, so
     * including it misroutes the request — the reason a browser (which omits it)
     * connects but this client would not. */
    char host_header[300];
    int default_port = is_tls ? 443 : 80;
    if (port == default_port) {
        snprintf(host_header, sizeof(host_header), "%s", host);
    } else {
        snprintf(host_header, sizeof(host_header), "%s:%d", host, port);
    }

    /* Build the upgrade request. */
    size_t cap = 512 + strlen(host_header) + strlen(path) + strlen(key) +
                 (subprotocol ? strlen(subprotocol) : 0) +
                 (extra_headers ? strlen(extra_headers) : 0);
    char *req = (char *)malloc(cap);
    if (!req) {
        free(key);
        return -1;
    }
    int off = snprintf(req, cap,
                       "GET %s HTTP/1.1\r\n"
                       "Host: %s\r\n"
                       "Upgrade: websocket\r\n"
                       "Connection: Upgrade\r\n"
                       "Sec-WebSocket-Version: 13\r\n"
                       "Sec-WebSocket-Key: %s\r\n",
                       path, host_header, key);
    free(key);
    if (off < 0 || (size_t)off >= cap) {
        free(req);
        return -1;
    }
    if (subprotocol && subprotocol[0])
        off += snprintf(req + off, cap - off, "Sec-WebSocket-Protocol: %s\r\n",
                        subprotocol);
    if (extra_headers && extra_headers[0])
        off += snprintf(req + off, cap - off, "%s", extra_headers);
    off += snprintf(req + off, cap - off, "\r\n");

    int rc = c->tp.write_all(c->tp.ctx, (const unsigned char *)req, (size_t)off);
    free(req);
    if (rc != 0)
        return -1;

    /* Read the response header block up to the terminating CRLFCRLF. */
    for (;;) {
        if (c->rlen >= 4) {
            for (size_t i = 0; i + 3 < c->rlen; i++) {
                if (c->rbuf[i] == '\r' && c->rbuf[i + 1] == '\n' &&
                    c->rbuf[i + 2] == '\r' && c->rbuf[i + 3] == '\n') {
                    /* Status line must be 101 Switching Protocols. */
                    int ok = wsc_contains(c->rbuf, i + 4, " 101 ");
                    wsc_consume(c, i + 4);
                    return ok ? 0 : -1;
                }
            }
        }
        if (wsc_fill(c, c->rlen + 1) != 0)
            return -1;
        if (c->rlen > 65536)
            return -1; /* runaway header block */
    }
}

/* ---- public API --------------------------------------------------------- */

void *peko_ws_client_connect(const char *url, const char *subprotocol,
                             const char *extra_headers)
{
    int is_tls = 0, port = 0;
    char host[256];
    char path[2048];
    if (wsc_parse_url(url, &is_tls, host, sizeof(host), &port, path, sizeof(path)) != 0)
        return NULL;

    ws_transport_t tp;
    int trc;
    if (is_tls)
        trc = peko_ws_tls_transport_connect(host, port, &tp);
    else
        trc = wsc_plain_transport_connect(host, port, &tp);
    if (trc != 0)
        return NULL;

    ws_client_t *c = (ws_client_t *)malloc(sizeof(*c));
    if (!c) {
        tp.close(tp.ctx);
        return NULL;
    }
    c->tp = tp;
    c->rbuf = NULL;
    c->rlen = 0;
    c->rcap = 0;

    if (wsc_handshake(c, host, port, is_tls, path, subprotocol, extra_headers) != 0) {
        peko_ws_client_close(c);
        return NULL;
    }
    return c;
}

int peko_ws_client_send(void *client, const char *text)
{
    ws_client_t *c = (ws_client_t *)client;
    if (!c || !text)
        return -1;
    return wsc_send_frame(c, WSC_OPCODE_TEXT, (const unsigned char *)text,
                          strlen(text));
}

char *peko_ws_client_recv(void *client)
{
    ws_client_t *c = (ws_client_t *)client;
    if (!c)
        return NULL;

    unsigned char *reasm = NULL; /* fragmented-message reassembly buffer */
    size_t reasm_len = 0;
    int reasm_opcode = 0;

    for (;;) {
        if (wsc_fill(c, 2) != 0)
            goto fail;
        unsigned char b0 = c->rbuf[0];
        unsigned char b1 = c->rbuf[1];
        int fin = (b0 & 0x80) != 0;
        int opcode = b0 & 0x0F;
        int masked = (b1 & 0x80) != 0;
        uint64_t len = b1 & 0x7F;
        size_t hdr = 2;

        if (len == 126) {
            if (wsc_fill(c, 4) != 0)
                goto fail;
            len = ((uint64_t)c->rbuf[2] << 8) | c->rbuf[3];
            hdr = 4;
        } else if (len == 127) {
            if (wsc_fill(c, 10) != 0)
                goto fail;
            len = 0;
            for (int i = 0; i < 8; i++)
                len = (len << 8) | c->rbuf[2 + i];
            hdr = 10;
        }
        size_t mask_len = masked ? 4 : 0;
        if (wsc_fill(c, hdr + mask_len + (size_t)len) != 0)
            goto fail;

        unsigned char *payload = c->rbuf + hdr + mask_len;
        if (masked) {
            unsigned char *mk = c->rbuf + hdr;
            for (uint64_t i = 0; i < len; i++)
                payload[i] ^= mk[i & 3];
        }

        if (opcode == WSC_OPCODE_CLOSE) {
            goto fail;
        } else if (opcode == WSC_OPCODE_PING) {
            wsc_send_frame(c, WSC_OPCODE_PONG, payload, (size_t)len);
            wsc_consume(c, hdr + mask_len + (size_t)len);
            continue;
        } else if (opcode == WSC_OPCODE_PONG) {
            wsc_consume(c, hdr + mask_len + (size_t)len);
            continue;
        }

        /* Data frame (text/binary/continuation). Reassemble fragments. */
        if (opcode == WSC_OPCODE_CONT || !fin) {
            unsigned char *nb = (unsigned char *)realloc(reasm, reasm_len + (size_t)len);
            if (!nb)
                goto fail;
            reasm = nb;
            memcpy(reasm + reasm_len, payload, (size_t)len);
            reasm_len += (size_t)len;
            if (opcode != WSC_OPCODE_CONT)
                reasm_opcode = opcode;
            wsc_consume(c, hdr + mask_len + (size_t)len);
            if (fin) {
                char *out = (char *)malloc(reasm_len + 1);
                if (!out)
                    goto fail;
                memcpy(out, reasm, reasm_len);
                out[reasm_len] = '\0';
                free(reasm);
                return out;
            }
            continue;
        }

        /* Single-frame message. */
        {
            char *out = (char *)malloc((size_t)len + 1);
            if (!out)
                goto fail;
            memcpy(out, payload, (size_t)len);
            out[len] = '\0';
            wsc_consume(c, hdr + mask_len + (size_t)len);
            (void)reasm_opcode;
            return out;
        }
    }

fail:
    if (reasm)
        free(reasm);
    return NULL;
}

void peko_ws_client_free_message(char *msg)
{
    free(msg);
}

void peko_ws_client_close(void *client)
{
    ws_client_t *c = (ws_client_t *)client;
    if (!c)
        return;
    /* Best-effort close frame. */
    unsigned char empty[1];
    wsc_send_frame(c, WSC_OPCODE_CLOSE, empty, 0);
    if (c->tp.close)
        c->tp.close(c->tp.ctx);
    if (c->rbuf)
        free(c->rbuf);
    free(c);
}
