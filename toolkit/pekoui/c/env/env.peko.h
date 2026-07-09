#include <peko.h>

PEKO_BEGIN

/* Reads an environment variable, returning the empty string when it is unset.
   The result points into the process environment; the caller copies it into
   managed memory immediately. Defined in c/env/peko_env.c. */
p_fn p_cstr peko_env(p_gc(p_i8) name);

/* Sets an environment variable in the current process, so a spawned child
   inherits it. Defined in c/env/peko_env.c. */
p_fn void peko_env_set(p_gc(p_i8) name, p_gc(p_i8) value);

/* The path of the running executable, empty when it cannot be resolved. The
   result is a static buffer copied into managed memory by the caller right
   away. Defined in c/env/peko_env.c. */
p_fn p_cstr peko_env_self_exe();

/* The host operating system as a stable identifier: "macos", "windows",
   "linux", "android", or "unknown". Defined in c/env/peko_env.c. */
p_fn p_cstr peko_env_os();

PEKO_END
