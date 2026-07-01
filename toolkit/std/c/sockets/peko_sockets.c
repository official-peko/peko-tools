/*
 * peko_sockets.c
 * Platform socket I/O for the Peko sockets library.
 * Handles TCP listen sockets, per-connection accept, and outbound requests.
 * WebSocket protocol logic lives in peko_websocket.c.
 */

#include "peko_sockets.h"

extern void *pgc_alloc_atomic(size_t size);
#include <pgc.h>

/* GC handle API (precise pgc runtime). A handle is a stable reference that
 * survives collections and object moves; pgc_handle_get returns the object's
 * current address. Used to keep the listener's closure context alive and
 * relocation-safe across the whole accept loop, the same way the threads
 * package keeps a thread's closure context alive until the thread attaches.
 * These are declared here in case include/pgc.h does not expose them; the
 * declarations are compatible with the runtime's definitions. */
#ifndef PEKO_PGC_HANDLE_DECLARED
#define PEKO_PGC_HANDLE_DECLARED
typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_handle_release(pgc_handle handle);
#endif

/* -------------------------------------------------------------------------
 * Internal helpers
 * ---------------------------------------------------------------------- */

/*
 * Sets a receive timeout on sock. The response read loop ends when a read
 * returns zero or negative. A server that holds the connection open without
 * closing it would otherwise block the read loop forever. The timeout bounds
 * that wait so the loop ends and the buffer read so far is returned.
 */
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

/*
 * Reads from sock into a dynamically growing buffer until the remote side
 * closes the connection or stops sending.  Returns a null-terminated string
 * in a malloc'd buffer.  The caller must free() the result.
 * Returns NULL on allocation failure or hard read error.
 */
static char *read_full(peko_socket_t sock)
{
    size_t capacity = PEKO_READ_CHUNK;
    size_t length   = 0;
    char  *buf      = (char *)malloc(capacity);

    if (!buf)
        return NULL;

    /* ------------------------------------------------------------------ */
    /* Phase 1: read available data.                                      */
    /* For an HTTP request, stop once the end-of-headers marker           */
    /* (\r\n\r\n) is seen. For a non-HTTP (raw TCP) message there is no    */
    /* such marker, so reading continues until the peer closes the        */
    /* connection, which is the end-of-message signal for a raw           */
    /* request/response exchange. have_headers records which case applies */
    /* so the HTTP body phase below runs only for HTTP requests.          */
    /* ------------------------------------------------------------------ */
    bool have_headers = false;
    for (;;) {
        /* Grow if needed. Reserve one extra byte for the terminating NUL
         * written after the loop, so a recv that fills the buffer exactly
         * cannot push that NUL one byte past the allocation. */
        if (length + PEKO_READ_CHUNK + 1 > capacity) {
            capacity *= 2;
            char *tmp = (char *)realloc(buf, capacity);
            if (!tmp) {
                free(buf);
                return NULL;
            }
            buf = tmp;
        }

        pgc_begin_blocking();
        int n = (int)peko_recv(sock, buf + length, PEKO_READ_CHUNK);
        pgc_end_blocking();
        if (n < 0) {
            free(buf);
            return NULL;
        }
        if (n == 0)
            break; /* connection closed */

        length += (size_t)n;

        /* Check whether the end-of-headers marker is anywhere in the buffer,
         * not just at the tail: a single recv may deliver headers and body
         * together, so we search the whole buffer. */
        for (size_t i = 0; i + 3 < length; i++) {
            if (buf[i] == '\r' && buf[i+1] == '\n' &&
                buf[i+2] == '\r' && buf[i+3] == '\n') {
                have_headers = true;
                break;
            }
        }
        if (have_headers)
            break;
    }

    /* ------------------------------------------------------------------ */
    /* Phase 2 (HTTP only): read the body if Content-Length is present.   */
    /* POST requests from browsers (especially WebKit/Safari) often send  */
    /* the body in a separate TCP segment. Without this phase the body is */
    /* silently discarded and POST data never reaches the Peko callback.  */
    /* Phase 2 also handles Transfer-Encoding: chunked requests by        */
    /* reading until the terminating zero-size chunk appears. The chunk   */
    /* framing is left in place; Peko's HttpRequest.get_body strips it.   */
    /* Skipped entirely for non-HTTP messages, which have no header block. */
    /* ------------------------------------------------------------------ */
    if (have_headers) {
        /* Locate the header block so we can scan it for Content-Length. */
        size_t header_end_offset = 0;
        for (size_t i = 0; i + 3 < length; i++) {
            if (buf[i] == '\r' && buf[i+1] == '\n' &&
                buf[i+2] == '\r' && buf[i+3] == '\n') {
                header_end_offset = i + 4;
                break;
            }
        }

        /* Parse Content-Length and detect Transfer-Encoding: chunked
         * (both case-insensitive). A request with chunked encoding has no
         * Content-Length; the body ends at a zero-size chunk terminator. */
        size_t content_length = 0;
        int    is_chunked     = 0;
        const char *p = buf;
        const char *hdrs_end = buf + header_end_offset;
        while (p < hdrs_end) {
            if (strncasecmp(p, "Content-Length:", 15) == 0) {
                const char *v = p + 15;
                while (v < hdrs_end && (*v == ' ' || *v == '\t'))
                    v++;
                content_length = (size_t)strtoul(v, NULL, 10);
            } else if (strncasecmp(p, "Transfer-Encoding:", 18) == 0) {
                const char *v = p + 18;
                while (v < hdrs_end && (*v == ' ' || *v == '\t'))
                    v++;
                if (strncasecmp(v, "chunked", 7) == 0)
                    is_chunked = 1;
            }
            /* Advance to next header line. */
            while (p < hdrs_end && *p != '\r' && *p != '\n')
                p++;
            while (p < hdrs_end && (*p == '\r' || *p == '\n'))
                p++;
        }

        /* How many body bytes did we already receive with the headers? */
        size_t body_received = (header_end_offset > 0)
                                ? length - header_end_offset : 0;

        /* Read remaining body bytes if any are still outstanding. */
        if (content_length > body_received) {
            size_t remaining = content_length - body_received;
            size_t needed    = length + remaining + 1;

            if (needed > capacity) {
                char *tmp = (char *)realloc(buf, needed);
                if (!tmp) {
                    free(buf);
                    return NULL;
                }
                buf      = tmp;
                capacity = needed;
            }

            while (remaining > 0) {
                pgc_begin_blocking();
                int n = (int)peko_recv(sock, buf + length,
                                       remaining < (size_t)PEKO_READ_CHUNK
                                           ? remaining
                                           : (size_t)PEKO_READ_CHUNK);
                pgc_end_blocking();
                if (n <= 0)
                    break;
                length    += (size_t)n;
                remaining -= (size_t)n;
            }
        } else if (is_chunked) {
            /* Read until the chunked terminator (\r\n0\r\n\r\n) appears
             * anywhere in the body region of the buffer, or until the peer
             * closes. The terminator may straddle the header boundary in
             * the rare case where the entire body arrived with the headers,
             * so the scan starts from the beginning of the body region. */
            for (;;) {
                /* Look for the terminator within bytes already received. */
                int found = 0;
                if (length >= 5) {
                    size_t start = header_end_offset;
                    /* Scan for "0\r\n\r\n" preceded by either CRLF or the
                     * header boundary. The pattern "\r\n0\r\n\r\n" marks
                     * the zero-size final chunk after a previous chunk's
                     * CRLF; the bare "0\r\n\r\n" form covers the case
                     * where the body starts directly with a zero chunk. */
                    for (size_t i = start; i + 4 < length; i++) {
                        if (buf[i] == '0'
                            && buf[i+1] == '\r' && buf[i+2] == '\n'
                            && buf[i+3] == '\r' && buf[i+4] == '\n') {
                            /* Verify the "0" stands alone as a chunk-size
                             * line: previous byte must be the start of the
                             * body or end a CRLF. */
                            if (i == start
                                || (i >= 2 && buf[i-2] == '\r'
                                          && buf[i-1] == '\n')) {
                                found = 1;
                                break;
                            }
                        }
                    }
                }
                if (found)
                    break;

                /* Grow if needed before the next read. */
                if (length + PEKO_READ_CHUNK + 1 > capacity) {
                    capacity *= 2;
                    char *tmp = (char *)realloc(buf, capacity);
                    if (!tmp) {
                        free(buf);
                        return NULL;
                    }
                    buf = tmp;
                }

                pgc_begin_blocking();
                int n = (int)peko_recv(sock, buf + length, PEKO_READ_CHUNK);
                pgc_end_blocking();
                if (n <= 0)
                    break;
                length += (size_t)n;
            }
        }
    }

    buf[length] = '\0';
    return buf; /* caller frees and copies to GC memory */
}

/*
 * Sends all bytes in buf over sock, retrying on partial writes.
 * Returns 0 on success, -1 on error.
 */
static int send_all(peko_socket_t sock, const char *buf, size_t len)
{
    /* buf may be a cstr that points into a managed String buffer, which the
     * collector can move while this thread is parked in the send below. Copy
     * into a stable malloc buffer first, while this thread is still running
     * and the source cannot move, then send from the copy. */
    if (len == 0)
        return 0;

    char *stable = (char *)malloc(len);
    if (!stable)
        return -1;
    memcpy(stable, buf, len);

    size_t sent = 0;
    int    rc   = 0;
    while (sent < len) {
        pgc_begin_blocking();
        int n = (int)peko_send(sock, stable + sent, len - sent);
        pgc_end_blocking();
        if (n <= 0) {
            rc = -1;
            break;
        }
        sent += (size_t)n;
    }
    free(stable);
    return rc;
}

/*
 * Applies standard socket options to a listen socket:
 *   SO_REUSEADDR  - allow rapid rebind after restart
 *   TCP_NODELAY   - disable Nagle for lower latency
 */
static void apply_listen_opts(peko_socket_t sock)
{
    int yes = 1;
    setsockopt(sock, SOL_SOCKET, SO_REUSEADDR,
               (const char *)&yes, sizeof(yes));
#ifdef TCP_NODELAY
    setsockopt(sock, IPPROTO_TCP, TCP_NODELAY,
               (const char *)&yes, sizeof(yes));
#endif
}

/* -------------------------------------------------------------------------
 * peko_create_listen_socket
 * ---------------------------------------------------------------------- */

peko_socket_t peko_create_listen_socket(int port)
{
    struct addrinfo hints, *res = NULL;
    peko_socket_t   sock;
    char            port_str[16];
    int             rc;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    hints.ai_flags    = AI_PASSIVE;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    rc = getaddrinfo(NULL, port_str, &hints, &res);
    pgc_end_blocking();
    if (rc != 0 || !res)
        return PEKO_INVALID_SOCKET;

    sock = socket(res->ai_family, res->ai_socktype, res->ai_protocol);
    if (sock == PEKO_INVALID_SOCKET) {
        freeaddrinfo(res);
        return PEKO_INVALID_SOCKET;
    }

    apply_listen_opts(sock);

    if (bind(sock, res->ai_addr, (int)res->ai_addrlen) != 0) {
        freeaddrinfo(res);
        peko_close_socket(sock);
        return PEKO_INVALID_SOCKET;
    }

    freeaddrinfo(res);

    if (listen(sock, SOMAXCONN) != 0) {
        peko_close_socket(sock);
        return PEKO_INVALID_SOCKET;
    }

    return sock;
}

/*
 * Returns the local TCP port a socket is bound to, in host byte order, or 0
 * when it cannot be read. The caller uses this after peko_create_listen_socket
 * with port 0 to learn the port the OS assigned.
 */
int peko_socket_local_port(peko_socket_t sock)
{
    struct sockaddr_in sin;
    socklen_t          len = sizeof(sin);
    if (getsockname(sock, (struct sockaddr *)&sin, &len) == 0)
        return (int)ntohs(sin.sin_port);
    return 0;
}

/* -------------------------------------------------------------------------
 * peko_accept_connection
 * ---------------------------------------------------------------------- */


/* -------------------------------------------------------------------------
 * Multipart form data parser
 *
 * Parses a multipart/form-data body and reconstructs it as a simple
 * key=value&key2=value2 body so the Peko callback sees a consistent
 * format regardless of how the browser encoded the form submission.
 *
 * WebKit/Safari always uses multipart/form-data for form submissions.
 * Chrome and Firefox use application/x-www-form-urlencoded by default.
 * Without this parser, WebKit POST data arrives but is unparseable.
 * ---------------------------------------------------------------------- */

/* URL-encodes a string into dst. dst must be at least src_len*3+1 bytes. */
static size_t url_encode(const char *src, size_t src_len,
                         char *dst, size_t dst_cap)
{
    static const char hex[] = "0123456789ABCDEF";
    size_t out = 0;
    for (size_t i = 0; i < src_len && out + 4 < dst_cap; i++) {
        unsigned char c = (unsigned char)src[i];
        if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
            (c >= '0' && c <= '9') ||
            c == '-' || c == '_' || c == '.' || c == '~') {
            dst[out++] = (char)c;
        } else if (c == ' ') {
            dst[out++] = '+';
        } else {
            dst[out++] = '%';
            dst[out++] = hex[c >> 4];
            dst[out++] = hex[c & 0xf];
        }
    }
    dst[out] = '\0';
    return out;
}

/*
 * Extracts a header parameter value.
 * e.g. extract_param("form-data; name=\"foo\"", "name", buf, bufsz)
 *      writes "foo" to buf.
 * Returns 1 on success, 0 if the parameter is not found.
 */
static int extract_param(const char *header, const char *param,
                         char *buf, size_t bufsz)
{
    const char *p = header;
    size_t      plen = strlen(param);

    while (*p) {
        /* skip whitespace and semicolons */
        while (*p == ' ' || *p == '\t' || *p == ';') p++;
        if (strncasecmp(p, param, plen) == 0 && p[plen] == '=') {
            p += plen + 1;
            size_t out = 0;
            if (*p == '"') {
                p++;
                while (*p && *p != '"' && out + 1 < bufsz)
                    buf[out++] = *p++;
            } else {
                while (*p && *p != ';' && *p != '\r' &&
                       *p != '\n' && out + 1 < bufsz)
                    buf[out++] = *p++;
            }
            buf[out] = '\0';
            return 1;
        }
        /* skip to next semicolon */
        while (*p && *p != ';') p++;
    }
    return 0;
}

/*
 * Parses a multipart/form-data body and returns a new malloc'd buffer
 * containing the request with the body replaced by a URL-encoded
 * key=value string.  Non-file text fields are combined; file fields are
 * included as filename=<name>.
 *
 * headers_end  - pointer to first byte after the header block (i.e. body start)
 * boundary     - the boundary string (without leading --)
 * full_buf     - the full request buffer (headers + body); will be replaced
 * full_len     - length of full_buf
 *
 * Returns a new malloc'd buffer the caller must free, or NULL on failure.
 */
static char *parse_multipart(const char *headers_end,
                              const char *boundary,
                              const char *full_buf,
                              size_t      full_len)
{
    /* Build the boundary marker we search for: --<boundary> */
    size_t blen       = strlen(boundary);
    size_t marker_len = blen + 2;
    char  *marker     = (char *)malloc(marker_len + 1);
    if (!marker) return NULL;
    marker[0] = '-'; marker[1] = '-';
    memcpy(marker + 2, boundary, blen);
    marker[marker_len] = '\0';

    /* Output buffer for the reconstructed body (URL-encoded). */
    size_t out_cap  = 4096;
    size_t out_len  = 0;
    char  *out      = (char *)malloc(out_cap);
    if (!out) { free(marker); return NULL; }

    const char *p    = headers_end;
    const char *end  = full_buf + full_len;
    int         first = 1;

    while (p < end) {
        /* Find next boundary marker */
        const char *bstart = NULL;
        for (const char *s = p; s + marker_len <= end; s++) {
            if (memcmp(s, marker, marker_len) == 0) {
                bstart = s;
                break;
            }
        }
        if (!bstart) break;

        const char *part_start = bstart + marker_len;

        /* Check for terminal boundary (--<boundary>--) */
        if (part_start + 2 <= end &&
            part_start[0] == '-' && part_start[1] == '-')
            break;

        /* Skip CRLF after boundary */
        if (part_start + 1 < end &&
            part_start[0] == '\r' && part_start[1] == '\n')
            part_start += 2;

        /* Find end of this part's headers */
        const char *part_hdrs_end = NULL;
        for (const char *s = part_start; s + 3 < end; s++) {
            if (s[0]=='\r' && s[1]=='\n' && s[2]=='\r' && s[3]=='\n') {
                part_hdrs_end = s + 4;
                break;
            }
        }
        if (!part_hdrs_end) break;

        /* Find start of next boundary to know where this part's body ends */
        const char *next_boundary = NULL;
        for (const char *s = part_hdrs_end; s + marker_len <= end; s++) {
            if (memcmp(s, marker, marker_len) == 0) {
                next_boundary = s;
                break;
            }
        }
        if (!next_boundary) break;

        /* Part body is between part_hdrs_end and next_boundary,
         * minus the trailing CRLF before the boundary. */
        const char *body_start = part_hdrs_end;
        const char *body_end   = next_boundary;
        if (body_end - 2 >= body_start &&
            body_end[-2] == '\r' && body_end[-1] == '\n')
            body_end -= 2;

        size_t body_len = (size_t)(body_end - body_start);

        /* Parse Content-Disposition from this part's headers */
        char name[256]     = "";
        char filename[256] = "";
        const char *ph = part_start;
        while (ph < part_hdrs_end - 2) {
            if (strncasecmp(ph, "Content-Disposition:", 20) == 0) {
                ph += 20;
                while (*ph == ' ') ph++;
                const char *line_end = ph;
                while (line_end < part_hdrs_end &&
                       *line_end != '\r' && *line_end != '\n')
                    line_end++;
                /* Copy header line for parsing */
                size_t hlen = (size_t)(line_end - ph);
                char *hcopy = (char *)malloc(hlen + 1);
                if (hcopy) {
                    memcpy(hcopy, ph, hlen);
                    hcopy[hlen] = '\0';
                    extract_param(hcopy, "name",     name,     sizeof(name));
                    extract_param(hcopy, "filename", filename, sizeof(filename));
                    free(hcopy);
                }
            }
            /* advance past this header line */
            while (ph < part_hdrs_end &&
                   (*ph != '\r' && *ph != '\n')) ph++;
            while (ph < part_hdrs_end &&
                   (*ph == '\r' || *ph == '\n')) ph++;
        }

        if (!name[0]) {
            p = next_boundary;
            continue;
        }

        /* Encode name and value into output buffer */
        size_t enc_name_cap  = strlen(name) * 3 + 1;
        size_t enc_value_cap = (filename[0] ? strlen(filename) : body_len) * 3 + 1;
        char  *enc_name      = (char *)malloc(enc_name_cap);
        char  *enc_value     = (char *)malloc(enc_value_cap + 1);

        if (!enc_name || !enc_value) {
            free(enc_name); free(enc_value);
            p = next_boundary;
            continue;
        }

        size_t en = url_encode(name, strlen(name), enc_name, enc_name_cap);
        size_t ev;
        if (filename[0]) {
            /* File field: use filename as value */
            ev = url_encode(filename, strlen(filename),
                            enc_value, enc_value_cap);
        } else {
            /* Text field: use body content as value */
            ev = url_encode(body_start, body_len, enc_value, enc_value_cap);
        }

        /* Grow output if needed */
        size_t needed = out_len + (first ? 0 : 1) + en + 1 + ev + 1;
        if (needed > out_cap) {
            out_cap = needed * 2;
            char *tmp = (char *)realloc(out, out_cap);
            if (!tmp) { free(enc_name); free(enc_value); break; }
            out = tmp;
        }

        if (!first) out[out_len++] = '&';
        memcpy(out + out_len, enc_name,  en); out_len += en;
        out[out_len++] = '=';
        memcpy(out + out_len, enc_value, ev); out_len += ev;
        out[out_len] = '\0';
        first = 0;

        free(enc_name);
        free(enc_value);
        p = next_boundary;
    }

    free(marker);

    /* Now build the final request: original headers + new URL-encoded body */
    /* Find the header/body split in the original request */
    size_t hdr_len = (size_t)(headers_end - full_buf);

    /* Build new Content-Length value */
    char cl_header[64];
    int  cl_len = snprintf(cl_header, sizeof(cl_header),
                           "Content-Length: %zu\r\n", out_len);

    /* Build new Content-Type header */
    const char *new_ct     = "Content-Type: application/x-www-form-urlencoded\r\n";
    size_t      new_ct_len = strlen(new_ct);

    /* Reconstruct headers, replacing Content-Type and Content-Length */
    size_t result_cap = hdr_len + (size_t)cl_len + new_ct_len + out_len + 4;
    char  *result     = (char *)malloc(result_cap);
    if (!result) { free(out); return NULL; }

    size_t rlen = 0;

    /* Copy headers line by line, skipping old Content-Type and Content-Length */
    const char *lp = full_buf;
    int         wrote_ct = 0, wrote_cl = 0;
    while (lp < headers_end) {
        const char *le = lp;
        while (le < headers_end && *le != '\r' && *le != '\n') le++;
        size_t llen = (size_t)(le - lp);

        if (strncasecmp(lp, "Content-Type:", 13) == 0) {
            /* Replace with URL-encoded content type */
            if (!wrote_ct) {
                memcpy(result + rlen, new_ct, new_ct_len);
                rlen += new_ct_len;
                wrote_ct = 1;
            }
        } else if (strncasecmp(lp, "Content-Length:", 15) == 0) {
            /* Replace with new content length */
            if (!wrote_cl) {
                memcpy(result + rlen, cl_header, (size_t)cl_len);
                rlen += (size_t)cl_len;
                wrote_cl = 1;
            }
        } else {
            /* Copy line as-is */
            if (llen > 0) {
                memcpy(result + rlen, lp, llen);
                rlen += llen;
                result[rlen++] = '\r';
                result[rlen++] = '\n';
            } else {
                /* Blank line = end of headers */
                result[rlen++] = '\r';
                result[rlen++] = '\n';
            }
        }

        /* Skip CRLF */
        while (le < headers_end && (*le == '\r' || *le == '\n')) le++;
        lp = le;
    }

    /* Append new body */
    memcpy(result + rlen, out, out_len);
    rlen += out_len;
    result[rlen] = '\0';

    free(out);
    return result;
}

/*
 * Extracts the boundary value from a Content-Type header line.
 * e.g. "multipart/form-data; boundary=----WebKitFormBoundaryXYZ"
 * Returns a malloc'd string the caller must free, or NULL.
 */
static char *extract_boundary(const char *request, size_t req_len)
{
    const char *p = request;
    const char *end = request + req_len;

    while (p < end) {
        if (strncasecmp(p, "Content-Type:", 13) == 0) {
            p += 13;
            while (*p == ' ') p++;
            /* Check if it's multipart */
            if (strncasecmp(p, "multipart/form-data", 19) != 0)
                return NULL;
            /* Find boundary parameter */
            char boundary[512] = "";
            const char *line_end = p;
            while (line_end < end && *line_end != '\r' && *line_end != '\n')
                line_end++;
            size_t llen = (size_t)(line_end - p);
            char  *lcopy = (char *)malloc(llen + 1);
            if (!lcopy) return NULL;
            memcpy(lcopy, p, llen);
            lcopy[llen] = '\0';
            int found = extract_param(lcopy, "boundary",
                                      boundary, sizeof(boundary));
            free(lcopy);
            if (found && boundary[0]) {
                char *result = (char *)malloc(strlen(boundary) + 1);
                if (result) strcpy(result, boundary);
                return result;
            }
            return NULL;
        }
        /* Advance past this line */
        while (p < end && *p != '\r' && *p != '\n') p++;
        while (p < end && (*p == '\r' || *p == '\n')) p++;
        /* Stop at header/body boundary */
        if (p + 1 < end && p[0] == '\r' && p[1] == '\n') break;
    }
    return NULL;
}

/*
 * If request is a multipart/form-data POST, parses it and returns a new
 * malloc'd buffer with the body replaced by URL-encoded key=value pairs.
 * Returns the original buffer unchanged if it is not multipart.
 * is_new is set to 1 if a new buffer was allocated (caller must free it).
 */
static char *normalize_request(char *request, size_t req_len, int *is_new)
{
    *is_new = 0;

    /* Find header/body split */
    const char *headers_end = NULL;
    for (size_t i = 0; i + 3 < req_len; i++) {
        if (request[i]   == '\r' && request[i+1] == '\n' &&
            request[i+2] == '\r' && request[i+3] == '\n') {
            headers_end = request + i + 4;
            break;
        }
    }
    if (!headers_end) return request;

    char *boundary = extract_boundary(request, req_len);
    if (!boundary) return request; /* not multipart */

    char *normalized = parse_multipart(headers_end, boundary,
                                       request, req_len);
    free(boundary);

    if (!normalized) return request;

    *is_new = 1;
    return normalized;
}

int peko_accept_connection(peko_socket_t   listen_socket,
                           char          *(*handler)(void *, char *),
                           void           *data)
{
    /* Keep the GC-managed closure context alive via a handle for the duration
     * of this call, then re-resolve its current address with pgc_handle_get
     * immediately before invoking the handler. The handle survives collections
     * and object moves that happen during the blocking reads below, so the
     * address handed to the handler is always current. This mirrors the
     * WebSocket accept path, which uses the same create/get/release pattern. */
    pgc_handle data_handle = pgc_handle_create(data);

    struct sockaddr_in client_addr;
    socklen_t          client_len = sizeof(client_addr);
    peko_socket_t      client;
    char              *request  = NULL;
    char              *response = NULL;
    int                rc       = 0;

    pgc_begin_blocking();
    client = accept(listen_socket,
                    (struct sockaddr *)&client_addr, &client_len);
    pgc_end_blocking();
    if (client == PEKO_INVALID_SOCKET) {
        pgc_handle_release(data_handle);
        return 1;
    }

    /* Read the full request. */
    request = read_full(client);
    if (!request) {
        peko_close_socket(client);
        pgc_handle_release(data_handle);
        return 1;
    }

    /* Ignore connections that produced no data at all. A client that opens
     * a socket and closes it without sending leaves an empty buffer, which
     * would otherwise reach the Peko callback as an empty Request. A
     * non-empty buffer is always dispatched, so raw TCP payloads (which have
     * no HTTP header block) are delivered unchanged. */
    if (request[0] == '\0') {
        free(request);
        peko_close_socket(client);
        pgc_handle_release(data_handle);
        return 0;
    }

    /* Normalize multipart/form-data to URL-encoded before passing to Peko.
     * This makes POST data consistent regardless of browser encoding. */
    size_t req_len  = strlen(request);
    int    is_new   = 0;
    char  *normalized = normalize_request(request, req_len, &is_new);

    /* Resolve the closure context's current address now, after all blocking
     * reads, immediately before the call. Pass the stable malloc buffer
     * straight to the handler: the cstr-to-String conversion inside the
     * handler copies it into managed memory, and a stable source cannot move
     * during that copy. A managed (pgc_alloc) buffer would be wrong here, as
     * it is unrooted across the handler and the collector could move or
     * reclaim it mid-call. */
    {
        void *live_data = pgc_handle_get(data_handle);
        response = handler(live_data, normalized);
    }

    if (is_new) free(normalized);
    free(request);
    request = NULL;

    /* Stream the full response back. send_all copies the response into a
     * stable buffer before the parked send, so a move of the response string
     * during the send cannot corrupt it. */
    if (response && *response) {
        rc = send_all(client, response, strlen(response));
    }

    peko_close_socket(client);
    pgc_handle_release(data_handle);
    return (rc == 0) ? 0 : 1;
}

/* -------------------------------------------------------------------------
 * peko_create_request
 * ---------------------------------------------------------------------- */

const char *peko_create_request(const char *host, int port,
                                const char *request)
{
    struct addrinfo hints, *res = NULL;
    peko_socket_t   sock;
    char            port_str[16];
    int             rc;

    /* --- resolve host --- */
    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;   /* accept IPv4 or IPv6 */
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (rc != 0 || !res)
        return "Error: could not resolve host";

    /* --- connect ---
     * Try every address getaddrinfo returned, not just the first. A host like
     * "localhost" resolves to both ::1 (IPv6) and 127.0.0.1 (IPv4), often with
     * IPv6 first; connecting to only the first address fails when the peer
     * listens on the other family. Walk the list and use the first address
     * that accepts. */
    sock = PEKO_INVALID_SOCKET;
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

    if (sock == PEKO_INVALID_SOCKET)
        return "Error: could not connect";

    /* Bound the response read so a server that never closes the connection
     * cannot block the read loop forever. */
    set_recv_timeout(sock, 30);

    /* --- streamed send --- */
    {
        size_t total = strlen(request);
        if (send_all(sock, request, total) != 0) {
            peko_close_socket(sock);
            return "Error: could not send request";
        }
    }

    /* --- streamed recv into growing buffer --- */
    {
        size_t capacity = PEKO_RESPONSE_INITIAL_SIZE;
        size_t length   = 0;
        char  *buf      = (char *)malloc(capacity);

        if (!buf) {
            peko_close_socket(sock);
            return "Error: out of memory";
        }

        for (;;) {
            /* Grow if needed. */
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

            pgc_begin_blocking();
            int n = (int)peko_recv(sock, buf + length, PEKO_READ_CHUNK);
            pgc_end_blocking();
            if (n < 0) {
                free(buf);
                peko_close_socket(sock);
                return "Error: could not read response";
            }
            if (n == 0)
                break; /* server closed connection */

            length += (size_t)n;
        }

        buf[length] = '\0';
        peko_close_socket(sock);

        /*
         * Copy into a GC-managed buffer so the Peko runtime can track it.
         * Free the temporary malloc buffer afterwards.
         */
        char *gc_buf = (char *)pgc_alloc_atomic((size_t)(length + 1));
        if (!gc_buf) {
            free(buf);
            return "Error: out of memory";
        }

        memcpy(gc_buf, buf, length + 1);
        free(buf);
        return gc_buf;
    }
}

/* -------------------------------------------------------------------------
 * peko_create_request_oneshot
 *
 * Fire-and-forget send. Connects to host:port, sends the full request bytes,
 * then half-closes the write side so the peer reaches end-of-stream as soon
 * as the send completes. Does not read a response. Returns 0 on success and
 * non-zero on failure.
 *
 * Use this from Peko send_no_response, especially for raw TCP messages where
 * the peer reads until EOF and has no other way to know the message is done.
 * ---------------------------------------------------------------------- */

int peko_create_request_oneshot(const char *host, int port,
                                const char *request)
{
    struct addrinfo hints, *res = NULL;
    peko_socket_t   sock;
    char            port_str[16];
    int             rc;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (rc != 0 || !res)
        return 1;

    sock = PEKO_INVALID_SOCKET;
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

    if (sock == PEKO_INVALID_SOCKET)
        return 2;

    if (send_all(sock, request, strlen(request)) != 0) {
        peko_close_socket(sock);
        return 3;
    }

    /* Half-close the write side so the peer's read loop sees EOF and stops
     * waiting for more bytes. Without this a peer reading until close (for
     * example a raw TCP server reading a message of unknown length) would
     * block until the receive-side timeout fires. */
    peko_shutdown_write(sock);

    peko_close_socket(sock);
    return 0;
}

/* -------------------------------------------------------------------------
 * Shared response helpers
 * ---------------------------------------------------------------------- */

/*
 * Locates the start of the HTTP response body by scanning for the blank
 * line that separates headers from body (\r\n\r\n).
 * Returns a pointer into buf at the first byte of the body, or NULL if
 * the header terminator is not found.
 */
static const char *find_body(const char *buf, size_t len)
{
    size_t i;
    for (i = 0; i + 3 < len; i++) {
        if (buf[i]   == '\r' && buf[i+1] == '\n' &&
            buf[i+2] == '\r' && buf[i+3] == '\n')
            return buf + i + 4;
    }
    return NULL;
}

/* -------------------------------------------------------------------------
 * Receive-streaming chunked decoder
 *
 * The decoder is a small state machine that consumes incoming TCP bytes from
 * the body of the response. The header parser hands off here once it has
 * called on_headers. Two modes are supported.
 *
 * In passthrough mode (no Transfer-Encoding: chunked) every incoming body byte
 * is forwarded directly to on_chunk in chunks the size of the recv() return.
 *
 * In chunked mode each chunk on the wire is framed as: a hex chunk size line
 * ending in CRLF, then exactly that many bytes of data, then a trailing CRLF.
 * A zero-size chunk ends the body. The decoder strips the framing bytes and
 * forwards only the data bytes to on_chunk.
 *
 * Chunk boundaries on the wire do not align with TCP read boundaries, so the
 * decoder maintains a small line buffer for in-progress chunk-size lines.
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
    size_t         remaining;        /* bytes still to read in the current chunk */
    char           line[STREAM_LINE_BUF];
    size_t         line_len;
} stream_decoder_t;

/*
 * Parses the chunk-size token in dec->line (which may contain extensions
 * after a semicolon). Returns the decoded size, or SIZE_MAX on parse error.
 */
static size_t parse_chunk_size(const stream_decoder_t *dec)
{
    size_t size  = 0;
    int    saw   = 0;
    size_t i     = 0;
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

/*
 * Feeds len bytes from buf through the decoder, calling on_chunk for every
 * data byte segment recovered. Returns 1 to keep reading, 0 to stop. The
 * decoder transitions to STREAM_DONE on a zero-size terminating chunk; the
 * caller should also stop reading when state == STREAM_DONE.
 */
/*
 * Copies len bytes from buf into a NUL-terminated stack buffer and invokes
 * on_chunk with it. The Peko closure trampoline treats the byte pointer as a
 * NUL-terminated cstr, so the chunk must be NUL-terminated before crossing
 * the FFI boundary. Returns the callback result. Bytes longer than the
 * stack buffer are split across multiple calls.
 */
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
            /* Accumulate the chunk-size line until CRLF. */
            while (i < len) {
                char c = buf[i++];
                if (c == '\n') {
                    /* Drop a trailing CR from the line buffer. */
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
            /* Skip the CRLF that terminates the chunk data. Tolerate a bare
             * LF since some servers use that.  */
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

/* -------------------------------------------------------------------------
 * peko_stream_request
 *
 * Streaming sibling of peko_create_request. Connects to host:port, sends the
 * full request, then reads the response incrementally. on_headers is called
 * once with the parsed header block. on_chunk is called repeatedly with body
 * bytes; it returns true to keep reading or false to stop.
 *
 * Returns 0 on a clean read (server closed, decoder reached the terminating
 * chunk, or on_chunk returned false). Returns non-zero on transport error.
 * ---------------------------------------------------------------------- */

int peko_stream_request(const char *host, int port, const char *request,
                        int  (*on_headers)(void *, const char *),
                        void *headers_ctx,
                        bool (*on_chunk)(void *, const char *, size_t),
                        void *chunk_ctx)
{
    struct addrinfo hints, *res = NULL;
    peko_socket_t   sock = PEKO_INVALID_SOCKET;
    char            port_str[16];
    int             rc;
    int             ret = 0;

    /* Keep both callback contexts reachable and current across the blocking
     * reads below. on_headers and on_chunk allocate, so a collection can move
     * the context objects between calls; re-resolve through the handle before
     * each call. */
    pgc_handle headers_handle = pgc_handle_create(headers_ctx);
    pgc_handle chunk_handle   = pgc_handle_create(chunk_ctx);

    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (rc != 0 || !res) {
        ret = 1;
        goto cleanup;
    }

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

    if (sock == PEKO_INVALID_SOCKET) {
        ret = 2;
        goto cleanup;
    }

    set_recv_timeout(sock, 30);

    if (send_all(sock, request, strlen(request)) != 0) {
        ret = 3;
        goto cleanup;
    }

    /* Read until we have the full header block, then call on_headers and
     * switch to body-streaming. Anything past the header terminator in the
     * same recv is processed by the body decoder before reading again. */
    char            hbuf[PEKO_READ_CHUNK * 2];
    size_t          hlen      = 0;
    int             headers_done = 0;
    stream_decoder_t dec;
    dec.state     = STREAM_PASSTHROUGH;
    dec.remaining = 0;
    dec.line_len  = 0;

    for (;;) {
        if (!headers_done && hlen >= sizeof(hbuf)) {
            /* Headers exceed our buffer: malformed response. */
            ret = 4;
            goto cleanup;
        }

        size_t want = headers_done ? PEKO_READ_CHUNK
                                   : (sizeof(hbuf) - hlen);
        char   tmp[PEKO_READ_CHUNK];
        char  *target = headers_done ? tmp : (hbuf + hlen);
        size_t cap    = headers_done ? sizeof(tmp) : want;

        pgc_begin_blocking();
        int n = (int)peko_recv(sock, target, cap);
        pgc_end_blocking();

        if (n <= 0)
            break;

        if (!headers_done) {
            hlen += (size_t)n;
            /* Look for end of headers. */
            const char *body = find_body(hbuf, hlen);
            if (body != NULL) {
                /* NUL-terminate the header block in place. The header
                 * separator is \r\n\r\n; replacing the first \r with \0
                 * keeps the rest of hbuf intact for body bytes after it. */
                size_t header_len = (size_t)(body - hbuf) - 4;
                hbuf[header_len] = '\0';

                /* Detect chunked encoding before calling on_headers, since
                 * the decoder state must be set before we hand off body
                 * bytes. */
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
                        /* Advance to the next line. */
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

                /* Feed any body bytes that arrived in the same recv. */
                size_t body_len = hlen - (header_len + 4);
                if (body_len > 0) {
                    if (!stream_decode(&dec, body, body_len,
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

        /* Body bytes. */
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
 * Send-streaming
 *
 * The send-streaming model gives Peko a writer object it pushes chunks into.
 * The C side opens the connection and sends the request line and headers
 * (with Transfer-Encoding: chunked), then returns an opaque handle to a
 * malloc'd state struct. Subsequent peko_stream_write_chunk calls frame each
 * chunk on the wire as <hex-size>CRLF<bytes>CRLF. peko_stream_finish writes
 * the terminating zero chunk, reads the full response, closes the socket,
 * and frees the handle. peko_stream_abort closes and frees without writing
 * a terminating chunk, for cleanup paths.
 *
 * The handle is malloc'd and opaque to Peko because it holds OS resources
 * (a socket fd) that the OS tracks by address; the GC must not move it.
 * ---------------------------------------------------------------------- */

typedef struct {
    peko_socket_t sock;
    int           closed;
} peko_send_stream_t;

/* Writes an HTTP/1.1 chunk to the socket as <hex-size>\r\n<bytes>\r\n.
 * Returns 0 on success, non-zero on send error. */
static int send_chunk_framed(peko_socket_t sock,
                             const char *bytes, size_t len)
{
    char header[32];
    int  hlen = snprintf(header, sizeof(header), "%zx\r\n", len);
    if (hlen <= 0)
        return 1;

    if (send_all(sock, header, (size_t)hlen) != 0)
        return 2;

    if (len > 0 && send_all(sock, bytes, len) != 0)
        return 3;

    if (send_all(sock, "\r\n", 2) != 0)
        return 4;

    return 0;
}

/*
 * Opens a streaming request connection. Sends the request line and the given
 * headers, then adds Transfer-Encoding: chunked. The caller provides only the
 * head of the request (method, path, version, and any headers); the function
 * appends Transfer-Encoding: chunked and the terminating CRLF that ends the
 * header block.
 *
 * Returns a malloc'd peko_send_stream_t * on success or NULL on failure. The
 * caller owns the handle and must call peko_stream_finish or peko_stream_abort
 * to release it.
 */
void *peko_open_stream_request(const char *host, int port,
                               const char *request_head)
{
    struct addrinfo hints, *res = NULL;
    peko_socket_t   sock;
    char            port_str[16];
    int             rc;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (rc != 0 || !res)
        return NULL;

    sock = PEKO_INVALID_SOCKET;
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

    if (sock == PEKO_INVALID_SOCKET)
        return NULL;

    set_recv_timeout(sock, 30);

    /* Send the request head as provided, then a Transfer-Encoding header,
     * then a blank line that ends the header block. The request_head from
     * Peko contains the request line and any user-supplied headers, each
     * terminated by CRLF, with no trailing blank line. */
    if (send_all(sock, request_head, strlen(request_head)) != 0) {
        peko_close_socket(sock);
        return NULL;
    }

    static const char te_and_terminator[] =
        "Transfer-Encoding: chunked\r\n\r\n";
    if (send_all(sock, te_and_terminator,
                 sizeof(te_and_terminator) - 1) != 0) {
        peko_close_socket(sock);
        return NULL;
    }

    peko_send_stream_t *st =
        (peko_send_stream_t *)malloc(sizeof(peko_send_stream_t));
    if (!st) {
        peko_close_socket(sock);
        return NULL;
    }
    st->sock   = sock;
    st->closed = 0;
    return st;
}

/*
 * Writes a chunk of body bytes through the streaming connection. The bytes
 * are framed as a single HTTP/1.1 chunk. Returns 0 on success, non-zero on
 * error. An error closes the connection and marks the handle closed; the
 * caller should still call peko_stream_abort to free the handle.
 */
int peko_stream_write_chunk(void *handle, const char *bytes, int len)
{
    peko_send_stream_t *st = (peko_send_stream_t *)handle;
    if (!st || st->closed)
        return 1;

    if (len < 0)
        return 2;

    /* An empty chunk over the wire is the terminator; treat a write of zero
     * bytes as a no-op so user code does not accidentally end the request. */
    if (len == 0)
        return 0;

    if (send_chunk_framed(st->sock, bytes, (size_t)len) != 0) {
        peko_close_socket(st->sock);
        st->closed = 1;
        return 3;
    }
    return 0;
}

/*
 * Finishes a streaming request: writes the terminating zero-size chunk, then
 * reads the full response into a GC-managed string. Closes the socket and
 * frees the handle. Returns the response string on success, or an error
 * string starting with "Error:" on failure. The handle is invalid after this
 * call regardless of outcome.
 */
const char *peko_stream_finish(void *handle)
{
    peko_send_stream_t *st = (peko_send_stream_t *)handle;
    if (!st)
        return "Error: invalid stream handle";

    if (st->closed) {
        free(st);
        return "Error: stream already closed";
    }

    /* Terminating zero-size chunk. */
    if (send_all(st->sock, "0\r\n\r\n", 5) != 0) {
        peko_close_socket(st->sock);
        free(st);
        return "Error: could not send terminator";
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

        pgc_begin_blocking();
        int n = (int)peko_recv(st->sock, buf + length, PEKO_READ_CHUNK);
        pgc_end_blocking();
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

/*
 * Closes a streaming handle without writing a terminator or reading the
 * response. For cleanup paths where the caller cannot or does not want to
 * finish the stream.
 */
void peko_stream_abort(void *handle)
{
    peko_send_stream_t *st = (peko_send_stream_t *)handle;
    if (!st)
        return;
    if (!st->closed)
        peko_close_socket(st->sock);
    free(st);
}

/* -------------------------------------------------------------------------
 * peko_download_to_file
 * ---------------------------------------------------------------------- */

int peko_download_to_file(const char *host, int port, const char *request,
                          const char *fpath, int chunk_size)
{
    struct addrinfo hints, *res = NULL;
    peko_socket_t   sock;
    char            port_str[16];
    FILE           *fp   = NULL;
    char           *buf  = NULL;
    int             csize;
    int             rc   = 1;

    csize = (chunk_size > 0) ? chunk_size : PEKO_READ_CHUNK;

    /* Resolve and connect. */
    memset(&hints, 0, sizeof(hints));
    hints.ai_family   = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    snprintf(port_str, sizeof(port_str), "%d", port);

    pgc_begin_blocking();
    int gai_rc = getaddrinfo(host, port_str, &hints, &res);
    pgc_end_blocking();
    if (gai_rc != 0 || !res)
        return 1;

    /* Try every resolved address, not just the first, so a host that resolves
     * to both IPv6 and IPv4 connects to whichever family the peer listens on. */
    sock = PEKO_INVALID_SOCKET;
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

    if (sock == PEKO_INVALID_SOCKET)
        return 1;

    /* Send the request. */
    if (send_all(sock, request, strlen(request)) != 0) {
        peko_close_socket(sock);
        return 1;
    }

    /* Open the output file for writing. */
#ifdef _WIN32
    fopen_s(&fp, fpath, "wb");
#else
    fp = fopen(fpath, "wb");
#endif
    if (!fp) {
        peko_close_socket(sock);
        return 1;
    }

    buf = (char *)malloc((size_t)csize + 1);
    if (!buf) {
        fclose(fp);
        peko_close_socket(sock);
        return 1;
    }

    /*
     * Read the first chunk, which contains the HTTP response headers.
     * Find the end of the headers and write only the body portion of
     * this first chunk to the file.
     */
    {
        pgc_begin_blocking();
        int         n        = (int)peko_recv(sock, buf, csize);
        pgc_end_blocking();
        const char *body_start;

        if (n <= 0)
            goto cleanup;

        buf[n] = '\0';
        body_start = find_body(buf, (size_t)n);

        if (!body_start) {
            /* Headers span more than one chunk. Keep reading until found. */
            size_t   hdr_capacity = (size_t)csize * 2;
            size_t   hdr_length   = (size_t)n;
            char    *hdr_buf      = (char *)malloc(hdr_capacity + 1);

            if (!hdr_buf)
                goto cleanup;

            memcpy(hdr_buf, buf, hdr_length);

            while (!body_start) {
                if (hdr_length + (size_t)csize + 1 > hdr_capacity) {
                    hdr_capacity *= 2;
                    char *tmp = (char *)realloc(hdr_buf, hdr_capacity + 1);
                    if (!tmp) {
                        free(hdr_buf);
                        goto cleanup;
                    }
                    hdr_buf = tmp;
                }

                pgc_begin_blocking();
                n = (int)peko_recv(sock, hdr_buf + hdr_length, csize);
                pgc_end_blocking();
                if (n <= 0) {
                    free(hdr_buf);
                    goto cleanup;
                }
                hdr_length += (size_t)n;
                hdr_buf[hdr_length] = '\0';
                body_start = find_body(hdr_buf, hdr_length);
            }

            /* Write the body portion of the header buffer. */
            size_t body_len = hdr_length - (size_t)(body_start - hdr_buf);
            if (body_len > 0 && fwrite(body_start, 1, body_len, fp) != body_len) {
                free(hdr_buf);
                goto cleanup;
            }

            free(hdr_buf);
        } else {
            /* Write the body portion of the first chunk. */
            size_t body_len = (size_t)n - (size_t)(body_start - buf);
            if (body_len > 0 && fwrite(body_start, 1, body_len, fp) != body_len)
                goto cleanup;
        }
    }

    /* Stream remaining body chunks directly to the file. */
    for (;;) {
        pgc_begin_blocking();
        int n = (int)peko_recv(sock, buf, csize);
        pgc_end_blocking();
        if (n < 0)
            goto cleanup;
        if (n == 0)
            break;
        if ((int)fwrite(buf, 1, (size_t)n, fp) != n)
            goto cleanup;
    }

    rc = 0;

cleanup:
    free(buf);
    fclose(fp);
    peko_close_socket(sock);
    return rc;
}
