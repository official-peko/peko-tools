/*
 * peko_deeplink_android.c
 *
 * Deep-link delivery on Android. The manifest's VIEW intent-filter routes a
 * registered-scheme URL to the activity, so the launch route is read from the
 * launch intent's data URI at startup. Live delivery for a URL that arrives
 * while the app runs (onNewIntent) is wired later.
 */

#if defined(__ANDROID__)

#include <jni.h>
#include <stdio.h>
#include <string.h>

/* The application glue owns the running activity. */
#include "../webview/android_native_app_glue.h"
extern struct android_app *gapp;

/* Reduce a scheme URL to the route path that follows the scheme, ensuring a
 * leading slash. A URL with no path becomes "/". */
static void peko_deeplink_route(const char *url, char *out, size_t out_len)
{
    const char *marker = strstr(url, "://");
    const char *route  = marker ? marker + 3 : url;
    if (route[0] == '/')
        snprintf(out, out_len, "%s", route);
    else
        snprintf(out, out_len, "/%s", route);
}

static char g_initial_route[2048];
static int  g_initial_taken = 0;

void peko_deeplink_set_handler(void *callback, void *context)
{
    /* Live delivery on Android is wired later; the launch route flows through
     * take_initial instead. */
    (void)callback;
    (void)context;
}

const char *peko_deeplink_take_initial(void)
{
    if (g_initial_taken)
        return "";
    g_initial_taken    = 1;
    g_initial_route[0] = '\0';

    if (!gapp || !gapp->activity)
        return "";

    JavaVM  *vm       = gapp->activity->vm;
    JNIEnv  *env      = NULL;
    int      attached = 0;
    if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        if ((*vm)->AttachCurrentThread(vm, &env, NULL) != 0)
            return "";
        attached = 1;
    }

    /* activity.getIntent().getData().toString(), guarding every step. */
    jobject   activity     = gapp->activity->clazz;
    jclass    activity_cls = (*env)->GetObjectClass(env, activity);
    jmethodID get_intent   = (*env)->GetMethodID(
        env, activity_cls, "getIntent", "()Landroid/content/Intent;");
    jobject intent = get_intent ? (*env)->CallObjectMethod(env, activity, get_intent) : NULL;
    if (intent) {
        jclass    intent_cls = (*env)->GetObjectClass(env, intent);
        jmethodID get_data   = (*env)->GetMethodID(
            env, intent_cls, "getData", "()Landroid/net/Uri;");
        jobject uri = get_data ? (*env)->CallObjectMethod(env, intent, get_data) : NULL;
        if (uri) {
            jclass    uri_cls   = (*env)->GetObjectClass(env, uri);
            jmethodID to_string = (*env)->GetMethodID(
                env, uri_cls, "toString", "()Ljava/lang/String;");
            jstring juri = to_string ? (*env)->CallObjectMethod(env, uri, to_string) : NULL;
            if (juri) {
                const char *utf = (*env)->GetStringUTFChars(env, juri, NULL);
                if (utf) {
                    peko_deeplink_route(utf, g_initial_route, sizeof(g_initial_route));
                    (*env)->ReleaseStringUTFChars(env, juri, utf);
                }
                (*env)->DeleteLocalRef(env, juri);
            }
            (*env)->DeleteLocalRef(env, uri_cls);
            (*env)->DeleteLocalRef(env, uri);
        }
        (*env)->DeleteLocalRef(env, intent_cls);
        (*env)->DeleteLocalRef(env, intent);
    }
    (*env)->DeleteLocalRef(env, activity_cls);

    if (attached)
        (*vm)->DetachCurrentThread(vm);
    return g_initial_route;
}

/* Android registers the scheme through the manifest VIEW intent-filter, so no
 * runtime registration is needed. */
void peko_deeplink_register(const char *scheme, const char *name)
{
    (void)scheme;
    (void)name;
}

/* Android runs one process (the activity is singleTop), so a launch while the
 * app runs reuses it and arrives through onNewIntent; there is nothing to
 * forward. */
void peko_deeplink_single_instance(const char *scheme)
{
    (void)scheme;
}

/* The Android activity is singleTop and comes to the front on onNewIntent, so
 * there is nothing to raise here. */
void peko_deeplink_set_window(void *window)
{
    (void)window;
}

#endif /* __ANDROID__ */
