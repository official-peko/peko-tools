#include <peko.h>

PEKO_BEGIN

/* Console fd I/O primitives backing std::io. Defined in console.c. The byte
   buffer parameters are GC-managed string buffers (NUL-terminated); the calls
   read them synchronously and do not allocate, so no pin is needed. read_line
   allocates a fresh managed buffer the collector owns. */

/* Enable ANSI and UTF-8 output on Windows; a no-op on Unix. */
p_fn p_i32 peko_console_init();

/* Write `len` bytes of `buf` to `fd`. Returns bytes written, or -1. */
p_fn p_i32 peko_write_fd(p_i32 fd, p_gc(p_i8) buf, p_i32 len);

/* Write a NUL-terminated buffer to `fd`. Returns bytes written, or -1. */
p_fn p_i32 peko_write_fd_string(p_i32 fd, p_gc(p_i8) str);

/* Flush the write buffer for `fd`. Returns 0, or -1. */
p_fn p_i32 peko_flush_fd(p_i32 fd);

/* Read one line from `fd` (newline excluded) into a fresh managed buffer.
   Returns the buffer, or null on error. */
p_fn p_gcsafe p_gc(p_i8) peko_read_line(p_i32 fd);

/* Open `path` with a flag bitmask. Returns the fd, or -1. */
p_fn p_i32 peko_open_fd(p_gc(p_i8) path, p_i32 flags);

/* Close a previously opened fd. Returns 0, or -1. */
p_fn p_i32 peko_close_fd(p_i32 fd);

/* Open the platform null device for writing. Returns the fd, or -1. */
p_fn p_i32 peko_open_null_fd();

PEKO_END
