// peko.h - the Peko FFI and runtime header.
//
// C and Objective-C interop sources include this header so the p_* aliases
// expand to concrete C types. The Peko FFI parser reads the same aliases,
// unexpanded, to map declarations marked with the p_fn and p_var markers to
// Peko FFI types.
//
// Mapping from a p_* alias to its Peko FFI type:
//
//   p_ch         i8
//   p_i1         i1
//   p_i8         i8
//   p_i16        i16
//   p_i32        i32
//   p_i64        i64
//   p_i128       i128
//   p_f16        f16
//   p_f32        f32
//   p_f64        f64
//   p_bool       i1
//   p_cstr       cstr
//   p_opaque     opaque
//   p_gc_opaque  pointer<void>
//   p_gc(T)      pointer<T>

#ifndef PEKO_H
#define PEKO_H

#include <stdint.h>

// Scalar types. A C boolean lowers to LLVM i1; the `bool` value type wraps it.
// A C char is a raw 8-bit scalar; the `char` value type wraps an i8.
#define p_ch   char
#define p_i1   _Bool
#define p_i8   int8_t
#define p_i16  int16_t
#define p_i32  int32_t
#define p_i64  int64_t
#define p_f16  _Float16
#define p_f32  float
#define p_f64  double
#define p_bool _Bool

// 128-bit integer needs C23 _BitInt.
#if defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 202311L)
#define p_i128 _BitInt(128)
#endif

// Pointers. p_gc_opaque and p_gc(T) are GC-managed: the collector traces them
// and may move them. p_opaque is an unmanaged malloc or OS handle. p_cstr is a
// C string.
#define p_gc_opaque void *
#define p_opaque    void *
#define p_cstr      const char *
#define p_gc(T)     T *

// Declaration markers. p_fn marks a function declaration and p_var marks a
// variable declaration as part of the Peko FFI surface. The Peko FFI parser
// reads them to find the declarations that cross into Peko. Both lead the
// declaration and expand to nothing for the C compiler.
#define p_fn
#define p_var

// Function attribute for a p_fn declaration, placed before the return type.
// p_gcsafe signifies that the C function can trigger a garbage collection,
// block, or call back into Peko. The Peko compiler keeps calls to a gcsafe
// function as GC safepoints, so live managed pointers stay correct across the
// call. An FFI function without this attribute is a leaf and does not collect.
// p_gcsafe expands to nothing for the C compiler.
#define p_gcsafe

// Linkage guard. Paste PEKO_BEGIN before the first declaration and PEKO_END
// after the last so a C++ compiler gives the declarations C linkage. Both
// expand to nothing in C. The FFI parser ignores them.
#ifdef __cplusplus
#define PEKO_BEGIN extern "C" {
#define PEKO_END }
#else
#define PEKO_BEGIN
#define PEKO_END
#endif

// ===========================================================================
// GC and runtime ABI.
//
// A future session adds the runtime allocation and write-barrier declarations
// here so C interop can call them without redeclaring the ABI. The set is
// expected to include at least:
//
//   pgc_alloc_atomic        allocate a GC buffer with no traced children
//   pgc_alloc_managed       allocate a traced GC object from a descriptor
//   peko_gc_write_barrier   record a store into a traced field
//   peko_gc_add_global_root register a global root with the collector
//
// Each declaration uses the p_* aliases for its parameter and return types so
// the C compiler and the FFI parser agree on the boundary types.
// ===========================================================================

#endif // PEKO_H
