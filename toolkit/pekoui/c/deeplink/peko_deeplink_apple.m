/*
 * peko_deeplink_apple.m
 *
 * Deep-link delivery on Apple platforms. On macOS the app receives a
 * custom-scheme URL through the Get URL Apple event, the mechanism the system
 * uses to open a registered URL scheme whether the app is launched cold or
 * already running. The URL is reduced to the route path that follows the
 * scheme and handed to a Peko closure that fires on the main thread inside the
 * parked webview run loop, mirroring the webview bind trampoline: unpark, run
 * the closure, repark. On iOS the app delegate (in webview_ios.m) hands the
 * launch URL to peko_deeplink_ios_deliver, which stores the route for
 * take_initial to return once the bridge asks for it.
 *
 * macOS builds this file without ARC (the macOS toolchain sets no -fobjc-arc),
 * so memory is managed manually.
 */

#if defined(__APPLE__)
#include <TargetConditionals.h>

/* The handler is a Peko closure: its raw function pointer and its managed
 * context, kept reachable across collections by a handle. */
typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_handle_release(pgc_handle handle);
extern void       pgc_begin_blocking(void);
extern void       pgc_end_blocking(void);

typedef void (*peko_deeplink_callback)(void *context, const char *path);

#if TARGET_OS_IPHONE

#include <stdio.h>
#include <string.h>

static char g_ios_route[2048];
static int  g_ios_taken = 0;

/* Store an incoming URL as the launch route. The app delegate calls this from
 * application:didFinishLaunchingWithOptions: (cold launch) and
 * application:openURL:options: (while running). The route is whatever follows
 * the scheme's "://", with a leading slash ensured. */
void peko_deeplink_ios_deliver(const char *url)
{
    if (!url)
        return;
    const char *marker = strstr(url, "://");
    const char *route  = marker ? marker + 3 : url;
    if (route[0] == '/')
        snprintf(g_ios_route, sizeof(g_ios_route), "%s", route);
    else
        snprintf(g_ios_route, sizeof(g_ios_route), "/%s", route);
}

/* Live delivery through the callback is wired later; the launch route flows
 * through take_initial instead. */
void peko_deeplink_set_handler(void *callback, void *context)
{
    (void)callback;
    (void)context;
}

const char *peko_deeplink_take_initial(void)
{
    if (g_ios_taken)
        return "";
    if (g_ios_route[0]) {
        g_ios_taken = 1;
        return g_ios_route;
    }
    return "";
}

void peko_deeplink_register(const char *scheme)
{
    (void)scheme;
}

#else /* macOS */

#import <Cocoa/Cocoa.h>
#include <stdio.h>
#include <string.h>

static peko_deeplink_callback g_deeplink_cb      = NULL;
static pgc_handle             g_deeplink_ctx      = 0;
static int                    g_deeplink_ctx_set  = 0;

/* The target the Get URL Apple event is routed to. It reads the event's URL,
 * reduces it to the path after the scheme, and forwards it to the Peko
 * callback. */
@interface PekoDeepLinkTarget : NSObject
- (void)handleGetURLEvent:(NSAppleEventDescriptor *)event
           withReplyEvent:(NSAppleEventDescriptor *)reply;
@end

@implementation PekoDeepLinkTarget
- (void)handleGetURLEvent:(NSAppleEventDescriptor *)event
           withReplyEvent:(NSAppleEventDescriptor *)reply
{
    (void)reply;
    NSString *url = [[event paramDescriptorForKeyword:keyDirectObject] stringValue];
    if (!url || !g_deeplink_cb)
        return;

    const char *full = [url UTF8String];
    if (!full)
        return;

    /* The route is whatever follows the scheme's "://". A URL with no path
     * yields "/". A leading slash is ensured so the route is a path. */
    const char *marker = strstr(full, "://");
    const char *route  = marker ? marker + 3 : full;
    char path[2048];
    if (route[0] == '/')
        snprintf(path, sizeof(path), "%s", route);
    else
        snprintf(path, sizeof(path), "/%s", route);

    /* Bring the app to the front, so a URL opened while it runs in the
     * background raises its window. */
    [[NSApplication sharedApplication] activateIgnoringOtherApps:YES];

    /* The event fires on the main thread inside the parked webview run loop.
     * Unpark to run the Peko closure, which allocates managed memory, then
     * repark. Re-resolve the context through its handle in case a collection
     * moved it. */
    pgc_end_blocking();
    void *context = g_deeplink_ctx_set ? pgc_handle_get(g_deeplink_ctx) : NULL;
    g_deeplink_cb(context, path);
    pgc_begin_blocking();
}
@end

static PekoDeepLinkTarget *g_deeplink_target = nil;

void peko_deeplink_set_handler(void *callback, void *context)
{
    g_deeplink_cb = (peko_deeplink_callback)callback;

    if (g_deeplink_ctx_set) {
        pgc_handle_release(g_deeplink_ctx);
        g_deeplink_ctx_set = 0;
    }
    if (context) {
        g_deeplink_ctx     = pgc_handle_create(context);
        g_deeplink_ctx_set = 1;
    }

    /* Register the Get URL event handler once. The system routes a registered
     * scheme's URL to it for the app's lifetime. */
    if (!g_deeplink_target) {
        g_deeplink_target = [[PekoDeepLinkTarget alloc] init];
        [[NSAppleEventManager sharedAppleEventManager]
            setEventHandler:g_deeplink_target
                andSelector:@selector(handleGetURLEvent:withReplyEvent:)
              forEventClass:kInternetEventClass
                 andEventID:kAEGetURL];
    }
}

/* macOS delivers the URL live through the Get URL event, so there is no launch
 * route to return. */
const char *peko_deeplink_take_initial(void)
{
    return "";
}

/* Apple platforms register the scheme through the Info.plist CFBundleURLTypes,
 * so no runtime registration is needed. */
void peko_deeplink_register(const char *scheme, const char *name)
{
    (void)scheme;
    (void)name;
}

#endif /* TARGET_OS_IPHONE */

/* Apple platforms are single instance through the system: a launch while the
 * app runs activates it and delivers the URL live, so there is nothing to
 * forward. */
void peko_deeplink_single_instance(const char *scheme)
{
    (void)scheme;
}

/* macOS raises the app through the Get URL event handler; iOS has no window. */
void peko_deeplink_set_window(void *window)
{
    (void)window;
}

#endif /* __APPLE__ */
