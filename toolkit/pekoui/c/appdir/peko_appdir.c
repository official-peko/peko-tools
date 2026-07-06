/*
 * peko_platform.c
 *
 * A writable per-app data directory for the pekoui storage and keychain
 * capabilities. The path is <base>/.peko/<app_id>, where base is the user home
 * directory (HOME on Unix, APPDATA or USERPROFILE on Windows). The directory
 * tree is created on request. The result lives in a static buffer that the
 * caller copies into managed memory right away, so this touches no GC state.
 */

#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>

#ifdef _WIN32
#include <direct.h>
#define PEKO_MKDIR(path) _mkdir(path)
#else
#include <sys/types.h>
#define PEKO_MKDIR(path) mkdir((path), 0700)
#endif

static char g_data_dir[2048];

const char *peko_app_data_dir(const char *app_id)
{
    const char *base = getenv("HOME");
#ifdef _WIN32
    if (base == NULL || base[0] == '\0')
        base = getenv("APPDATA");
    if (base == NULL || base[0] == '\0')
        base = getenv("USERPROFILE");
#endif
    if (base == NULL || base[0] == '\0')
        base = ".";

    const char *id = (app_id != NULL && app_id[0] != '\0') ? app_id : "app";

    /* Create the parent .peko directory, then the per-app directory under it. */
    snprintf(g_data_dir, sizeof(g_data_dir), "%s/.peko", base);
    PEKO_MKDIR(g_data_dir);
    snprintf(g_data_dir, sizeof(g_data_dir), "%s/.peko/%s", base, id);
    PEKO_MKDIR(g_data_dir);

    return g_data_dir;
}
