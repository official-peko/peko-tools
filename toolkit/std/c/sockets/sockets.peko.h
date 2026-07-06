#include <peko.h>

PEKO_BEGIN

/* Socket platform layer backing std::sockets, defined in peko_sockets.c,
   peko_sockets_tls.c, and peko_websocket.c over the BSD socket API and a
   vendored BearSSL for TLS. Listen sockets and client fds are plain integers.
   Stream handles are unmanaged malloc handles the caller owns. Callbacks are a
   closure's raw function pointer (passed as an opaque) paired with its managed
   context (passed as a managed pointer the C side keeps alive with a GC handle
   across blocking reads).

   String arguments are managed buffers passed as the raw bytes of a Peko
   string. Functions that build a response return it as a C string the Peko
   side copies into a managed string, so a static error string and a heap
   response are returned the same way. */

/* The GC parks the calling thread for a blocking call. The C entry points
   bracket their own blocking sections, so Peko callers do not. */
p_fn p_gcsafe void pgc_begin_blocking();
p_fn void pgc_end_blocking();

/* One-shot requests. Each connects, sends the full request, and returns the
   full raw response (status line, headers, body) or a string starting with
   "Error:". The tls form runs the exchange over BearSSL. */
p_fn p_gcsafe p_cstr peko_create_request(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request);
p_fn p_gcsafe p_cstr peko_create_request_tls(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request);

/* Fire-and-forget send. Connects, sends the request, shuts the write half, and
   does not read a reply. Returns 0 on success. */
p_fn p_gcsafe p_i32 peko_create_request_oneshot(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request);

/* Streaming reads. on_headers is a closure function of (cstr) returning i32,
   called once with the status line and header block; a non-zero return stops
   the read. on_chunk is a closure function of (cstr, i64) returning i1, called
   with each body chunk; it returns true to keep reading. The contexts are the
   closures' managed environments. Returns 0 on a clean read. */
p_fn p_gcsafe p_i32 peko_stream_request(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request,
    p_opaque on_headers, p_gc_opaque headers_ctx,
    p_opaque on_chunk, p_gc_opaque chunk_ctx);
p_fn p_gcsafe p_i32 peko_stream_request_tls(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request,
    p_opaque on_headers, p_gc_opaque headers_ctx,
    p_opaque on_chunk, p_gc_opaque chunk_ctx);

/* Send-streaming. open returns an unmanaged handle after sending the request
   head, write frames one body chunk, finish writes the terminator and returns
   the full response (or an "Error:" string) and frees the handle, abort frees
   the handle without finishing. The tls forms run over BearSSL. */
p_fn p_gcsafe p_opaque peko_open_stream_request(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request_head);
p_fn p_gcsafe p_i32 peko_stream_write_chunk(p_opaque handle, p_gc(p_i8) bytes, p_i32 len);
p_fn p_gcsafe p_cstr peko_stream_finish(p_opaque handle);
p_fn p_gcsafe void peko_stream_abort(p_opaque handle);
p_fn p_gcsafe p_opaque peko_open_stream_request_tls(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request_head);
p_fn p_gcsafe p_i32 peko_stream_write_chunk_tls(p_opaque handle, p_gc(p_i8) bytes, p_i32 len);
p_fn p_gcsafe p_cstr peko_stream_finish_tls(p_opaque handle);
p_fn p_gcsafe void peko_stream_abort_tls(p_opaque handle);

/* Streams a response body straight to a file at fpath, in chunks of chunk_size
   bytes (0 for the default). Returns 0 on success. */
p_fn p_gcsafe p_i32 peko_download_to_file(p_gc(p_i8) host, p_i32 port, p_gc(p_i8) request, p_gc(p_i8) fpath, p_i32 chunk_size);

/* Listen socket. create binds to port (0 lets the OS choose) and returns the
   fd, or a non-positive value on error. local_port returns the bound port. */
p_fn p_gcsafe p_i32 peko_create_listen_socket(p_i32 port);
p_fn p_i32 peko_socket_local_port(p_i32 socket);

/* Accepts one connection on listen_socket, reads the full request, calls the
   handler closure (function of (cstr) returning cstr, the response to send),
   sends the returned response, and closes the connection. Returns 0 on
   success. */
p_fn p_gcsafe p_i32 peko_accept_connection(p_i32 listen_socket, p_opaque handler, p_gc_opaque data);

/* WebSocket server. accept returns the next client fd on listen_socket, or -1
   on failure. serve performs the upgrade handshake on a client fd and calls the
   handler closure (function of (i32 event, i32 fd, cstr text): event 0 open, 1
   message, 2 close; text empty except for a message) until the connection
   closes; run it on its own thread to serve many connections at once. Returns 0
   on a clean close. accept_connection accepts then serves on the calling thread
   for single-connection callers. send_text sends text as a frame on a client fd
   and returns the bytes sent, or -1 on error. */
p_fn p_gcsafe p_i32 peko_ws_accept(p_i32 listen_socket);
p_fn p_gcsafe p_i32 peko_ws_serve(p_i32 client, p_opaque handler, p_gc_opaque data);
p_fn p_gcsafe p_i32 peko_ws_accept_connection(p_i32 listen_socket, p_opaque handler, p_gc_opaque data);
p_fn p_gcsafe p_i32 peko_ws_send_text(p_i32 socket, p_gc(p_i8) text);

PEKO_END
