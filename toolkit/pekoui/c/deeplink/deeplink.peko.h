#include <peko.h>

PEKO_BEGIN

/* Native deep-link handling, defined per platform in c/deeplink/. When the OS
   launches or activates the app with a registered custom-scheme URL, the URL
   is delivered here, reduced to the path that follows the scheme, and handed to
   the Peko handler so the app can navigate to that route.

   set_handler's callback is a Peko closure of (cstr path) -> void, passed as its
   raw function pointer and managed context. It is invoked on the UI thread with
   the incoming route path (a leading-slash path such as "/settings") when a URL
   arrives while the app runs. Passing a null callback clears the handler. macOS
   delivers live through this callback; the other platforms wire live delivery
   later.

   take_initial returns the route path the app was launched with, when the OS
   opened it via a registered-scheme URL, or an empty string otherwise. It is
   read once at startup so the launch route can be applied before the UI mounts.
   Desktop platforms read it from the process arguments; Android reads it from
   the launch intent; macOS returns empty and delivers through the callback.

   register asks the OS to route the given scheme's URLs to this app, for the
   platforms that need a runtime registration. Windows writes the scheme into
   the per-user registry so `start scheme://path` launches the app with the
   URL. The other platforms register the scheme through their bundle launch
   config, so the call is a no-op there. Passing an empty scheme does nothing.
   name is the app's display name, used for the installed desktop entry title on
   Linux and ignored elsewhere. */
p_fn p_gcsafe void peko_deeplink_set_handler(p_opaque callback, p_gc_opaque context);
p_fn p_cstr peko_deeplink_take_initial(void);
p_fn void peko_deeplink_register(p_gc(p_i8) scheme, p_gc(p_i8) name);

/* set_window records the native window handle so a forwarded deep link can
   raise the running window to the front. Windows and Linux use it; macOS
   activates through the system event and the mobile platforms have no windows,
   so their call is a no-op. */
p_fn void peko_deeplink_set_window(p_opaque window);

/* single_instance makes the app a single running instance for the scheme, so a
   later launch with a URL reuses this window instead of opening a second one.
   Called once at startup, before the rest of the app starts. When another
   instance already runs, this call forwards the launch URL to it and exits the
   process; otherwise it returns and this process becomes the owner, delivering
   a forwarded URL through the deep-link callback. macOS is already single
   instance through the system, so its call is a no-op. */
p_fn void peko_deeplink_single_instance(p_gc(p_i8) scheme);

PEKO_END
