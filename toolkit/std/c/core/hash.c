/*
 * hash.c
 * Value-hashing primitives for std::core. The Hash trait implementations on
 * the value types call these so equal values hash equally and a Map can pick a
 * bucket. The results are folded to a non-negative 31-bit range, which an f64
 * (and so a `number`) represents exactly.
 *
 * These read their inputs synchronously and never allocate, so they reach no
 * safepoint and the managed byte buffer they read cannot move during the call.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

/* Keep the result non-negative and exactly representable in an f64. */
#define PEKO_HASH_MASK 0x7fffffffu

/* FNV-1a over `len` bytes of `data`. */
int64_t peko_hash_bytes(const int8_t *data, int64_t len)
{
    uint32_t hash = 2166136261u; /* FNV offset basis */
    for (int64_t i = 0; i < len; i++) {
        hash ^= (uint8_t)data[i];
        hash *= 16777619u; /* FNV prime */
    }
    return (int64_t)(hash & PEKO_HASH_MASK);
}

/*
 * Hash an f64 by its full bit pattern, so two equal values (including their
 * decimals) hash equally and distinct values spread across buckets. -0.0 is
 * collapsed to +0.0 first, since the two compare equal and so must hash equal.
 */
int64_t peko_hash_f64(double value)
{
    if (value == 0.0)
        value = 0.0;

    uint64_t bits;
    memcpy(&bits, &value, sizeof bits);

    uint32_t folded = (uint32_t)(bits ^ (bits >> 32));
    return (int64_t)(folded & PEKO_HASH_MASK);
}

/* Byte-equality of two managed buffers, by length then content. Backs the
   Equals trait on string so equal strings compare equal regardless of
   identity. Reads synchronously and never allocates. */
_Bool peko_bytes_equal(const int8_t *a, int64_t a_len, const int8_t *b,
                       int64_t b_len)
{
    if (a_len != b_len)
        return 0;
    return memcmp(a, b, (size_t)a_len) == 0;
}
