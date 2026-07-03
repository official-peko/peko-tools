/* Android NativeActivity entry point for a Peko app.
 *
 * A Peko Android app is a NativeActivity with no application DEX of its own.
 * The platform loads this shared library and calls ANativeActivity_onCreate,
 * which the app glue included below provides. The glue starts a dedicated
 * thread and calls android_main on it. android_main records the activity for
 * the runtime and the webview backend, then runs the compiled program entry.
 *
 * The glue source is compiled only through this file so it never enters the
 * shared std build on the desktop targets, which have no Android headers. The
 * whole file compiles to nothing off Android. */

#if defined(__ANDROID__)

#include "android_native_app_glue.h"
#include "android_native_app_glue.c"

/* The running activity. Read by the Android webview backend to reach the Java
 * VM and the activity object. */
struct android_app *gapp = NULL;

/* The runtime entry emitted from runtime.peko. It boots the collector, attaches
 * the thread, runs the global initializers, and calls the program on_start
 * hook. The desktop targets reach it as the process main; here the glue thread
 * calls it directly. */
extern int main(int argc, char **argv);

/* Runs on the glue thread after the activity is created. */
void android_main(struct android_app *app) {
    gapp = app;
    main(0, NULL);
}

#endif /* __ANDROID__ */
