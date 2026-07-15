/*
 * peko_sockets_tls.c
 * Outbound HTTPS support for the Peko sockets library, built on BearSSL.
 *
 * This provides peko_create_request_tls, the TLS sibling of
 * peko_create_request: it connects over TCP, performs a TLS handshake, sends
 * the request, reads the whole response, and returns it as a GC-managed
 * string, mirroring the plain-HTTP function's contract exactly.
 *
 * ===========================================================================
 * SECURITY WARNING - INSECURE BY DESIGN
 * ===========================================================================
 * This adapter does NOT verify server certificates. It installs a custom
 * X.509 engine (the "insecure" engine below) that accepts ANY certificate
 * chain from ANY host without validation. This means the connection is
 * encrypted but NOT authenticated: a man-in-the-middle can impersonate the
 * server and read or modify all traffic. It is a convenience for connecting
 * to arbitrary hosts in a demo/personal context. DO NOT use this for anything
 * handling sensitive data or in production. To make it secure, replace the
 * insecure engine with br_ssl_client_init_full plus a real trust-anchor set.
 * ===========================================================================
 *
 * Vendoring BearSSL: this file expects the BearSSL headers and sources to be
 * compiled and linked alongside it (the single "bearssl.h" umbrella header is
 * included below). Drop the BearSSL src/ and inc/ into the package and add
 * them to each platform's object build next to peko_sockets.c.
 */

#include "peko_sockets.h"

#include "inc/bearssl.h"

extern void *pgc_alloc_atomic(size_t size);
extern void  pgc_begin_blocking(void);
extern void  pgc_end_blocking(void);

typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_handle_release(pgc_handle handle);

/* -------------------------------------------------------------------------
 * Receive timeout
 *
 * Sets a receive timeout on the socket. The response read loop ends when a
 * read returns zero or negative. A server that holds the connection open
 * without closing it would otherwise block the read loop forever. The timeout
 * bounds that wait so the loop ends and the function returns the bytes read so
 * far.
 * ---------------------------------------------------------------------- */

static void set_recv_timeout(peko_socket_t sock, int seconds)
{
#ifdef _WIN32
    DWORD ms = (DWORD)seconds * 1000;
    setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, (const char *)&ms, sizeof(ms));
#else
    struct timeval tv;
    tv.tv_sec  = seconds;
    tv.tv_usec = 0;
    setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
#endif
}

/* -------------------------------------------------------------------------
 * "Accept any certificate" X.509 engine
 *
 * This engine wraps the br_x509_minimal engine. Every callback is delegated to
 * the minimal engine so the certificate is parsed and the server public key is
 * extracted, both of which the handshake requires. The end_chain callback
 * discards the minimal engine's trust verdict and returns BR_ERR_OK, so the
 * handshake completes for any certificate.
 *
 * This is INSECURE (no authentication; MITM-vulnerable). See the warning at the
 * top of the file. It is the documented BearSSL pattern for an insecure client.
 * ---------------------------------------------------------------------- */

typedef struct {
    const br_x509_class *vtable;
    br_x509_minimal_context inner;   /* the real engine, used for parsing */
} insecure_x509_context;

static void ix_start_chain(const br_x509_class **ctx, const char *server_name)
{
    insecure_x509_context *c = (insecure_x509_context *)ctx;
    c->inner.vtable->start_chain(&c->inner.vtable, server_name);
}

static void ix_start_cert(const br_x509_class **ctx, uint32_t length)
{
    insecure_x509_context *c = (insecure_x509_context *)ctx;
    c->inner.vtable->start_cert(&c->inner.vtable, length);
}

static void ix_append(const br_x509_class **ctx,
                      const unsigned char *buf, size_t len)
{
    insecure_x509_context *c = (insecure_x509_context *)ctx;
    c->inner.vtable->append(&c->inner.vtable, buf, len);
}

static void ix_end_cert(const br_x509_class **ctx)
{
    insecure_x509_context *c = (insecure_x509_context *)ctx;
    c->inner.vtable->end_cert(&c->inner.vtable);
}

static unsigned ix_end_chain(const br_x509_class **ctx)
{
    insecure_x509_context *c = (insecure_x509_context *)ctx;
    /* Run the real end_chain so the engine finishes parsing and the public key
     * is available, but ignore its trust verdict and report success. */
    (void)c->inner.vtable->end_chain(&c->inner.vtable);
    return 0;   /* BR_ERR_OK: accept regardless of trust */
}

static const br_x509_pkey *ix_get_pkey(const br_x509_class *const *ctx,
                                       unsigned *usages)
{
    insecure_x509_context *c = (insecure_x509_context *)ctx;
    /* Hand back the key the inner engine parsed from the server certificate. */
    return c->inner.vtable->get_pkey(
        (const br_x509_class *const *)&c->inner.vtable, usages);
}

static const br_x509_class insecure_x509_vtable = {
    sizeof(insecure_x509_context),
    ix_start_chain,
    ix_start_cert,
    ix_append,
    ix_end_cert,
    ix_end_chain,
    ix_get_pkey
};

/* -------------------------------------------------------------------------
 * TCP connect (mirrors peko_create_request: iterate all addresses)
 * ---------------------------------------------------------------------- */

static peko_socket_t tcp_connect(const char *host, int port)
{
    struct addrinfo hints, *res = NULL;
    char            port_str[16];
    peko_socket_t   sock = PEKO_INVALID_SOCKET;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    int gai_rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (gai_rc != 0 || res == NULL)
        return PEKO_INVALID_SOCKET;

    {
        struct addrinfo *ai;
        for (ai = res; ai != NULL; ai = ai->ai_next) {
            peko_socket_t s = socket(ai->ai_family, ai->ai_socktype,
                                     ai->ai_protocol);
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
    }

    freeaddrinfo(res);
    return sock;
}

/* -------------------------------------------------------------------------
 * Low-level read/write callbacks BearSSL uses to move bytes over the socket.
 * Wrapped in the GC blocking contract, since they block on network IO.
 * ---------------------------------------------------------------------- */

static int sock_read(void *ctx, unsigned char *buf, size_t len)
{
    peko_socket_t sock = *(peko_socket_t *)ctx;
    for (;;) {
        pgc_begin_blocking();
        int rlen = (int)peko_recv(sock, (char *)buf, len);
        pgc_end_blocking();
        if (rlen <= 0) {
            if (rlen < 0)
                return -1;
            return -1;   /* connection closed */
        }
        return rlen;
    }
}

static int sock_write(void *ctx, const unsigned char *buf, size_t len)
{
    peko_socket_t sock = *(peko_socket_t *)ctx;
    for (;;) {
        pgc_begin_blocking();
        int wlen = (int)peko_send(sock, (const char *)buf, len);
        pgc_end_blocking();
        if (wlen <= 0)
            return -1;
        return wlen;
    }
}

/*
 * Writes all of buf over the TLS connection from a stable copy. The caller may
 * pass a cstr that points into a managed String buffer, which the collector
 * can move while sock_write is parked inside br_sslio_write_all. Copy into a
 * stable malloc buffer first, while this thread is still running and the
 * source cannot move, then write from the copy. Returns the br_sslio_write_all
 * result, or -1 on allocation failure.
 */
static int tls_write_all_stable(br_sslio_context *ioc,
                                const char *buf, size_t len)
{
    if (len == 0)
        return 0;

    char *stable = (char *)malloc(len);
    if (!stable)
        return -1;
    memcpy(stable, buf, len);

    int rc = br_sslio_write_all(ioc, stable, len);
    free(stable);
    return rc;
}

/* -------------------------------------------------------------------------
 * peko_create_request_tls
 *
 * HTTPS sibling of peko_create_request. Takes the host, port, and a complete
 * request string, performs the TLS handshake, sends the request, reads the
 * whole response, and returns it as a GC-managed string. The contract matches
 * peko_create_request exactly: the caller supplies the full request bytes and
 * receives the full raw response (status line, headers, and body).
 *
 * The Peko side builds the request string and parses the response, so HTTP
 * framing lives in one place across both the TLS and plain transports.
 *
 * Returns a GC-managed, NUL-terminated string with the full response, or a
 * static error string on failure.
 * ---------------------------------------------------------------------- */

const char *peko_create_request_tls(const char *host, int port,
                                    const char *request)
{
    peko_socket_t sock = tcp_connect(host, port);
    if (sock == PEKO_INVALID_SOCKET)
        return "Error: could not connect";

    /* Bound the response read so a server that never closes the connection
     * cannot block the read loop forever. */
    set_recv_timeout(sock, 30);

    /* BearSSL client state. The full client profile sets up supported cipher
     * suites and hash implementations. We then install our custom X.509 engine
     * (which wraps the minimal engine but never fails trust) so the handshake
     * completes for any certificate. */
    br_ssl_client_context sc;
    insecure_x509_context xc;
    unsigned char iobuf[BR_SSL_BUFSIZE_BIDI];
    br_sslio_context ioc;

    /* Initialize the client (this also configures an internal x509 minimal
     * engine, which we override below). Pass our inner minimal context here so
     * the full profile wires its trust-anchor-less engine, then we replace the
     * engine pointer with our wrapper. */
    br_ssl_client_init_full(&sc, &xc.inner, NULL, 0);

    /* Set up the wrapper vtable and point the SSL engine at it. The wrapper
     * delegates parsing to xc.inner (initialized by init_full above) but forces
     * end_chain to succeed. */
    xc.vtable = &insecure_x509_vtable;
    br_ssl_engine_set_x509(&sc.eng, &xc.vtable);

    br_ssl_engine_set_buffer(&sc.eng, iobuf, sizeof(iobuf), 1);

    if (!br_ssl_client_reset(&sc, host, 0)) {
        peko_close_socket(sock);
        return "Error: TLS reset failed";
    }

    br_sslio_init(&ioc, &sc.eng, sock_read, &sock, sock_write, &sock);

    /* --- send the request --- */
    {
        size_t total = strlen(request);
        if (tls_write_all_stable(&ioc, request, total) != 0) {
            peko_close_socket(sock);
            return "Error: could not send request";
        }
        br_sslio_flush(&ioc);
    }

    /* --- read the response into a growing buffer --- */
    {
        size_t capacity = PEKO_RESPONSE_INITIAL_SIZE;
        size_t length   = 0;
        char  *buf      = (char *)malloc(capacity);

        if (!buf) {
            peko_close_socket(sock);
            return "Error: out of memory";
        }

        for (;;) {
            if (length + PEKO_READ_CHUNK + 1 > capacity) {
                size_t next = capacity * 2;
                if (next > PEKO_RESPONSE_MAX_SIZE)
                    next = PEKO_RESPONSE_MAX_SIZE;
                if (length + PEKO_READ_CHUNK + 1 > next) {
                    free(buf);
                    peko_close_socket(sock);
                    return "Error: response exceeds maximum buffer size";
                }
                char *tmp = (char *)realloc(buf, next);
                if (!tmp) {
                    free(buf);
                    peko_close_socket(sock);
                    return "Error: out of memory";
                }
                buf      = tmp;
                capacity = next;
            }

            int rlen = br_sslio_read(&ioc, buf + length, PEKO_READ_CHUNK);
            if (rlen < 0)
                break;   /* end of stream or error: stop reading */
            length += (size_t)rlen;
        }

        /*
         * Distinguish a clean close from a real error. After the loop, if the
         * SSL engine ended in an error state OTHER than a normal close, and we
         * also got no data, report failure. The "not trusted" X.509 error is
         * deliberately treated as success here, that is the insecure behavior.
         */
        {
            int err = br_ssl_engine_last_error(&sc.eng);
            if (length == 0 && err != BR_ERR_OK
                && err != BR_ERR_X509_NOT_TRUSTED) {
                free(buf);
                peko_close_socket(sock);
                return "Error: TLS read failed";
            }
        }

        /*
         * Copy the full raw response (status line, headers, and body) into a
         * GC-managed buffer. The Peko HttpResponse layer splits headers from
         * body and de-chunks when needed, so the same parser serves the TLS
         * and plain transports.
         */
        char *gc_buf = (char *)pgc_alloc_atomic(length + 1);
        if (!gc_buf) {
            free(buf);
            peko_close_socket(sock);
            return "Error: out of memory";
        }

        memcpy(gc_buf, buf, length);
        gc_buf[length] = '\0';

        free(buf);
        peko_close_socket(sock);
        return gc_buf;
    }
}

/* -------------------------------------------------------------------------
 * Streaming chunked decoder (mirror of peko_sockets.c)
 *
 * Decodes Transfer-Encoding: chunked body bytes from the TLS read stream
 * before they reach on_chunk. Passthrough mode forwards bytes verbatim.
 * Chunk boundaries on the wire do not align with TLS record boundaries, so
 * the decoder maintains a small line buffer for in-progress chunk-size lines.
 * ---------------------------------------------------------------------- */

#define STREAM_LINE_BUF 64

typedef enum {
    STREAM_PASSTHROUGH,
    STREAM_CHUNK_SIZE,
    STREAM_CHUNK_DATA,
    STREAM_CHUNK_CRLF,
    STREAM_DONE
} stream_state_t;

typedef struct {
    stream_state_t state;
    size_t         remaining;
    char           line[STREAM_LINE_BUF];
    size_t         line_len;
} stream_decoder_t;

static size_t parse_chunk_size(const stream_decoder_t *dec)
{
    size_t size = 0;
    int    saw  = 0;
    size_t i    = 0;
    while (i < dec->line_len) {
        char c = dec->line[i];
        if (c == ';' || c == ' ' || c == '\t')
            break;
        int v;
        if      (c >= '0' && c <= '9') v = c - '0';
        else if (c >= 'a' && c <= 'f') v = c - 'a' + 10;
        else if (c >= 'A' && c <= 'F') v = c - 'A' + 10;
        else return (size_t)-1;
        size = size * 16 + (size_t)v;
        saw  = 1;
        i++;
    }
    return saw ? size : (size_t)-1;
}

/* Mirrors peko_sockets.c. See that file for the contract. */
#define STREAM_CALLBACK_BUF 4097

static bool emit_chunk(bool (*on_chunk)(void *, const char *, size_t),
                       pgc_handle ctx_handle, const char *buf, size_t len)
{
    char out[STREAM_CALLBACK_BUF];
    size_t offset = 0;
    while (offset < len) {
        size_t take = len - offset;
        if (take > STREAM_CALLBACK_BUF - 1)
            take = STREAM_CALLBACK_BUF - 1;
        memcpy(out, buf + offset, take);
        out[take] = '\0';
        /* Re-resolve the context before each call. on_chunk allocates, so a
         * collection between iterations can move the context object. */
        void *ctx = pgc_handle_get(ctx_handle);
        if (!on_chunk(ctx, out, take))
            return false;
        offset += take;
    }
    return true;
}

static int stream_decode(stream_decoder_t *dec,
                         const char *buf, size_t len,
                         bool (*on_chunk)(void *, const char *, size_t),
                         pgc_handle user_handle)
{
    size_t i = 0;

    if (dec->state == STREAM_PASSTHROUGH) {
        if (len > 0)
            return emit_chunk(on_chunk, user_handle, buf, len) ? 1 : 0;
        return 1;
    }

    while (i < len && dec->state != STREAM_DONE) {
        if (dec->state == STREAM_CHUNK_SIZE) {
            while (i < len) {
                char c = buf[i++];
                if (c == '\n') {
                    if (dec->line_len > 0
                        && dec->line[dec->line_len - 1] == '\r')
                        dec->line_len--;
                    size_t sz = parse_chunk_size(dec);
                    dec->line_len = 0;
                    if (sz == (size_t)-1) {
                        dec->state = STREAM_DONE;
                        return 0;
                    }
                    if (sz == 0) {
                        dec->state = STREAM_DONE;
                        return 1;
                    }
                    dec->remaining = sz;
                    dec->state     = STREAM_CHUNK_DATA;
                    break;
                }
                if (dec->line_len + 1 < STREAM_LINE_BUF) {
                    dec->line[dec->line_len++] = c;
                }
            }
            continue;
        }

        if (dec->state == STREAM_CHUNK_DATA) {
            size_t avail = len - i;
            size_t take  = (avail < dec->remaining) ? avail : dec->remaining;
            if (take > 0) {
                if (!emit_chunk(on_chunk, user_handle, buf + i, take))
                    return 0;
                i              += take;
                dec->remaining -= take;
            }
            if (dec->remaining == 0)
                dec->state = STREAM_CHUNK_CRLF;
            continue;
        }

        if (dec->state == STREAM_CHUNK_CRLF) {
            while (i < len && dec->state == STREAM_CHUNK_CRLF) {
                char c = buf[i++];
                if (c == '\n')
                    dec->state = STREAM_CHUNK_SIZE;
            }
            continue;
        }
    }

    return 1;
}

/* Locates the end of HTTP headers (\r\n\r\n) inside buf[0..len). Returns the
 * offset of the first body byte, or 0 when the terminator is not present. */
static size_t find_header_end_offset(const char *buf, size_t len)
{
    for (size_t i = 0; i + 3 < len; i++) {
        if (buf[i] == '\r' && buf[i + 1] == '\n'
            && buf[i + 2] == '\r' && buf[i + 3] == '\n') {
            return i + 4;
        }
    }
    return 0;
}

/* -------------------------------------------------------------------------
 * peko_stream_request_tls
 *
 * TLS sibling of peko_stream_request. Connects, performs the TLS handshake,
 * sends the request, then reads the response incrementally calling on_headers
 * once with the parsed header block and on_chunk repeatedly with body bytes.
 *
 * Returns 0 on a clean read or callback stop. Returns non-zero on error.
 * ---------------------------------------------------------------------- */

int peko_stream_request_tls(const char *host, int port, const char *request,
                            int  (*on_headers)(void *, const char *),
                            void *headers_ctx,
                            bool (*on_chunk)(void *, const char *, size_t),
                            void *chunk_ctx)
{
    int ret = 0;

    /* Keep both callback contexts reachable and current across the blocking
     * reads below. on_headers and on_chunk allocate, so a collection can move
     * the context objects between calls; re-resolve through the handle before
     * each call. */
    pgc_handle headers_handle = pgc_handle_create(headers_ctx);
    pgc_handle chunk_handle   = pgc_handle_create(chunk_ctx);

    peko_socket_t sock = tcp_connect(host, port);
    if (sock == PEKO_INVALID_SOCKET) {
        ret = 2;
        goto cleanup;
    }

    set_recv_timeout(sock, 30);

    br_ssl_client_context sc;
    insecure_x509_context xc;
    unsigned char iobuf[BR_SSL_BUFSIZE_BIDI];
    br_sslio_context ioc;

    br_ssl_client_init_full(&sc, &xc.inner, NULL, 0);
    xc.vtable = &insecure_x509_vtable;
    br_ssl_engine_set_x509(&sc.eng, &xc.vtable);
    br_ssl_engine_set_buffer(&sc.eng, iobuf, sizeof(iobuf), 1);

    if (!br_ssl_client_reset(&sc, host, 0)) {
        ret = 5;
        goto cleanup;
    }

    br_sslio_init(&ioc, &sc.eng, sock_read, &sock, sock_write, &sock);

    /* Send the request in full. */
    if (tls_write_all_stable(&ioc, request, strlen(request)) != 0) {
        ret = 3;
        goto cleanup;
    }
    if (br_sslio_flush(&ioc) != 0) {
        ret = 3;
        goto cleanup;
    }

    char            hbuf[PEKO_READ_CHUNK * 2];
    size_t          hlen         = 0;
    int             headers_done = 0;
    stream_decoder_t dec;
    dec.state     = STREAM_PASSTHROUGH;
    dec.remaining = 0;
    dec.line_len  = 0;

    for (;;) {
        if (!headers_done && hlen >= sizeof(hbuf)) {
            ret = 4;
            goto cleanup;
        }

        char   tmp[PEKO_READ_CHUNK];
        char  *target = headers_done ? tmp : (hbuf + hlen);
        size_t cap    = headers_done ? sizeof(tmp) : (sizeof(hbuf) - hlen);

        int n = br_sslio_read(&ioc, target, cap);
        if (n <= 0)
            break;

        if (!headers_done) {
            hlen += (size_t)n;
            size_t body_off = find_header_end_offset(hbuf, hlen);
            if (body_off > 0) {
                /* NUL-terminate the header block in place. */
                hbuf[body_off - 4] = '\0';

                /* Detect chunked encoding. */
                {
                    const char *p = hbuf;
                    while (*p) {
                        if ((*p == 't' || *p == 'T')
                            && strncasecmp(p, "transfer-encoding:", 18) == 0) {
                            const char *v = p + 18;
                            while (*v == ' ' || *v == '\t') v++;
                            if (strncasecmp(v, "chunked", 7) == 0) {
                                dec.state = STREAM_CHUNK_SIZE;
                            }
                            break;
                        }
                        while (*p && *p != '\n') p++;
                        if (*p == '\n') p++;
                    }
                }

                /* Re-resolve the headers context after the blocking reads. */
                void *live_headers = pgc_handle_get(headers_handle);
                int stop = on_headers(live_headers, hbuf);
                if (stop) {
                    ret = 0;
                    goto cleanup;
                }

                headers_done = 1;

                size_t body_len = hlen - body_off;
                if (body_len > 0) {
                    if (!stream_decode(&dec, hbuf + body_off, body_len,
                                       on_chunk, chunk_handle)) {
                        ret = 0;
                        goto cleanup;
                    }
                    if (dec.state == STREAM_DONE) {
                        ret = 0;
                        goto cleanup;
                    }
                }
            }
            continue;
        }

        if (!stream_decode(&dec, tmp, (size_t)n, on_chunk, chunk_handle)) {
            ret = 0;
            goto cleanup;
        }
        if (dec.state == STREAM_DONE) {
            ret = 0;
            goto cleanup;
        }
    }

    ret = 0;

cleanup:
    if (sock != PEKO_INVALID_SOCKET)
        peko_close_socket(sock);
    pgc_handle_release(chunk_handle);
    pgc_handle_release(headers_handle);
    return ret;
}

/* -------------------------------------------------------------------------
 * TLS send-streaming
 *
 * Mirror of the plain send-streaming entry points. The struct holds the
 * full BearSSL client state for the lifetime of the streaming request,
 * since BearSSL contexts contain internal pointers and must not move. The
 * struct is malloc'd; Peko sees it as an opaque pointer.
 * ---------------------------------------------------------------------- */

typedef struct {
    peko_socket_t          sock;
    int                    closed;
    br_ssl_client_context  sc;
    insecure_x509_context  xc;
    unsigned char          iobuf[BR_SSL_BUFSIZE_BIDI];
    br_sslio_context       ioc;
} peko_send_stream_tls_t;

/* Writes an HTTP/1.1 chunk over TLS as <hex-size>\r\n<bytes>\r\n.
 * Returns 0 on success, non-zero on send error. */
static int send_chunk_framed_tls(peko_send_stream_tls_t *st,
                                 const char *bytes, size_t len)
{
    char header[32];
    int  hlen = snprintf(header, sizeof(header), "%zx\r\n", len);
    if (hlen <= 0)
        return 1;

    if (br_sslio_write_all(&st->ioc, header, (size_t)hlen) != 0)
        return 2;

    if (len > 0 && tls_write_all_stable(&st->ioc, bytes, len) != 0)
        return 3;

    if (br_sslio_write_all(&st->ioc, "\r\n", 2) != 0)
        return 4;

    if (br_sslio_flush(&st->ioc) != 0)
        return 5;

    return 0;
}

void *peko_open_stream_request_tls(const char *host, int port,
                                   const char *request_head)
{
    peko_socket_t sock = tcp_connect(host, port);
    if (sock == PEKO_INVALID_SOCKET)
        return NULL;

    set_recv_timeout(sock, 30);

    peko_send_stream_tls_t *st =
        (peko_send_stream_tls_t *)malloc(sizeof(peko_send_stream_tls_t));
    if (!st) {
        peko_close_socket(sock);
        return NULL;
    }
    st->sock   = sock;
    st->closed = 0;

    br_ssl_client_init_full(&st->sc, &st->xc.inner, NULL, 0);
    st->xc.vtable = &insecure_x509_vtable;
    br_ssl_engine_set_x509(&st->sc.eng, &st->xc.vtable);
    br_ssl_engine_set_buffer(&st->sc.eng, st->iobuf,
                             sizeof(st->iobuf), 1);

    if (!br_ssl_client_reset(&st->sc, host, 0)) {
        peko_close_socket(st->sock);
        free(st);
        return NULL;
    }

    br_sslio_init(&st->ioc, &st->sc.eng,
                  sock_read, &st->sock, sock_write, &st->sock);

    /* Send the request head, then add Transfer-Encoding and the blank line
     * that ends the header block. */
    if (tls_write_all_stable(&st->ioc, request_head,
                           strlen(request_head)) != 0) {
        peko_close_socket(st->sock);
        free(st);
        return NULL;
    }

    static const char te_and_terminator[] =
        "Transfer-Encoding: chunked\r\n\r\n";
    if (br_sslio_write_all(&st->ioc, te_and_terminator,
                           sizeof(te_and_terminator) - 1) != 0) {
        peko_close_socket(st->sock);
        free(st);
        return NULL;
    }

    if (br_sslio_flush(&st->ioc) != 0) {
        peko_close_socket(st->sock);
        free(st);
        return NULL;
    }

    return st;
}

int peko_stream_write_chunk_tls(void *handle, const char *bytes, int len)
{
    peko_send_stream_tls_t *st = (peko_send_stream_tls_t *)handle;
    if (!st || st->closed)
        return 1;
    if (len < 0)
        return 2;
    if (len == 0)
        return 0;

    if (send_chunk_framed_tls(st, bytes, (size_t)len) != 0) {
        peko_close_socket(st->sock);
        st->closed = 1;
        return 3;
    }
    return 0;
}

const char *peko_stream_finish_tls(void *handle)
{
    peko_send_stream_tls_t *st = (peko_send_stream_tls_t *)handle;
    if (!st)
        return "Error: invalid stream handle";

    if (st->closed) {
        free(st);
        return "Error: stream already closed";
    }

    /* Terminating zero-size chunk. */
    if (br_sslio_write_all(&st->ioc, "0\r\n\r\n", 5) != 0) {
        peko_close_socket(st->sock);
        free(st);
        return "Error: could not send terminator";
    }
    if (br_sslio_flush(&st->ioc) != 0) {
        peko_close_socket(st->sock);
        free(st);
        return "Error: could not flush terminator";
    }

    /* Read the response into a growing buffer. */
    size_t capacity = PEKO_RESPONSE_INITIAL_SIZE;
    size_t length   = 0;
    char  *buf      = (char *)malloc(capacity);
    if (!buf) {
        peko_close_socket(st->sock);
        free(st);
        return "Error: out of memory";
    }

    for (;;) {
        if (length + PEKO_READ_CHUNK + 1 > capacity) {
            size_t next = capacity * 2;
            if (next > PEKO_RESPONSE_MAX_SIZE)
                next = PEKO_RESPONSE_MAX_SIZE;
            if (length + PEKO_READ_CHUNK + 1 > next) {
                free(buf);
                peko_close_socket(st->sock);
                free(st);
                return "Error: response exceeds maximum buffer size";
            }
            char *tmp = (char *)realloc(buf, next);
            if (!tmp) {
                free(buf);
                peko_close_socket(st->sock);
                free(st);
                return "Error: out of memory";
            }
            buf      = tmp;
            capacity = next;
        }

        int n = br_sslio_read(&st->ioc, buf + length, PEKO_READ_CHUNK);
        if (n <= 0)
            break;
        length += (size_t)n;
    }

    char *gc_buf = (char *)pgc_alloc_atomic(length + 1);
    if (!gc_buf) {
        free(buf);
        peko_close_socket(st->sock);
        free(st);
        return "Error: out of memory";
    }

    memcpy(gc_buf, buf, length);
    gc_buf[length] = '\0';

    free(buf);
    peko_close_socket(st->sock);
    free(st);
    return gc_buf;
}

void peko_stream_abort_tls(void *handle)
{
    peko_send_stream_tls_t *st = (peko_send_stream_tls_t *)handle;
    if (!st)
        return;
    if (!st->closed)
        peko_close_socket(st->sock);
    free(st);
}

/* =========================================================================
 * WebSocket client TLS transport
 *
 * Provides the wss:// byte transport for peko_websocket_client.c. The BearSSL
 * contexts must outlive the handshake, so they live in one heap struct that is
 * never moved (the engine and sslio hold interior pointers into it). Trust
 * matches the rest of this file: the insecure client engine above.
 * ====================================================================== */

typedef struct {
    peko_socket_t         sock;
    br_ssl_client_context sc;
    insecure_x509_context xc;
    br_sslio_context      ioc;
    unsigned char         iobuf[BR_SSL_BUFSIZE_BIDI];
} ws_tls_ctx;

static int ws_tls_read(void *ctx, unsigned char *buf, size_t len)
{
    ws_tls_ctx *t = (ws_tls_ctx *)ctx;
    int n = br_sslio_read(&t->ioc, buf, len);
    return n; /* >0 bytes, <0 on close/error */
}

static int ws_tls_write_all(void *ctx, const unsigned char *buf, size_t len)
{
    ws_tls_ctx *t = (ws_tls_ctx *)ctx;
    if (tls_write_all_stable(&t->ioc, (const char *)buf, len) != 0)
        return -1;
    br_sslio_flush(&t->ioc);
    return 0;
}

static void ws_tls_close(void *ctx)
{
    ws_tls_ctx *t = (ws_tls_ctx *)ctx;
    if (!t)
        return;
    if (t->sock != PEKO_INVALID_SOCKET)
        peko_close_socket(t->sock);
    free(t);
}

int peko_ws_tls_transport_connect(const char *host, int port, ws_transport_t *out)
{
    ws_tls_ctx *t = (ws_tls_ctx *)malloc(sizeof(*t));
    if (!t)
        return -1;

    t->sock = tcp_connect(host, port);
    if (t->sock == PEKO_INVALID_SOCKET) {
        free(t);
        return -1;
    }

    br_ssl_client_init_full(&t->sc, &t->xc.inner, NULL, 0);
    t->xc.vtable = &insecure_x509_vtable;
    br_ssl_engine_set_x509(&t->sc.eng, &t->xc.vtable);
    br_ssl_engine_set_buffer(&t->sc.eng, t->iobuf, sizeof(t->iobuf), 1);
    if (!br_ssl_client_reset(&t->sc, host, 0)) {
        peko_close_socket(t->sock);
        free(t);
        return -1;
    }
    /* The read/write callbacks take a peko_socket_t*; &t->sock is stable for the
     * life of this heap struct. */
    br_sslio_init(&t->ioc, &t->sc.eng, sock_read, &t->sock, sock_write, &t->sock);

    out->ctx = t;
    out->read = ws_tls_read;
    out->write_all = ws_tls_write_all;
    out->close = ws_tls_close;
    return 0;
}
