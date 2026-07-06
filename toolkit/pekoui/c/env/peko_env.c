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

const char *peko_env(const char *name)
{
    if (name == NULL)
        return "";
    const char *value = getenv(name);
    return (value != NULL) ? value : "";
}
