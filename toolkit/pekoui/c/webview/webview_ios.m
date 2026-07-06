/* iOS webview backend for std::webview.
 *
 * The desktop backend in webview.cc uses AppKit, which iOS does not have, so
 * iOS provides the same C API against UIKit and WebKit here. A single WKWebView
 * fills one UIViewController inside the app window. The JavaScript-to-native
 * bind protocol matches the desktop library: a bound global function posts an
 * RPC message that this file dispatches to the Peko closure, and webview_return
 * resolves the pending JavaScript promise.
 *
 * The file compiles to nothing off iOS so it can sit alongside webview.cc in
 * the one std native build. It is built with ARC (the iOS toolchain sets
 * -fobjc-arc). */

#if defined(__APPLE__)
#include <TargetConditionals.h>
#endif

#if defined(__APPLE__) && TARGET_OS_IPHONE

#import <UIKit/UIKit.h>
#import <WebKit/WebKit.h>
#include <stdlib.h>
#include <string.h>

typedef void *webview_t;
typedef void (*webview_bind_fn)(const char *seq, const char *req, void *arg);

/* A single JavaScript-to-native binding: the Peko trampoline function pointer
 * and its managed context, both held as raw pointers the GC keeps live through
 * a handle on the Peko side. */
@interface PekoBinding : NSObject
@property(nonatomic, assign) webview_bind_fn fn;
@property(nonatomic, assign) void *arg;
@end

@implementation PekoBinding
@end

/* The webview instance. Holds the WKWebView, the bound handlers, and the
 * navigation requested before the run loop starts. */
@interface PekoWebView : NSObject <WKScriptMessageHandler>
@property(nonatomic, strong) WKWebView *web_view;
@property(nonatomic, strong) WKWebViewConfiguration *config;
@property(nonatomic, strong) NSMutableDictionary<NSString *, PekoBinding *> *bindings;
@property(nonatomic, strong) NSURLRequest *pending_request;
@property(nonatomic, strong) NSString *pending_html;
@property(nonatomic, strong) NSString *title;
- (void)apply_pending;
- (void)eval:(NSString *)js;
@end

/* iOS hosts one window per app, so the webview is a process-wide singleton the
 * app delegate reads at launch. */
static PekoWebView *g_webview = nil;

@implementation PekoWebView

- (instancetype)init {
    self = [super init];
    if (self) {
        _bindings = [NSMutableDictionary dictionary];
        _config = [[WKWebViewConfiguration alloc] init];
        /* One message handler receives every bound call. */
        [_config.userContentController addScriptMessageHandler:self name:@"__peko__"];
        /* The WKWebView is a UIView and installs gesture recognizers, so it is
           created in the app delegate after UIApplicationMain, where UIKit is
           initialized. This init runs before UIApplicationMain. Building the web
           view there from the populated configuration lets init and bind scripts
           registered before run apply to it. */
    }
    return self;
}

/* Loads the navigation requested before the web view was on screen. */
- (void)apply_pending {
    if (self.pending_request) {
        [self.web_view loadRequest:self.pending_request];
        self.pending_request = nil;
    } else if (self.pending_html) {
        [self.web_view loadHTMLString:self.pending_html baseURL:nil];
        self.pending_html = nil;
    }
}

- (void)eval:(NSString *)js {
    [self.web_view evaluateJavaScript:js completionHandler:nil];
}

/* Injects `js` to run at the start of every page load. */
- (void)inject:(NSString *)js {
    WKUserScript *script =
        [[WKUserScript alloc] initWithSource:js
                               injectionTime:WKUserScriptInjectionTimeAtDocumentStart
                            forMainFrameOnly:NO];
    [self.config.userContentController addUserScript:script];
}

/* Receives an RPC message from a bound JavaScript function, dispatches it to
 * the matching Peko closure, and lets that closure resolve the promise through
 * webview_return. */
- (void)userContentController:(WKUserContentController *)controller
      didReceiveScriptMessage:(WKScriptMessage *)message {
    NSData *data = [[message.body description] dataUsingEncoding:NSUTF8StringEncoding];
    NSDictionary *msg = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    if (!msg) {
        return;
    }

    NSString *seq    = [NSString stringWithFormat:@"%@", msg[@"id"]];
    NSString *method = msg[@"method"];
    id params        = msg[@"params"];

    PekoBinding *binding = self.bindings[method];
    if (!binding) {
        return;
    }

    NSData *params_data = [NSJSONSerialization dataWithJSONObject:params options:0 error:nil];
    NSString *req = [[NSString alloc] initWithData:params_data encoding:NSUTF8StringEncoding];

    binding.fn([seq UTF8String], [req UTF8String], binding.arg);
}

@end

/* A launch or activation URL is handed to the deep-link layer, which stores the
 * route for the client SDK to fetch once the bridge connects. */
extern void peko_deeplink_ios_deliver(const char *url);

/* Deliver a deep-link URL: store it for the connect-time fetch (covers a cold
 * launch, where the page is not loaded yet), and, when the page is already
 * loaded, push the route straight into it so a URL that arrives after connect
 * still navigates. */
static void peko_ios_deliver_url(NSURL *url) {
    if (!url) {
        return;
    }
    NSString *full = [url absoluteString];
    peko_deeplink_ios_deliver([full UTF8String]);

    NSRange separator = [full rangeOfString:@"://"];
    NSString *route = (separator.location != NSNotFound)
                          ? [full substringFromIndex:separator.location + separator.length]
                          : full;
    if (![route hasPrefix:@"/"]) {
        route = [@"/" stringByAppendingString:route];
    }

    if (g_webview && g_webview.web_view) {
        NSString *escaped =
            [[route stringByReplacingOccurrencesOfString:@"\\" withString:@"\\\\"]
                stringByReplacingOccurrencesOfString:@"\"" withString:@"\\\""];
        NSString *js = [NSString
            stringWithFormat:@"window.__peko_deeplink&&window.__peko_deeplink(\"%@\")", escaped];
        dispatch_async(dispatch_get_main_queue(), ^{
          [g_webview.web_view evaluateJavaScript:js completionHandler:nil];
        });
    }
}

/* The app delegate builds the window around the singleton web view at launch. */
@interface PekoAppDelegate : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@end

@implementation PekoAppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)options {
    // A cold launch through a registered-scheme URL carries it in the launch
    // options; hand it to the deep-link layer as the launch route.
    NSURL *launch_url = options[UIApplicationLaunchOptionsURLKey];
    if (launch_url) {
        peko_ios_deliver_url(launch_url);
    }

    self.window = [[UIWindow alloc] initWithFrame:[[UIScreen mainScreen] bounds]];

    UIViewController *root = [[UIViewController alloc] init];
    if (g_webview) {
        g_webview.web_view = [[WKWebView alloc] initWithFrame:root.view.bounds
                                                configuration:g_webview.config];
        g_webview.web_view.autoresizingMask =
            UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight;
        // Extend the content edge to edge. The scroll view otherwise insets the
        // page by the safe areas, which leaves the web view background showing
        // in the status bar and home indicator strips. A clear web view over a
        // black window keeps those strips filled rather than white.
        g_webview.web_view.scrollView.contentInsetAdjustmentBehavior =
            UIScrollViewContentInsetAdjustmentNever;
        g_webview.web_view.opaque = NO;
        g_webview.web_view.backgroundColor = [UIColor clearColor];
        g_webview.web_view.scrollView.backgroundColor = [UIColor clearColor];
        root.view.backgroundColor = [UIColor blackColor];
        [root.view addSubview:g_webview.web_view];
        root.title = g_webview.title;
        [g_webview apply_pending];
    }

    self.window.rootViewController = root;
    [self.window makeKeyAndVisible];
    return YES;
}

/* A URL opened while the app runs is handed to the deep-link layer too. */
- (BOOL)application:(UIApplication *)application
            openURL:(NSURL *)url
            options:(NSDictionary<UIApplicationOpenURLOptionsKey, id> *)options {
    (void)application;
    (void)options;
    peko_ios_deliver_url(url);
    return YES;
}

@end

/* ------------------------------------------------------------------------- */
/* The C API, matching webview.cc so std::webview links against either.       */
/* ------------------------------------------------------------------------- */

webview_t webview_create(int debug, void *window) {
    (void)debug;
    (void)window;
    g_webview = [[PekoWebView alloc] init];
    return (__bridge void *)g_webview;
}

void webview_destroy(webview_t w) {
    (void)w;
    g_webview = nil;
}

void webview_run(webview_t w) {
    (void)w;
    @autoreleasepool {
        char *argv[] = {NULL};
        UIApplicationMain(0, argv, nil, NSStringFromClass([PekoAppDelegate class]));
    }
}

/* iOS applications do not terminate themselves under the platform guidelines,
 * so this is a no-op. */
void webview_terminate(webview_t w) {
    (void)w;
}

void webview_set_title(webview_t w, const char *title) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    view.title = [NSString stringWithUTF8String:title];
}

/* A window on iOS fills the screen, so an explicit size is not applied. */
void webview_set_size(webview_t w, int width, int height, int hints) {
    (void)w;
    (void)width;
    (void)height;
    (void)hints;
}

void webview_navigate(webview_t w, const char *url) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    NSURL *target = [NSURL URLWithString:[NSString stringWithUTF8String:url]];
    NSURLRequest *request = [NSURLRequest requestWithURL:target];
    if (view.web_view.window) {
        [view.web_view loadRequest:request];
    } else {
        view.pending_request = request;
    }
}

void webview_set_html(webview_t w, const char *html) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    NSString *body = [NSString stringWithUTF8String:html];
    if (view.web_view.window) {
        [view.web_view loadHTMLString:body baseURL:nil];
    } else {
        view.pending_html = body;
    }
}

void webview_init(webview_t w, const char *js) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    [view inject:[NSString stringWithUTF8String:js]];
}

void webview_eval(webview_t w, const char *js) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    [view eval:[NSString stringWithUTF8String:js]];
}

/* Registers a native callback under a global JavaScript name. The injected
 * glue mirrors the desktop RPC protocol: the bound function returns a promise
 * kept in window._rpc and posts the call to the __peko__ message handler. */
void webview_bind(webview_t w, const char *name,
                  webview_bind_fn fn, void *arg) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    NSString *bind_name = [NSString stringWithUTF8String:name];
    if (view.bindings[bind_name]) {
        return;
    }

    PekoBinding *binding = [[PekoBinding alloc] init];
    binding.fn  = fn;
    binding.arg = arg;
    view.bindings[bind_name] = binding;

    NSString *js = [NSString stringWithFormat:
        @"(function() { var name = '%@';"
        @"  var RPC = window._rpc = (window._rpc || {nextSeq: 1});"
        @"  window[name] = function() {"
        @"    var seq = RPC.nextSeq++;"
        @"    var promise = new Promise(function(resolve, reject) {"
        @"      RPC[seq] = { resolve: resolve, reject: reject };"
        @"    });"
        @"    window.webkit.messageHandlers.__peko__.postMessage(JSON.stringify({"
        @"      id: seq, method: name,"
        @"      params: Array.prototype.slice.call(arguments) }));"
        @"    return promise;"
        @"  };"
        @"})()", bind_name];

    [view inject:js];
    [view eval:js];
}

void webview_unbind(webview_t w, const char *name) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    NSString *bind_name = [NSString stringWithUTF8String:name];
    if (view.bindings[bind_name]) {
        [view.bindings removeObjectForKey:bind_name];
        NSString *js = [NSString stringWithFormat:@"delete window['%@'];", bind_name];
        [view inject:js];
        [view eval:js];
    }
}

/* Resolves or rejects the pending promise for a bound call. result is already
 * JSON, so it is inserted into the resolve call unquoted, as on the desktop. */
void webview_return(webview_t w, const char *seq, int status, const char *result) {
    PekoWebView *view = (__bridge PekoWebView *)w;
    NSString *seq_str    = [NSString stringWithUTF8String:seq];
    NSString *result_str = [NSString stringWithUTF8String:result];
    NSString *method     = (status == 0) ? @"resolve" : @"reject";
    NSString *js = [NSString stringWithFormat:
        @"window._rpc[%@].%@(%@); delete window._rpc[%@]",
        seq_str, method, result_str, seq_str];
    [view eval:js];
}

/* Desktop window chrome has no meaning on iOS, where a view fills the screen
   and there is no movable window. These keep the C API uniform. */
void peko_webview_set_transparent(webview_t w, int transparent) {
    (void)w;
    (void)transparent;
}

void peko_webview_set_decorations(webview_t w, int decorated) {
    (void)w;
    (void)decorated;
}

void peko_webview_begin_drag(webview_t w) {
    (void)w;
}

/* A view fills the screen on iOS and there is no movable or resizable window,
 * so the window controls are no-ops. */
void peko_webview_minimize(webview_t w) {
    (void)w;
}

void peko_webview_maximize(webview_t w) {
    (void)w;
}

void peko_webview_close(webview_t w) {
    (void)w;
}

/* A view fills the screen on iOS with no native window controls. */
void peko_webview_set_window_buttons_hidden(webview_t w, int hidden) {
    (void)w;
    (void)hidden;
}

int peko_webview_has_native_window_controls(webview_t w) {
    (void)w;
    return 0;
}

#endif /* __APPLE__ && TARGET_OS_IPHONE */
