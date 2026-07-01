#include <peko.h>

PEKO_BEGIN

/* Legacy MD5 and SHA-1 streaming contexts, defined in legacy.c. The context is
   an unmanaged malloc handle threaded through init, update, and final; final
   frees it. update reads a managed input buffer and final writes the digest
   into a caller-allocated managed buffer (16 bytes for MD5, 20 for SHA-1).
   Neither allocates, so neither is gcsafe. These algorithms are broken and
   serve compatibility only. */

p_fn p_opaque md5_context_allocate();
p_fn void md5_init_binded(p_opaque ctx);
p_fn void md5_update_binded(p_opaque ctx, p_gc(p_i8) data, p_i32 len);
p_fn void md5_final_binded(p_opaque ctx, p_gc(p_i8) hash);

p_fn p_opaque sha1_context_allocate();
p_fn void sha1_init_binded(p_opaque ctx);
p_fn void sha1_update_binded(p_opaque ctx, p_gc(p_i8) data, p_i32 len);
p_fn void sha1_final_binded(p_opaque ctx, p_gc(p_i8) hash);

PEKO_END
