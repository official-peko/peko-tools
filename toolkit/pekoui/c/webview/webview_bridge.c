/* Bridge between the webview JS-to-native bind callback and a Peko closure.
 *
 * webview_bind registers a global JavaScript function that, when called, runs a
 * C callback of the form void(const char *seq, const char *req, void *arg). The
 * bridge stores the Peko closure behind that callback and forwards the request
 * string to it, then returns the closure's result to the pending JavaScript
 * call through webview_return.
 *
 * The webview core and the pgc collector symbols are defined elsewhere (the
 * verbatim library in webview.cc and the GC runtime), so they are declared
 * extern here. */

#include <stdlib.h>

typedef void *webview_t;

extern void webview_bind(webview_t w, const char *name,
                         void (*fn)(const char *seq, const char *req, void *arg),
                         void *arg);
extern void webview_return(webview_t w, const char *seq, int status,
                           const char *result);

/* A pgc_handle keeps a managed object reachable and its address current across
 * collections and object moves. */
typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_begin_blocking(void);
extern void       pgc_end_blocking(void);

/* A bound handler: the webview, the Peko closure's raw function pointer, and a
 * handle keeping the closure's managed environment reachable for the lifetime
 * of the binding. */
typedef struct {
    webview_t   w;
    char     *(*fn)(void *ctx, const char *req);
    pgc_handle  ctx;
} peko_webview_binding;

/* The C callback the webview invokes when the bound JavaScript function runs.
 *
 * It fires on the UI thread inside webview_run, which is parked for the
 * blocking event loop. The closure allocates managed memory (the request
 * string and its result), which a parked thread must not do, so the trampoline
 * ends the blocking section for the duration of the call and begins it again
 * before returning to the loop. The closure context's address is re-resolved
 * through its handle immediately before the call, so a collection that moved it
 * is accounted for. The request pointer is a stable buffer owned by the
 * webview for the call; the closure copies it into managed memory. */
static void peko_webview_bind_trampoline(const char *seq, const char *req,
                                         void *arg)
{
    peko_webview_binding *binding = (peko_webview_binding *)arg;

    pgc_end_blocking();

    void *ctx    = pgc_handle_get(binding->ctx);
    char *result = binding->fn(ctx, req);
    webview_return(binding->w, seq, 0, result);

    pgc_begin_blocking();
}

/* Binds a Peko closure to a global JavaScript function `name`. fn is the
 * closure's raw function pointer and ctx its managed environment. The binding
 * and its context handle live until the webview is destroyed. Called while the
 * calling thread is running, so pgc_handle_create is safe. */
void peko_webview_bind(webview_t w, const char *name, void *fn, void *ctx)
{
    peko_webview_binding *binding = malloc(sizeof(peko_webview_binding));
    binding->w   = w;
    binding->fn  = (char *(*)(void *, const char *))fn;
    binding->ctx = pgc_handle_create(ctx);
    webview_bind(w, name, peko_webview_bind_trampoline, binding);
}
