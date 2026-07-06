#include <peko.h>

PEKO_BEGIN

/* Native desktop menu bar, defined per platform in c/menu/. The menu is built
   imperatively into a single application menu: begin resets it and takes the
   app name so the platform can synthesize its standard application menu,
   submenu opens a top-level submenu that following item, separator, and role
   calls fill, and apply installs the bar and registers the click callback.
   Desktop only; the calls are no-ops on iOS and Android.

   begin creates the standard application menu automatically (the bold one under
   the app name on macOS), so the first user submenu is not absorbed as the app
   menu. app_open reopens that application menu so callers can append extra
   entries to it; on platforms without an application menu it is a no-op.

   apply's callback is a Peko closure of (cstr action_id) -> void, passed as its
   raw function pointer and managed context. It is invoked on the UI thread when
   a plain item is chosen. role is the integer from pekoui::menu::role_code.
   apply's window is the native webview handle; desktop platforms attach a
   per-window menu bar to its OS window, and macOS ignores it (global bar). */
p_fn void peko_menu_begin(p_gc(p_i8) app_name);
p_fn void peko_menu_app_open(void);
p_fn void peko_menu_submenu(p_gc(p_i8) label);
p_fn void peko_menu_item(p_gc(p_i8) label, p_gc(p_i8) action_id, p_gc(p_i8) accelerator);
p_fn void peko_menu_separator(void);
p_fn void peko_menu_role(p_gc(p_i8) label, p_i32 role);
p_fn p_gcsafe void peko_menu_apply(p_opaque callback, p_gc_opaque context, p_opaque window);

PEKO_END
