#include <peko.h>

PEKO_BEGIN

/* C string and memory primitives from libc, used by the std::core value types
   for string length, copying, and comparison. libc is always linked, so these
   need no [native] sources of their own; this header only declares them. */

p_fn p_i64 strlen(p_cstr str);
p_fn void memcpy(p_gc_opaque dst, p_opaque src, p_i64 count);
p_fn p_i32 strcmp(p_cstr left, p_cstr right);

PEKO_END
