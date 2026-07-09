/*
 * peko_env.c
 *
 * Reads an environment variable. The dev loop (peko run) passes the dev server
 * URL, the route to restore, and a state-file path to the launched app this
 * way. The result points into the process environment and is copied into
 * managed memory by the caller right away, so this touches no GC state. A
 * missing variable reads as the empty string.
 */

#include <stdlib.h>

#if defined(_WIN32)
#include <windows.h>
#elif defined(__APPLE__)
#include <mach-o/dyld.h>
#include <stdint.h>
#else
#include <unistd.h>
#endif

const char *peko_env(const char *name)
{
    if (name == NULL)
        return "";
    const char *value = getenv(name);
    return (value != NULL) ? value : "";
}

/* Sets an environment variable in the current process, so a spawned child
   inherits it. An app hands a pop-up child its bridge and route this way. */
void peko_env_set(const char *name, const char *value)
{
    if (name == NULL)
        return;
    if (value == NULL)
        value = "";
#if defined(_WIN32)
    _putenv_s(name, value);
#else
    setenv(name, value, 1);
#endif
}

/* The path of the running executable, so an app can spawn another instance of
   itself for a pop-up window. Empty when it cannot be resolved. The result is a
   static buffer copied into managed memory by the caller right away. */
const char *peko_env_self_exe(void)
{
    static char buffer[4096];
    buffer[0] = '\0';
#if defined(_WIN32)
    DWORD len = GetModuleFileNameA(NULL, buffer, (DWORD)sizeof(buffer));
    if (len == 0 || len >= sizeof(buffer))
        buffer[0] = '\0';
#elif defined(__APPLE__)
    uint32_t size = (uint32_t)sizeof(buffer);
    if (_NSGetExecutablePath(buffer, &size) != 0)
        buffer[0] = '\0';
#else
    ssize_t len = readlink("/proc/self/exe", buffer, sizeof(buffer) - 1);
    if (len < 0)
        buffer[0] = '\0';
    else
        buffer[len] = '\0';
#endif
    return buffer;
}

/* The host operating system the app runs on, as a stable identifier. */
const char *peko_env_os(void)
{
#if defined(_WIN32)
    return "windows";
#elif defined(__APPLE__)
    return "macos";
#elif defined(__ANDROID__)
    return "android";
#elif defined(__linux__)
    return "linux";
#else
    return "unknown";
#endif
}
