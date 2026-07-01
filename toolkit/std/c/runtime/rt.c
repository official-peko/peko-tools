/*
 * rt.c
 * Core runtime helpers for Pekoscript.
 * Pure C99. No file I/O (that lives in peko_fs.c).
 * No threading (that lives in peko_threads.c).
 *
 * Covers:
 *   - macOS/iOS bundle identifier injection
 *   - Windows console hiding
 *   - Cross-platform sleep
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>
#include <time.h>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <winsock2.h>
#  include <windows.h>
#else
#  include <unistd.h>
#endif


/* =========================================================================
 * Cross-platform sleep
 * Called with pgc_begin_blocking/pgc_end_blocking bracketing from Peko
 * so the GC can run collections while this thread is parked.
 * ====================================================================== */

void peko_sleep_ms(int ms)
{
#ifdef _WIN32
    Sleep((DWORD)ms);
#else
    struct timespec ts;
    ts.tv_sec  = ms / 1000;
    ts.tv_nsec = (ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
#endif
}

/* =========================================================================
 * peko_printf
 * Windows UCRT does not export printf as a linkable symbol in newer versions.
 * This wrapper has a unique name so there is no redefinition conflict with
 * the inline printf in the CRT headers. vprintf IS always exported.
 * ====================================================================== */

#include <stdarg.h>

int peko_printf(const char *fmt, ...)
{
    va_list args;
    int result;
    va_start(args, fmt);
    result = vprintf(fmt, args);
    va_end(args);
    return result;
}

/* =========================================================================
 * Windows socket lifecycle
 * Called from standard/main.peko on Windows before and after OnStart().
 * ====================================================================== */

#ifdef _WIN32
void windowsStart(void)
{
    WSADATA wsa;
    WSAStartup(MAKEWORD(2, 2), &wsa);
}

void windowsCleanup(void)
{
    WSACleanup();
}
#else
void windowsStart(void)   {}
void windowsCleanup(void) {}
#endif

/* =========================================================================
 * Windows GUI helpers
 * ====================================================================== */

#ifdef _WIN32
void windows_hide_console(void)
{
    HWND console_window = GetConsoleWindow();
    ShowWindow(console_window, 0);
}
#endif

#ifdef __ANDROID__

#include <jni.h>
#include "android_native_app_glue.h"
#include <android/log.h>

/* The running activity. Defined by the application glue. */
extern struct android_app *gapp;

/* Records the application files directory. Implemented in the storage path
 * object. */
extern void peko_storage_set_files_dir(const char *path);

#define PEKO_INIT_LOG(...) __android_log_print(ANDROID_LOG_INFO, "peko_storage_init", __VA_ARGS__)

/* Reads Context.getFilesDir through JNI and forwards the absolute path to
 * peko_storage_set_files_dir. The thread attaches to the Java VM for the call
 * and detaches when this function performed the attach. Returns 0 on success. */
static int set_files_dir_via_jni(void)
{
    JavaVM *vm = gapp->activity->vm;
    JNIEnv *env = NULL;
    int attached = 0;
    int result = -1;

    if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        if ((*vm)->AttachCurrentThread(vm, &env, NULL) != 0 || env == NULL) {
            PEKO_INIT_LOG("jni: attach failed");
            return -1;
        }
        attached = 1;
    }

    jobject activity = gapp->activity->clazz;
    jclass ctx_cls = (*env)->GetObjectClass(env, activity);
    jmethodID m_files = (*env)->GetMethodID(env, ctx_cls, "getFilesDir",
                                            "()Ljava/io/File;");
    jobject file = m_files ? (*env)->CallObjectMethod(env, activity, m_files) : NULL;

    if (file) {
        jclass file_cls = (*env)->GetObjectClass(env, file);
        jmethodID m_path = (*env)->GetMethodID(env, file_cls, "getAbsolutePath",
                                               "()Ljava/lang/String;");
        jstring jpath = m_path
            ? (jstring)(*env)->CallObjectMethod(env, file, m_path)
            : NULL;
        if (jpath) {
            const char *path = (*env)->GetStringUTFChars(env, jpath, NULL);
            if (path) {
                PEKO_INIT_LOG("jni: files dir=%s", path);
                peko_storage_set_files_dir(path);
                (*env)->ReleaseStringUTFChars(env, jpath, path);
                result = 0;
            }
            (*env)->DeleteLocalRef(env, jpath);
        }
        (*env)->DeleteLocalRef(env, file_cls);
        (*env)->DeleteLocalRef(env, file);
    }

    (*env)->DeleteLocalRef(env, ctx_cls);

    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
    }

    if (attached) {
        (*vm)->DetachCurrentThread(vm);
    }
    return result;
}

void peko_storage_android_init(void)
{
    if (!gapp || !gapp->activity) {
        PEKO_INIT_LOG("init: no activity available");
        return;
    }

    /* The native activity holds the absolute path of Context.getFilesDir in
     * internalDataPath. */
    const char *internal = gapp->activity->internalDataPath;
    if (internal && internal[0] != '\0') {
        PEKO_INIT_LOG("init: internalDataPath=%s", internal);
        peko_storage_set_files_dir(internal);
        return;
    }

    /* An empty field falls back to a JNI read of Context.getFilesDir. */
    PEKO_INIT_LOG("init: internalDataPath empty, using jni");
    if (!gapp->activity->vm || set_files_dir_via_jni() != 0) {
        PEKO_INIT_LOG("init: could not resolve files dir");
    }
}

#endif /* __ANDROID__ */
