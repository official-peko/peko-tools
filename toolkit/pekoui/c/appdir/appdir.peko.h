#include <peko.h>

PEKO_BEGIN

/* Returns a writable per-app data directory, creating <home>/.peko/<app_id>.
   The result is a static buffer valid until the next call; the caller copies it
   into managed memory immediately. Defined in c/platform/peko_platform.c. */
p_fn p_cstr peko_app_data_dir(p_gc(p_i8) app_id);

PEKO_END
