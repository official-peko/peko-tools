/*
 * peko_sockets.h
 * Shared types, constants, and function declarations for the Peko sockets
 * library. Include this header in both peko_sockets.c and peko_websocket.c.
 */

#ifndef PEKO_SOCKETS_H
#define PEKO_SOCKETS_H

/* -------------------------------------------------------------------------
 * Platform detection and includes
 * ---------------------------------------------------------------------- */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <winsock2.h>
#  include <ws2tcpip.h>
#  pragma comment(lib, "ws2_32.lib")
   typedef SOCKET peko_socket_t;
#  define PEKO_INVALID_SOCKET INVALID_SOCKET
#  define peko_close_socket(s) closesocket(s)
#  define peko_send(s, buf, len) send((s), (buf), (int)(len), 0)
#  define peko_recv(s, buf, len) recv((s), (buf), (int)(len), 0)
#  define peko_shutdown_write(s) shutdown((s), SD_SEND)
   /* MSVC names the case-insensitive compare _strnicmp, not strncasecmp. */
#  define strncasecmp _strnicmp
#else
#  include <arpa/inet.h>
#  include <errno.h>
#  include <fcntl.h>
#  include <netdb.h>
#  include <netinet/in.h>
#  include <netinet/tcp.h>
#  include <sys/socket.h>
#  include <sys/types.h>
#  include <unistd.h>
   typedef int peko_socket_t;
#  define PEKO_INVALID_SOCKET (-1)
#  define peko_close_socket(s) do { shutdown((s), SHUT_RDWR); close((s)); } while (0)
#  define peko_send(s, buf, len) send((s), (buf), (len), 0)
#  define peko_recv(s, buf, len) recv((s), (buf), (len), 0)
#  define peko_shutdown_write(s) shutdown((s), SHUT_WR)
#endif

/* -------------------------------------------------------------------------
 * Buffer constants
 * ---------------------------------------------------------------------- */

/* Initial size for the dynamic response buffer in create_request. */
#define PEKO_RESPONSE_INITIAL_SIZE  4096
/* Hard cap on dynamic response buffer growth (16 MB). */
#define PEKO_RESPONSE_MAX_SIZE      (16 * 1024 * 1024)
/* Read chunk size for incoming socket data. */
#define PEKO_READ_CHUNK             4096
/* Read chunk size for incoming WebSocket frames. */
#define PEKO_WS_READ_CHUNK          4096

/* -------------------------------------------------------------------------
 * Peko GC interface
 * Implemented in the pgc runtime, included via include/pgc.h in each .c file.
 * Use pgc_alloc_atomic for non-pointer data, or pgc_handle_create/get/release
 * to keep managed objects alive across blocking calls.
 * ---------------------------------------------------------------------- */

/* -------------------------------------------------------------------------
 * WebSocket opcodes
 * ---------------------------------------------------------------------- */

#define WS_OPCODE_CONT  0x0
#define WS_OPCODE_TXT   0x1
#define WS_OPCODE_BIN   0x2
#define WS_OPCODE_CLOSE 0x8
#define WS_OPCODE_PING  0x9
#define WS_OPCODE_PONG  0xA

/* -------------------------------------------------------------------------
 * WebSocket message structure
 * ---------------------------------------------------------------------- */

/*
 * Holds a decoded WebSocket frame. `payload` points into a caller-managed
 * buffer. Do not free it separately.
 */
typedef struct {
    uint8_t  opcode;
    size_t   payload_length;
    char    *payload;
} ws_message_t;

/* -------------------------------------------------------------------------
 * Function declarations - peko_sockets.c
 * ---------------------------------------------------------------------- */

/*
 * Creates a TCP listen socket bound to *port.
 * If *port is 0, the OS assigns a free port and writes it back to *port.
 * Returns a valid socket fd/SOCKET on success, PEKO_INVALID_SOCKET on error.
 */
peko_socket_t peko_create_listen_socket(int port);

/*
 * Returns the local TCP port a socket is bound to, in host byte order, or 0
 * when it cannot be read.
 */
int peko_socket_local_port(peko_socket_t sock);

/*
 * Accepts exactly one connection on listen_socket, reads the full request
 * into a dynamically allocated buffer, calls handler(data, request_buf),
 * streams the response back to the client, then closes the client connection.
 *
 * handler   - callback: given the user context pointer and the null-terminated
 *             request string, returns a null-terminated response string.
 * data      - opaque user context forwarded to handler unchanged.
 *
 * Returns 0 on success, non-zero on error. The caller owns listen_socket
 * and must close it separately when done.
 */
int peko_accept_connection(peko_socket_t   listen_socket,
                           char          *(*handler)(void *, char *),
                           void           *data);

/*
 * Connects to host:port, sends request in full (streamed writes), reads
 * the complete response into a GC-managed buffer, and returns it.
 * The returned pointer is owned by the Peko GC.
 * Returns a non-null error string (starting with "Error:") on failure.
 */
const char *peko_create_request(const char *host, int port,
                                const char *request);

/*
 * Fire-and-forget send. Connects to host:port, sends request in full, then
 * shuts down the write half so the peer reaches end-of-stream at once.
 * Does not read a response. Returns 0 on success and non-zero on failure.
 */
int peko_create_request_oneshot(const char *host, int port,
                                const char *request);

/*
 * HTTPS sibling of peko_create_request. Connects to host:port, performs a
 * TLS handshake, sends request in full, reads the complete response into a
 * GC-managed buffer, and returns it. The contract matches peko_create_request:
 * the caller supplies the full request bytes and receives the full raw
 * response (status line, headers, and body).
 * The returned pointer is owned by the Peko GC.
 * Returns a non-null error string (starting with "Error:") on failure.
 *
 * The TLS layer does not verify server certificates. The connection is
 * encrypted but not authenticated. See peko_sockets_tls.c.
 */
const char *peko_create_request_tls(const char *host, int port,
                                    const char *request);

/*
 * Streaming sibling of peko_create_request. Connects to host:port, sends
 * request, then reads the response incrementally. on_headers is called once
 * with a NUL-terminated string containing the status line and header block,
 * before any body bytes. A non-zero return from on_headers stops the read.
 * on_chunk is called repeatedly with body bytes (de-chunked when the response
 * uses Transfer-Encoding: chunked). on_chunk returns true to keep reading or
 * false to stop. headers_ctx and chunk_ctx are forwarded unchanged to the
 * respective callbacks.
 *
 * Returns 0 on a clean read or callback-requested stop. Returns non-zero on
 * a transport error.
 */
int peko_stream_request(const char *host, int port, const char *request,
                        int  (*on_headers)(void *, const char *),
                        void *headers_ctx,
                        bool (*on_chunk)(void *, const char *, size_t),
                        void *chunk_ctx);

/*
 * Streaming sibling of peko_create_request_tls. Same contract as
 * peko_stream_request, with TLS framing on the transport.
 */
int peko_stream_request_tls(const char *host, int port, const char *request,
                            int  (*on_headers)(void *, const char *),
                            void *headers_ctx,
                            bool (*on_chunk)(void *, const char *, size_t),
                            void *chunk_ctx);

/* -------------------------------------------------------------------------
 * Send-streaming entry points
 *
 * peko_open_stream_request connects to host:port and sends the request line
 * and headers (the caller's request_head plus an appended Transfer-Encoding
 * header). It returns an opaque malloc'd handle on success or NULL on
 * failure.
 *
 * peko_stream_write_chunk frames each chunk of body bytes and writes it.
 * Returns 0 on success, non-zero on error. A zero-length write is a no-op.
 *
 * peko_stream_finish writes the terminating zero chunk, reads the full
 * response into a GC-managed string, and frees the handle. Returns the
 * response or an "Error:" string.
 *
 * peko_stream_abort frees the handle without writing a terminator. For
 * cleanup paths.
 *
 * The TLS sibling has identical semantics with the connection running over
 * a BearSSL session.
 * ---------------------------------------------------------------------- */

void       *peko_open_stream_request(const char *host, int port,
                                     const char *request_head);
int         peko_stream_write_chunk(void *handle, const char *bytes, int len);
const char *peko_stream_finish(void *handle);
void        peko_stream_abort(void *handle);

void       *peko_open_stream_request_tls(const char *host, int port,
                                         const char *request_head);
int         peko_stream_write_chunk_tls(void *handle, const char *bytes,
                                        int len);
const char *peko_stream_finish_tls(void *handle);
void        peko_stream_abort_tls(void *handle);

/* -------------------------------------------------------------------------
 * Function declarations - peko_websocket.c
 * ---------------------------------------------------------------------- */

/*
 * Accepts one connection on listen_socket, returning the client fd or -1 on
 * failure. The thread parks during accept so a collection can proceed. Pair
 * with peko_ws_serve to handle each connection on its own thread.
 */
int peko_ws_accept(peko_socket_t listen_socket);

/*
 * Serves one accepted connection: performs the HTTP upgrade handshake, then
 * calls handler for the connection's lifecycle. handler is called as
 * handler(data, event, client_fd, text) where event is 0 (open), 1 (message),
 * or 2 (close); text carries the frame for a message and is empty otherwise.
 * Ping frames are answered automatically. The caller runs this on a dedicated
 * thread, so many connections are served at once.
 *
 * Returns 0 when the connection closes cleanly, non-zero on handshake error.
 */
int peko_ws_serve(peko_socket_t   client,
                  void          (*handler)(void *, int, peko_socket_t, char *),
                  void           *data);

/*
 * Accepts and serves one connection on the calling thread (peko_ws_accept then
 * peko_ws_serve). Retained for single-connection callers.
 * Returns 0 when the connection closes cleanly, non-zero on error.
 */
int peko_ws_accept_connection(peko_socket_t   listen_socket,
                              void          (*handler)(void *, int, peko_socket_t, char *),
                              void           *data);

/*
 * Encodes text as a WebSocket text frame and sends it to socket.
 * Handles frames of any length correctly without using strlen on
 * the encoded frame bytes.
 * Returns the number of bytes sent, or -1 on error.
 */
int peko_ws_send_text(peko_socket_t socket, const char *text);

/* -------------------------------------------------------------------------
 * Function declarations - peko_websocket_client.c (outbound WS client)
 *
 * A client that dials a WebSocket server (the pekoui native bridge dials the
 * hosted `/__peko__`). Both `ws://` (plain) and `wss://` (TLS) are supported;
 * the TLS transport lives in peko_sockets_tls.c and is reached through the
 * factory below so the framing code carries no BearSSL dependency.
 * ---------------------------------------------------------------------- */

/*
 * A byte transport under the WebSocket framing: either a plain socket or a TLS
 * session. read returns >0 bytes read, or <=0 on close/error; write_all returns
 * 0 on success, -1 on error; close tears the transport down and frees ctx.
 */
typedef struct {
    void *ctx;
    int  (*read)(void *ctx, unsigned char *buf, size_t len);
    int  (*write_all)(void *ctx, const unsigned char *buf, size_t len);
    void (*close)(void *ctx);
} ws_transport_t;

/*
 * Open a TLS byte transport to host:port (implemented in peko_sockets_tls.c).
 * Returns 0 and fills *out on success, -1 on failure. Certificate trust matches
 * the rest of the std TLS layer (the documented BearSSL insecure client engine).
 */
int peko_ws_tls_transport_connect(const char *host, int port, ws_transport_t *out);

/*
 * Connect to a WebSocket URL (ws:// or wss://) and complete the upgrade
 * handshake. `subprotocol` and `extra_headers` may be NULL; `extra_headers`, if
 * given, is inserted verbatim into the handshake (each line CRLF-terminated),
 * for the bridge token cookie. Returns an opaque client handle, or NULL on
 * failure. Free with peko_ws_client_close.
 */
void *peko_ws_client_connect(const char *url, const char *subprotocol,
                             const char *extra_headers);

/* Send text as a masked text frame. Returns 0 on success, -1 on error. */
int peko_ws_client_send(void *client, const char *text);

/*
 * Block for the next text message, answering ping/pong internally. Returns a
 * malloc'd, NUL-terminated string the caller owns, or NULL when the connection
 * closes or errors.
 */
char *peko_ws_client_recv(void *client);

/* Free a message returned by peko_ws_client_recv, after the caller has copied it
 * into a managed string. */
void peko_ws_client_free_message(char *msg);

/* Send a close frame (best effort), tear down the transport, and free. */
void peko_ws_client_close(void *client);


/*
 * Connects to host:port, sends request, then streams the response body
 * directly into the file at fpath in chunks of chunk_size bytes.
 * Strips the HTTP response headers and writes only the body to the file.
 * Creates or overwrites fpath on success.
 *
 * chunk_size - number of bytes to read and write per iteration.
 *              Pass 0 to use the default (PEKO_READ_CHUNK).
 *
 * Returns 0 on success, non-zero on error.
 */
int peko_download_to_file(const char *host, int port, const char *request,
                          const char *fpath, int chunk_size);

#endif /* PEKO_SOCKETS_H */
