#include <peko.h>

PEKO_BEGIN

/* Reads an environment variable, returning the empty string when it is unset.
   The result points into the process environment; the caller copies it into
   managed memory immediately. Defined in c/env/peko_env.c. */
p_fn p_cstr peko_env(p_gc(p_i8) name);

PEKO_END
