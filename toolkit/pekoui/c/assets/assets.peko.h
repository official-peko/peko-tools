#include <peko.h>

PEKO_BEGIN

/* The asset HTTP server backing pekoui::assets, defined in
   c/assets/peko_asset_server.c. It binds a dynamic loopback port and serves
   GET requests under the /_assets/ prefix by streaming asset bytes from the
   platform bundle, or from a directory on disk when a debug directory is
   given. Range requests are honored so media can seek. */

/* Start the server on a fresh loopback port. debug_dir is a directory to serve
   from during development, or an empty buffer to serve from the platform
   bundle. The native call copies the directory synchronously before returning,
   so a GC buffer is safe here. Returns the bound port, or 0 on failure. */
p_fn p_i32 peko_asset_server_start(p_gc(p_i8) debug_dir);

/* The port the server is bound to, or 0 when it is not running. */
p_fn p_i32 peko_asset_server_port();

/* Stop the server and release its resources. */
p_fn void peko_asset_server_stop();

PEKO_END
