/* Android webview backend for std::webview (Route 2).
 *
 * A Peko app is a NativeActivity: a real android.app.Activity driven from
 * native code with no application DEX of its own. This backend shows a real
 * android.webkit.WebView, not a pixel blit. The native side here drives the
 * prebuilt Java helper (android/classes.dex, class dev.peko.webview.*) through
 * static calls, and the helper calls back through the JNI-exported native
 * methods below. The WebView lives on the Java UI thread; every WebView
 * operation is dispatched there by the helper.
 *
 * The file compiles to nothing off Android so it can sit alongside webview.cc
 * and webview_ios.m in the one std native build. */

#if defined(__ANDROID__)

#include <jni.h>
#include <android/log.h>
#include <android/looper.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* The application glue owns the running activity. */
#include "../runtime/android_native_app_glue.h"
extern struct android_app *gapp;

/* The GC runtime. The Java UI thread runs the bind callbacks, so it attaches
 * to the collector once and stays parked between callbacks. */
extern void pgc_thread_attach(void);
extern void pgc_begin_blocking(void);
extern void pgc_end_blocking(void);

typedef void *webview_t;
typedef void (*webview_bind_fn)(const char *seq, const char *req, void *arg);

#define PEKO_WV_LOG(...) __android_log_print(ANDROID_LOG_INFO, "peko_webview", __VA_ARGS__)
#define BRIDGE_CLASS "dev/peko/webview/PekoWebViewBridge"

/* One JavaScript-to-native binding. */
typedef struct peko_wv_binding {
    char                    *name;
    webview_bind_fn          fn;
    void                    *arg;
    struct peko_wv_binding  *next;
} peko_wv_binding;

/* One init script, re-evaluated at the start of every page load. */
typedef struct peko_wv_script {
    char                  *js;
    struct peko_wv_script *next;
} peko_wv_script;

static JavaVM         *g_vm       = NULL;
static jclass          g_bridge   = NULL;   /* global ref to PekoWebViewBridge */
static peko_wv_binding *g_bindings = NULL;
static peko_wv_script  *g_scripts  = NULL;

/* The JNI callbacks from the bridge, defined at the end of this file. They are
 * bound to the Java native methods by RegisterNatives, because NativeActivity
 * loads this library outside System.loadLibrary and the Java VM would otherwise
 * not find them by name. */
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnReady(JNIEnv *env, jclass cls);
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnMessage(JNIEnv *env, jclass cls,
                                                        jstring message);
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnPageStarted(JNIEnv *env, jclass cls,
                                                            jstring url);
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnPageFinished(JNIEnv *env, jclass cls,
                                                             jstring url);

/* Binds the bridge's native methods to the callbacks above. */
static void register_bridge_natives(JNIEnv *env, jclass bridge) {
    static const JNINativeMethod methods[] = {
        {"nativeOnReady", "()V",
         (void *)Java_dev_peko_webview_PekoWebViewBridge_nativeOnReady},
        {"nativeOnMessage", "(Ljava/lang/String;)V",
         (void *)Java_dev_peko_webview_PekoWebViewBridge_nativeOnMessage},
        {"nativeOnPageStarted", "(Ljava/lang/String;)V",
         (void *)Java_dev_peko_webview_PekoWebViewBridge_nativeOnPageStarted},
        {"nativeOnPageFinished", "(Ljava/lang/String;)V",
         (void *)Java_dev_peko_webview_PekoWebViewBridge_nativeOnPageFinished},
    };
    if ((*env)->RegisterNatives(env, bridge, methods, 4) != 0) {
        PEKO_WV_LOG("create: RegisterNatives failed");
        if ((*env)->ExceptionCheck(env)) {
            (*env)->ExceptionClear(env);
        }
    }
}

/* Resolves a JNIEnv for the calling thread, attaching it to the Java VM when
 * needed. attached is set when this call performed the attach. */
static JNIEnv *get_env(int *attached) {
    JNIEnv *env = NULL;
    *attached = 0;
    if (!g_vm) {
        return NULL;
    }
    if ((*g_vm)->GetEnv(g_vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        if ((*g_vm)->AttachCurrentThread(g_vm, &env, NULL) != 0) {
            return NULL;
        }
        *attached = 1;
    }
    return env;
}

static void put_env(int attached) {
    if (attached && g_vm) {
        (*g_vm)->DetachCurrentThread(g_vm);
    }
}

/* Loads an application class through the activity's class loader. FindClass on
 * an attached native thread uses the system loader, which cannot see the
 * prebuilt helper, so the activity's own loader resolves it. Returns a global
 * ref the caller keeps. */
static jclass load_app_class(JNIEnv *env, const char *name) {
    jobject activity = gapp->activity->clazz;
    jclass activity_cls = (*env)->GetObjectClass(env, activity);
    jmethodID get_loader = (*env)->GetMethodID(
        env, activity_cls, "getClassLoader", "()Ljava/lang/ClassLoader;");
    jobject loader = (*env)->CallObjectMethod(env, activity, get_loader);

    jclass loader_cls = (*env)->GetObjectClass(env, loader);
    jmethodID load_class = (*env)->GetMethodID(
        env, loader_cls, "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;");

    jstring jname = (*env)->NewStringUTF(env, name);
    jclass found = (jclass)(*env)->CallObjectMethod(env, loader, load_class, jname);

    jclass result = found ? (jclass)(*env)->NewGlobalRef(env, found) : NULL;

    (*env)->DeleteLocalRef(env, jname);
    (*env)->DeleteLocalRef(env, loader_cls);
    (*env)->DeleteLocalRef(env, loader);
    (*env)->DeleteLocalRef(env, activity_cls);
    if (found) {
        (*env)->DeleteLocalRef(env, found);
    }
    return result;
}

/* Calls a static void method of the bridge that takes a single Java string. */
static void call_bridge_string(const char *method, const char *arg) {
    if (!g_bridge || !arg) {
        return;
    }
    int attached = 0;
    JNIEnv *env = get_env(&attached);
    if (!env) {
        return;
    }
    jmethodID m = (*env)->GetStaticMethodID(env, g_bridge, method,
                                            "(Ljava/lang/String;)V");
    if (m) {
        jstring js = (*env)->NewStringUTF(env, arg);
        (*env)->CallStaticVoidMethod(env, g_bridge, m, js);
        (*env)->DeleteLocalRef(env, js);
    }
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
    put_env(attached);
}

/* Records an init script so it is re-evaluated at each page start. */
static void remember_script(const char *js) {
    peko_wv_script *entry = (peko_wv_script *)malloc(sizeof(peko_wv_script));
    entry->js = strdup(js);
    entry->next = g_scripts;
    g_scripts = entry;
}

/* ------------------------------------------------------------------------- */
/* The C API, matching webview.cc so std::webview links against either.       */
/* ------------------------------------------------------------------------- */

webview_t webview_create(int debug, void *window) {
    (void)debug;
    (void)window;

    g_vm = gapp->activity->vm;

    int attached = 0;
    JNIEnv *env = get_env(&attached);
    if (!env) {
        PEKO_WV_LOG("create: no JNIEnv");
        return NULL;
    }

    if (!g_bridge) {
        g_bridge = load_app_class(env, BRIDGE_CLASS);
        if (!g_bridge) {
            PEKO_WV_LOG("create: bridge class not found");
            put_env(attached);
            return NULL;
        }
        register_bridge_natives(env, g_bridge);
    }

    jmethodID create = (*env)->GetStaticMethodID(
        env, g_bridge, "create", "(Landroid/app/Activity;)V");
    if (create) {
        (*env)->CallStaticVoidMethod(env, g_bridge, create, gapp->activity->clazz);
    }
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }
    put_env(attached);

    /* A stable non-null token. The state lives in the file statics and the Java
     * helper, so the exact value is not read back. */
    return (webview_t)&g_bridge;
}

void webview_destroy(webview_t w) {
    (void)w;
}

/* Runs the app glue event loop while the Java UI thread drives the WebView.
 * The glue delivers the activity lifecycle commands on this thread's looper.
 * Draining them is required: the main thread blocks in its onStart and
 * onResume callbacks until this loop acknowledges each command, and the WebView
 * runnables the bridge posts to the main thread do not run until it does. The
 * caller has already parked this thread for the collector, and the poll and the
 * command processing touch no managed memory. */
void webview_run(webview_t w) {
    (void)w;
    for (;;) {
        int events;
        struct android_poll_source *source = NULL;
        int timeout = (gapp && gapp->destroyRequested) ? 0 : -1;
        int ident = ALooper_pollAll(timeout, NULL, &events, (void **)&source);
        if (source != NULL) {
            source->process(gapp, source);
        }
        if (ident == ALOOPER_POLL_ERROR || (gapp && gapp->destroyRequested)) {
            return;
        }
    }
}

void webview_terminate(webview_t w) {
    (void)w;
}

/* The title is the activity's, set through the app, so this is a no-op. */
void webview_set_title(webview_t w, const char *title) {
    (void)w;
    (void)title;
}

/* A WebView fills the activity content view, so an explicit size is not
 * applied. */
void webview_set_size(webview_t w, int width, int height, int hints) {
    (void)w;
    (void)width;
    (void)height;
    (void)hints;
}

void webview_navigate(webview_t w, const char *url) {
    (void)w;
    call_bridge_string("navigate", url);
}

void webview_set_html(webview_t w, const char *html) {
    (void)w;
    call_bridge_string("loadHtml", html);
}

void webview_init(webview_t w, const char *js) {
    (void)w;
    remember_script(js);
    call_bridge_string("eval", js);
}

void webview_eval(webview_t w, const char *js) {
    (void)w;
    call_bridge_string("eval", js);
}

/* Registers a native callback under a global JavaScript name. The injected
 * glue matches the desktop and iOS RPC protocol: the bound function returns a
 * promise held in window._rpc and posts the call to the native bridge. The
 * request is posted as three newline-separated fields (seq, name, params JSON)
 * so the native side splits it without a JSON parser. */
void webview_bind(webview_t w, const char *name, webview_bind_fn fn, void *arg) {
    (void)w;

    for (peko_wv_binding *b = g_bindings; b; b = b->next) {
        if (strcmp(b->name, name) == 0) {
            return;
        }
    }

    peko_wv_binding *binding = (peko_wv_binding *)malloc(sizeof(peko_wv_binding));
    binding->name = strdup(name);
    binding->fn   = fn;
    binding->arg  = arg;
    binding->next = g_bindings;
    g_bindings = binding;

    const char *fmt =
        "(function() { var name = '%s';"
        "  var RPC = window._rpc = (window._rpc || {nextSeq: 1});"
        "  window[name] = function() {"
        "    var seq = RPC.nextSeq++;"
        "    var promise = new Promise(function(resolve, reject) {"
        "      RPC[seq] = { resolve: resolve, reject: reject };"
        "    });"
        "    window.__peko_native__.postMessage("
        "      seq + '\\n' + name + '\\n' +"
        "      JSON.stringify(Array.prototype.slice.call(arguments)));"
        "    return promise;"
        "  };"
        "})()";
    size_t len = strlen(fmt) + strlen(name) * 2 + 1;
    char *js = (char *)malloc(len);
    snprintf(js, len, fmt, name, name);
    remember_script(js);
    call_bridge_string("eval", js);
    free(js);
}

void webview_unbind(webview_t w, const char *name) {
    (void)w;
    peko_wv_binding **link = &g_bindings;
    while (*link) {
        if (strcmp((*link)->name, name) == 0) {
            peko_wv_binding *dead = *link;
            *link = dead->next;
            free(dead->name);
            free(dead);
            return;
        }
        link = &(*link)->next;
    }
}

/* Resolves or rejects the pending promise. result is already JSON, so it is
 * inserted unquoted, as on the other platforms. */
void webview_return(webview_t w, const char *seq, int status, const char *result) {
    (void)w;
    const char *method = (status == 0) ? "resolve" : "reject";
    const char *fmt = "window._rpc[%s].%s(%s); delete window._rpc[%s]";
    size_t len = strlen(fmt) + strlen(seq) * 2 + strlen(method) + strlen(result) + 1;
    char *js = (char *)malloc(len);
    snprintf(js, len, fmt, seq, method, result, seq);
    call_bridge_string("eval", js);
    free(js);
}

/* Desktop window chrome has no meaning on Android, where a WebView fills the
   activity and there is no movable window. These keep the C API uniform. */
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

/* A WebView fills the activity and there is no movable or resizable window, so
   the window controls are no-ops. */
void peko_webview_minimize(webview_t w) {
    (void)w;
}

void peko_webview_maximize(webview_t w) {
    (void)w;
}

void peko_webview_close(webview_t w) {
    (void)w;
}

/* ------------------------------------------------------------------------- */
/* JNI callbacks from the prebuilt Java helper.                               */
/* ------------------------------------------------------------------------- */

/* The WebView is on screen. This runs on the Java UI thread, so it attaches
 * that thread to the collector and parks it. Each later callback un-parks for
 * the duration of the managed work, matching the desktop and iOS backends. */
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnReady(JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    pgc_thread_attach();
    pgc_begin_blocking();
}

/* A bound JavaScript function posted a request. The body is three
 * newline-separated fields: seq, method, and the params JSON. */
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnMessage(JNIEnv *env, jclass cls,
                                                        jstring message) {
    (void)cls;
    if (!message) {
        return;
    }
    const char *body = (*env)->GetStringUTFChars(env, message, NULL);
    if (!body) {
        return;
    }

    char *copy = strdup(body);
    (*env)->ReleaseStringUTFChars(env, message, body);

    char *first  = strchr(copy, '\n');
    char *second = first ? strchr(first + 1, '\n') : NULL;
    if (first && second) {
        *first  = '\0';
        *second = '\0';
        const char *seq    = copy;
        const char *method = first + 1;
        const char *params = second + 1;

        for (peko_wv_binding *b = g_bindings; b; b = b->next) {
            if (strcmp(b->name, method) == 0) {
                b->fn(seq, params, b->arg);
                break;
            }
        }
    }
    free(copy);
}

/* Re-injects the init scripts at the start of each page load. This is pure JNI
 * with no managed work, so the parked UI thread does not un-park. */
JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnPageStarted(JNIEnv *env, jclass cls,
                                                            jstring url) {
    (void)env;
    (void)cls;
    (void)url;
    for (peko_wv_script *s = g_scripts; s; s = s->next) {
        call_bridge_string("eval", s->js);
    }
}

JNIEXPORT void JNICALL
Java_dev_peko_webview_PekoWebViewBridge_nativeOnPageFinished(JNIEnv *env, jclass cls,
                                                             jstring url) {
    (void)env;
    (void)cls;
    (void)url;
}

#endif /* __ANDROID__ */
