/*
 * peko_dialog_apple.m
 *
 * macOS folder chooser for pekoui::dialog, backed by NSOpenPanel. AppKit is
 * main-thread only, so the panel runs on the main queue; the caller (a bridge
 * handler thread) parks for the GC while it waits. Compiled only for macOS; iOS
 * and the other platforms use the fallback.
 */

#if defined(__APPLE__)
#include <TargetConditionals.h>
#if TARGET_OS_OSX

#import <Cocoa/Cocoa.h>
#include <string.h>

/* The runtime parks and unparks the calling thread across the blocking wait. */
extern void pgc_begin_blocking(void);
extern void pgc_end_blocking(void);

static char g_dialog_path[4096];

const char *peko_dialog_pick_folder(const char *title)
{
    g_dialog_path[0] = '\0';

    void (^work)(void) = ^{
        NSOpenPanel *panel = [NSOpenPanel openPanel];
        [panel setCanChooseFiles:NO];
        [panel setCanChooseDirectories:YES];
        [panel setAllowsMultipleSelection:NO];
        [panel setCanCreateDirectories:YES];
        if (title != NULL && title[0] != '\0') {
            [panel setMessage:[NSString stringWithUTF8String:title]];
        }
        if ([panel runModal] == NSModalResponseOK) {
            NSURL *url = [[panel URLs] firstObject];
            const char *path = url ? [[url path] UTF8String] : NULL;
            if (path != NULL) {
                strncpy(g_dialog_path, path, sizeof(g_dialog_path) - 1);
                g_dialog_path[sizeof(g_dialog_path) - 1] = '\0';
            }
        }
    };

    if ([NSThread isMainThread]) {
        work();
    } else {
        /* Park for the GC: the main thread runs the panel while this thread
           waits, and a collection can fire meanwhile. */
        pgc_begin_blocking();
        dispatch_sync(dispatch_get_main_queue(), work);
        pgc_end_blocking();
    }

    return g_dialog_path;
}

#endif /* TARGET_OS_OSX */
#endif /* __APPLE__ */
