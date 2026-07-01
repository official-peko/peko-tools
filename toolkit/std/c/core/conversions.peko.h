#include <peko.h>

PEKO_BEGIN

/* Value <-> string conversion primitives backing the std::runtime helpers.
   The to-string side formats into a fresh GC-managed atomic byte buffer the
   collector owns; the from-string side parses a raw C string. libc is always
   linked, so the formatting and parsing use snprintf / strtoll / strtod. */

/* Copy a raw C string into a fresh GC-managed atomic byte buffer (NUL byte
   included) and return it. The collector owns the buffer; it has no traced
   children. */
p_fn p_gcsafe p_gc(p_i8) peko_managed_from_cstr(p_cstr src);

/* The length (excluding the NUL) of a managed byte buffer. */
p_fn p_i64 peko_buffer_length(p_gc(p_i8) buffer);

/* Format a scalar into a fresh GC-managed atomic byte buffer (NUL included). */
p_fn p_gcsafe p_gc(p_i8) peko_int_to_cstr(p_i64 value);
p_fn p_gcsafe p_gc(p_i8) peko_float_to_cstr(p_f64 value);
p_fn p_gcsafe p_gc(p_i8) peko_bool_to_cstr(p_i1 value);
p_fn p_gcsafe p_gc(p_i8) peko_char_to_cstr(p_i8 value);

/* Parse a managed string's byte buffer into a scalar. The buffer is read
   synchronously and nothing here allocates, so no pin is needed. */
p_fn p_i64 peko_cstr_to_int(p_gc(p_i8) text);
p_fn p_f64 peko_cstr_to_float(p_gc(p_i8) text);
p_fn p_i1 peko_cstr_to_bool(p_gc(p_i8) text);
p_fn p_i8 peko_cstr_to_char(p_gc(p_i8) text);

/* Optional unwrap halt. peko_halt_begin prints the failure header, then one
   peko_halt_frame per context frame (newest first), then peko_halt_end flushes
   and exits the process. */
p_fn void peko_halt_begin(p_i1 is_error);
p_fn void peko_halt_frame(p_gc(p_i8) file, p_i32 line, p_i32 character);
p_fn void peko_halt_end();

PEKO_END
