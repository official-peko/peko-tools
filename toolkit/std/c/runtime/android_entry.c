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

#include <android/asset_manager.h>
#include <jni.h>
#include <stdlib.h>

#include "android_native_app_glue.h"
#include "android_native_app_glue.c"

/* Copies the launch intent's string extras into the process environment before
 * the program reads it. Android does not pass env to a launched app, so a host
 * tool that starts the activity with `am start --es KEY VALUE` reaches the
 * runtime's env::get (plain getenv) only through this bridge. Reads
 * activity.getIntent().getExtras() and setenv's each string entry. */
static void peko_android_import_intent_env(struct android_app *app) {
    if (app == NULL || app->activity == NULL)
        return;

    JavaVM *vm       = app->activity->vm;
    JNIEnv *env      = NULL;
    int     attached = 0;
    if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        if ((*vm)->AttachCurrentThread(vm, &env, NULL) != 0)
            return;
        attached = 1;
    }

    jobject   activity     = app->activity->clazz;
    jclass    activity_cls = (*env)->GetObjectClass(env, activity);
    jmethodID get_intent   = (*env)->GetMethodID(
        env, activity_cls, "getIntent", "()Landroid/content/Intent;");
    jobject intent = get_intent ? (*env)->CallObjectMethod(env, activity, get_intent) : NULL;
    if (intent != NULL) {
        jclass    intent_cls = (*env)->GetObjectClass(env, intent);
        jmethodID get_extras = (*env)->GetMethodID(
            env, intent_cls, "getExtras", "()Landroid/os/Bundle;");
        jobject bundle = get_extras ? (*env)->CallObjectMethod(env, intent, get_extras) : NULL;
        if (bundle != NULL) {
            jclass    bundle_cls = (*env)->GetObjectClass(env, bundle);
            jmethodID key_set    = (*env)->GetMethodID(
                env, bundle_cls, "keySet", "()Ljava/util/Set;");
            jmethodID get_string = (*env)->GetMethodID(
                env, bundle_cls, "getString", "(Ljava/lang/String;)Ljava/lang/String;");
            jobject set = key_set ? (*env)->CallObjectMethod(env, bundle, key_set) : NULL;
            if (set != NULL && get_string != NULL) {
                jclass    set_cls  = (*env)->GetObjectClass(env, set);
                jmethodID to_array = (*env)->GetMethodID(
                    env, set_cls, "toArray", "()[Ljava/lang/Object;");
                jobjectArray keys =
                    to_array ? (jobjectArray)(*env)->CallObjectMethod(env, set, to_array) : NULL;
                if (keys != NULL) {
                    jsize count = (*env)->GetArrayLength(env, keys);
                    for (jsize i = 0; i < count; i++) {
                        jstring key = (jstring)(*env)->GetObjectArrayElement(env, keys, i);
                        if (key == NULL)
                            continue;
                        jstring value = (jstring)(*env)->CallObjectMethod(env, bundle, get_string, key);
                        if (value != NULL) {
                            const char *key_utf   = (*env)->GetStringUTFChars(env, key, NULL);
                            const char *value_utf = (*env)->GetStringUTFChars(env, value, NULL);
                            if (key_utf != NULL && value_utf != NULL)
                                setenv(key_utf, value_utf, 1);
                            if (key_utf != NULL)
                                (*env)->ReleaseStringUTFChars(env, key, key_utf);
                            if (value_utf != NULL)
                                (*env)->ReleaseStringUTFChars(env, value, value_utf);
                            (*env)->DeleteLocalRef(env, value);
                        }
                        (*env)->DeleteLocalRef(env, key);
                    }
                    (*env)->DeleteLocalRef(env, keys);
                }
                (*env)->DeleteLocalRef(env, set_cls);
                (*env)->DeleteLocalRef(env, set);
            }
            (*env)->DeleteLocalRef(env, bundle_cls);
            (*env)->DeleteLocalRef(env, bundle);
        }
        (*env)->DeleteLocalRef(env, intent_cls);
        (*env)->DeleteLocalRef(env, intent);
    }
    (*env)->DeleteLocalRef(env, activity_cls);

    if (attached)
        (*vm)->DetachCurrentThread(vm);
}

/* The running activity. Read by the Android webview backend to reach the Java
 * VM and the activity object. */
struct android_app *gapp = NULL;

/* The app's AAssetManager, reached through the activity. A package that serves
 * bundled files packed in the APK's assets/ (the pekoui asset server) reads
 * them through it. Returns NULL before the activity is recorded. */
AAssetManager *peko_android_asset_manager(void) {
    return (gapp != NULL && gapp->activity != NULL) ? gapp->activity->assetManager : NULL;
}

/* The runtime entry emitted from runtime.peko. It boots the collector, attaches
 * the thread, runs the global initializers, and calls the program on_start
 * hook. The desktop targets reach it as the process main; here the glue thread
 * calls it directly. */
extern int main(int argc, char **argv);

/* Runs on the glue thread after the activity is created. */
void android_main(struct android_app *app) {
    gapp = app;
    peko_android_import_intent_env(app);
    main(0, NULL);
}

#endif /* __ANDROID__ */
