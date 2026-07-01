#include <peko.h>

PEKO_BEGIN

/* The desktop webview surface backing std::webview, defined in webview.cc.
   The library is Serge Zaitsev's single-header webview: WKWebView on macOS,
   WebKitGTK on Linux, WebView2 on Windows.

   A webview_t is an unmanaged library handle the caller owns and destroys.
   String parameters are GC-managed buffers read synchronously by the native
   call, which copies the text into the native widget and returns, so no
   collection can move the buffer mid-call. */

/* Create and destroy. window is null for a fresh top-level window. */
p_fn p_opaque webview_create(p_i32 debug, p_opaque window);
p_fn void webview_destroy(p_opaque w);

/* Run the native event loop until terminated, then stop it. The caller
   brackets webview_run with pgc_begin_blocking and pgc_end_blocking. */
p_fn void webview_run(p_opaque w);
p_fn void webview_terminate(p_opaque w);

/* Window chrome. hints are the WebViewHint size-hint values. */
p_fn void webview_set_title(p_opaque w, p_gc(p_i8) title);
p_fn void webview_set_size(p_opaque w, p_i32 width, p_i32 height, p_i32 hints);

/* Content. navigate loads a URL, set_html loads a literal document. */
p_fn void webview_navigate(p_opaque w, p_gc(p_i8) url);
p_fn void webview_set_html(p_opaque w, p_gc(p_i8) html);

/* JavaScript. init injects code that runs at the start of every page load;
   eval runs code once in the current page. */
p_fn void webview_init(p_opaque w, p_gc(p_i8) js);
p_fn void webview_eval(p_opaque w, p_gc(p_i8) js);

/* Binds a Peko closure to a global JavaScript function `name`. fn is the
   closure's raw function pointer, ctx its managed environment. When the JS
   function runs, the closure receives the request string (a JSON array of the
   call arguments) and returns the JSON result. Defined in webview_bridge.c. */
p_fn void peko_webview_bind(p_opaque w, p_gc(p_i8) name, p_opaque callback, p_gc_opaque ctx);

/* The GC parks the calling thread across the blocking event loop. */
p_fn p_gcsafe void pgc_begin_blocking();
p_fn void pgc_end_blocking();

PEKO_END
