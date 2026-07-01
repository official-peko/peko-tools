#include <peko.h>

PEKO_BEGIN

/* Filesystem primitives backing std::fs, defined in fs.c. Path and write-text
   parameters are GC-managed string buffers read synchronously. A handle is an
   unmanaged FILE* the caller owns and closes. The string reads allocate a
   fresh managed buffer the collector owns, so they are gcsafe. */

/* Metadata. */
p_fn p_i1 fs_exists(p_gc(p_i8) fpath);
p_fn p_i32 fs_get_mode(p_gc(p_i8) fpath);
p_fn p_i1 fs_is_directory(p_gc(p_i8) fpath);
p_fn p_i1 fs_is_regular(p_gc(p_i8) fpath);
p_fn p_i1 fs_is_link(p_gc(p_i8) fpath);
p_fn p_i1 fs_is_block(p_gc(p_i8) fpath);
p_fn p_i1 fs_chmod(p_gc(p_i8) fpath, p_i32 mode);

/* Handle open and close. The handle is an unmanaged FILE*. */
p_fn p_opaque fs_open_handle(p_gc(p_i8) fpath, p_i32 mode);
p_fn void fs_close_handle(p_opaque handle);

/* String reads into a fresh managed NUL-terminated buffer, or null. */
p_fn p_gcsafe p_gc(p_i8) fs_read_string(p_opaque handle, p_i32 n);
p_fn p_gcsafe p_gc(p_i8) fs_read_all_string(p_opaque handle);

/* Writes, seek, tell, flush. */
p_fn p_i32 fs_write_string(p_opaque handle, p_gc(p_i8) text);
p_fn p_i32 fs_seek(p_opaque handle, p_i64 offset, p_i32 origin);
p_fn p_i64 fs_tell(p_opaque handle);
p_fn p_i32 fs_flush(p_opaque handle);

/* Filesystem operations. */
p_fn p_i1 fs_mkdir(p_gc(p_i8) dirpath);
p_fn p_i1 fs_remove(p_gc(p_i8) fpath);
p_fn p_i1 fs_copy(p_gc(p_i8) src, p_gc(p_i8) dst);
p_fn p_i1 fs_move(p_gc(p_i8) src, p_gc(p_i8) dst);

/* Convenience helpers that open, operate, and close in one call. */
p_fn p_gcsafe p_gc(p_i8) fs_helper_read_file(p_gc(p_i8) fpath);
p_fn p_i1 fs_helper_write_file(p_gc(p_i8) fpath, p_gc(p_i8) text);
p_fn p_i1 fs_helper_append_file(p_gc(p_i8) fpath, p_gc(p_i8) text);
p_fn p_i1 fs_helper_copy(p_gc(p_i8) src, p_gc(p_i8) dst);
p_fn p_i1 fs_helper_move(p_gc(p_i8) src, p_gc(p_i8) dst);

/* Binary reads and writes. The buffer is a caller-allocated managed buffer
   filled or drained synchronously, so these do not allocate and are not
   gcsafe. */
p_fn p_i64 fs_remaining(p_opaque handle);
p_fn p_i32 fs_read_into(p_opaque handle, p_gc(p_i8) buf, p_i32 n);
p_fn p_i32 fs_write_from(p_opaque handle, p_gc(p_i8) buf, p_i32 n);

/* Directory iteration. The handle is an unmanaged OS directory stream; each
   entry name is unmanaged and valid until the next step or close. */
p_fn p_opaque fs_dir_open(p_gc(p_i8) dirpath);
p_fn p_cstr fs_dir_next(p_opaque handle);
p_fn void fs_dir_close(p_opaque handle);

/* True when the file's contents differ from the snapshot bytes. */
p_fn p_i1 fs_content_changed(p_gc(p_i8) fpath, p_gc(p_i8) snapshot, p_i32 snapshot_len);

PEKO_END
