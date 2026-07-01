#include <peko.h>

PEKO_BEGIN

/* Value-hashing primitives backing the Hash trait on the core value types.
   Defined in hash.c. Both fold to a non-negative 31-bit value an f64 holds
   exactly. They read their input synchronously and never allocate, so the
   managed byte buffer cannot move during the call and no pin is needed. */

/* FNV-1a over a managed byte buffer. */
p_fn p_i64 peko_hash_bytes(p_gc(p_i8) data, p_i64 len);

/* Hash an f64 by its full bit pattern. */
p_fn p_i64 peko_hash_f64(p_f64 value);

/* Byte-equality of two managed buffers, by length then content. */
p_fn p_i1 peko_bytes_equal(p_gc(p_i8) a, p_i64 a_len, p_gc(p_i8) b, p_i64 b_len);

PEKO_END
