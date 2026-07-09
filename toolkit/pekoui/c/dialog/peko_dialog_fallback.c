/*
 * peko_dialog_fallback.c
 *
 * The folder chooser on the platforms with no native chooser: iOS and Android,
 * which have no folder browser. It returns the empty string and the caller
 * falls back to a typed path. macOS uses peko_dialog_apple.m, Windows uses
 * peko_dialog_windows.c, and desktop Linux uses peko_dialog_linux.c.
 */

#if defined(__APPLE__)
#include <TargetConditionals.h>
#endif

#if defined(__ANDROID__) || (defined(__APPLE__) && !TARGET_OS_OSX)

const char *peko_dialog_pick_folder(const char *title)
{
    (void)title;
    return "";
}

#endif
