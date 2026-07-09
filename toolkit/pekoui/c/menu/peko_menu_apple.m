/*
 * peko_menu_apple.m
 *
 * The native menu bar on Apple platforms. macOS builds an NSMenu and installs
 * it as the application main menu; iOS has no menu bar, so the calls compile to
 * no-ops. Menu clicks route to a Peko closure through a target that fires on the
 * main thread inside the parked webview run loop, mirroring the webview bind
 * trampoline: unpark, run the closure, repark.
 *
 * macOS builds this file without ARC (the macOS toolchain sets no -fobjc-arc),
 * so memory is managed manually. The iOS branch is empty and needs none.
 */

#if defined(__APPLE__)
#include <TargetConditionals.h>

/* The click callback is a Peko closure: its raw function pointer and its
 * managed context, kept reachable across collections by a handle. */
typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_handle_release(pgc_handle handle);
extern void       pgc_begin_blocking(void);
extern void       pgc_end_blocking(void);

typedef void (*peko_menu_callback)(void *context, const char *action_id);

#if TARGET_OS_IPHONE

/* iOS has no menu bar. */
void peko_menu_begin(const char *app_name) { (void)app_name; }
void peko_menu_app_open(void) {}
void peko_menu_submenu(const char *label) { (void)label; }
void peko_menu_item(const char *label, const char *action_id, const char *accel)
{
    (void)label;
    (void)action_id;
    (void)accel;
}
void peko_menu_separator(void) {}
void peko_menu_role(const char *label, int role) { (void)label; (void)role; }
void peko_menu_apply(void *callback, void *context, void *window)
{
    (void)callback;
    (void)context;
    (void)window;
}

#else /* macOS */

#import <Cocoa/Cocoa.h>

static NSMenu            *g_menu_bar        = nil;
static NSMenu            *g_current_submenu = nil; /* non-owning: the bar holds it */
static NSMenu            *g_app_submenu     = nil; /* non-owning: the bar holds it */
static peko_menu_callback g_menu_cb         = NULL;
static pgc_handle         g_menu_ctx        = 0;
static int                g_menu_ctx_set    = 0;
/* Insertion index for entries added to the application menu; -1 appends. User
 * extras are inserted after the About item rather than after the Quit block. */
static NSInteger          g_app_insert_index = -1;

/* The target whose action every plain menu item points at. It reads the item's
 * action id and forwards it to the Peko callback. */
@interface PekoMenuTarget : NSObject
- (void)pekoMenuAction:(id)sender;
@end

@implementation PekoMenuTarget
- (void)pekoMenuAction:(id)sender
{
    NSString *action_id = [(NSMenuItem *)sender representedObject];
    if (!g_menu_cb || !action_id)
        return;

    /* The action fires on the main thread inside the parked webview run loop.
     * Unpark to run the Peko closure, which allocates managed memory, then
     * repark before returning to the loop. Re-resolve the context through its
     * handle in case a collection moved it. */
    pgc_end_blocking();
    void *context = g_menu_ctx_set ? pgc_handle_get(g_menu_ctx) : NULL;
    g_menu_cb(context, [action_id UTF8String]);
    pgc_begin_blocking();
}
@end

static PekoMenuTarget *g_menu_target = nil;

/* Add an item to a submenu, inserting into the application menu at the tracked
 * index (so extras land after About) and appending elsewhere. */
static void peko_menu_add(NSMenu *submenu, NSMenuItem *item)
{
    if (submenu == g_app_submenu && g_app_insert_index >= 0) {
        [submenu insertItem:item atIndex:g_app_insert_index];
        g_app_insert_index += 1;
    } else {
        [submenu addItem:item];
    }
}

/* Parse an accelerator like "CmdOrCtrl+Shift+S" into a key equivalent and a
 * modifier mask. On macOS CmdOrCtrl maps to Command. */
static void peko_menu_parse_accel(const char *accel, NSString **out_key,
                                  NSUInteger *out_mods)
{
    *out_key  = @"";
    *out_mods = 0;
    if (!accel || !accel[0])
        return;

    NSString *spec  = [NSString stringWithUTF8String:accel];
    NSArray  *parts = [spec componentsSeparatedByString:@"+"];
    for (NSString *raw in parts) {
        NSString *part = [[raw stringByTrimmingCharactersInSet:
                            [NSCharacterSet whitespaceCharacterSet]] lowercaseString];
        if ([part isEqualToString:@"cmdorctrl"] || [part isEqualToString:@"cmd"] ||
            [part isEqualToString:@"command"] || [part isEqualToString:@"super"]) {
            *out_mods |= NSEventModifierFlagCommand;
        } else if ([part isEqualToString:@"ctrl"] || [part isEqualToString:@"control"]) {
            *out_mods |= NSEventModifierFlagControl;
        } else if ([part isEqualToString:@"shift"]) {
            *out_mods |= NSEventModifierFlagShift;
        } else if ([part isEqualToString:@"alt"] || [part isEqualToString:@"option"]) {
            *out_mods |= NSEventModifierFlagOption;
        } else if ([part length] > 0) {
            *out_key = part;
        }
    }
}

/* Map a role code to its standard responder-chain selector, default key, and
 * modifier mask. Returns NULL for an unknown role. */
static SEL peko_menu_role_selector(int role, NSString **out_key, NSUInteger *out_mods)
{
    *out_key  = @"";
    *out_mods = NSEventModifierFlagCommand;
    switch (role) {
        case 1:  *out_key = @"q"; return @selector(terminate:);
        case 2:  *out_mods = 0;   return @selector(orderFrontStandardAboutPanel:);
        case 3:  *out_key = @"c"; return @selector(copy:);
        case 4:  *out_key = @"x"; return @selector(cut:);
        case 5:  *out_key = @"v"; return @selector(paste:);
        case 6:  *out_key = @"a"; return @selector(selectAll:);
        case 7:  *out_key = @"z"; return @selector(undo:);
        case 8:  *out_key = @"z"; *out_mods = NSEventModifierFlagCommand | NSEventModifierFlagShift; return @selector(redo:);
        case 9:  *out_key = @"m"; return @selector(performMiniaturize:);
        case 10: *out_key = @"w"; return @selector(performClose:);
        case 11: *out_key = @"f"; *out_mods = NSEventModifierFlagCommand | NSEventModifierFlagControl; return @selector(toggleFullScreen:);
        case 12: *out_key = @"h"; return @selector(hide:);
        default: *out_mods = 0;   return NULL;
    }
}

/* Build the standard application menu and install it as the first item of the
 * bar. AppKit always renders the first main-menu item as the bold application
 * menu titled with the bundle name, regardless of the item's own title.
 * Creating it here means the first user submenu lands at index 1 and stays a
 * normal top-level menu. The standard items travel the responder chain to
 * NSApp, so they need no target. */
static void peko_menu_install_app_menu(const char *app_name)
{
    NSString *name = (app_name && app_name[0])
        ? [NSString stringWithUTF8String:app_name]
        : [[NSProcessInfo processInfo] processName];

    NSMenuItem *app_item = [[NSMenuItem alloc] initWithTitle:@"" action:NULL keyEquivalent:@""];
    NSMenu     *app_menu = [[NSMenu alloc] initWithTitle:@""];

    [app_menu addItemWithTitle:[@"About " stringByAppendingString:name]
                        action:@selector(orderFrontStandardAboutPanel:)
                 keyEquivalent:@""];
    [app_menu addItem:[NSMenuItem separatorItem]];
    [app_menu addItemWithTitle:[@"Hide " stringByAppendingString:name]
                        action:@selector(hide:)
                 keyEquivalent:@"h"];
    NSMenuItem *hide_others = [app_menu addItemWithTitle:@"Hide Others"
                                                  action:@selector(hideOtherApplications:)
                                           keyEquivalent:@"h"];
    [hide_others setKeyEquivalentModifierMask:NSEventModifierFlagCommand | NSEventModifierFlagOption];
    [app_menu addItemWithTitle:@"Show All"
                        action:@selector(unhideAllApplications:)
                 keyEquivalent:@""];
    [app_menu addItem:[NSMenuItem separatorItem]];
    [app_menu addItemWithTitle:[@"Quit " stringByAppendingString:name]
                        action:@selector(terminate:)
                 keyEquivalent:@"q"];

    [app_item setSubmenu:app_menu];   /* item retains the submenu */
    [g_menu_bar addItem:app_item];    /* bar retains the item */
    [app_item release];
    [app_menu release];
    g_app_submenu = app_menu;         /* alive through bar -> item -> submenu */
}

void peko_menu_begin(const char *app_name)
{
    if (g_menu_bar)
        [g_menu_bar release];
    g_menu_bar = [[NSMenu alloc] init];
    g_current_submenu = nil;
    g_app_submenu = nil;
    g_app_insert_index = -1;
    if (!g_menu_target)
        g_menu_target = [[PekoMenuTarget alloc] init];
    peko_menu_install_app_menu(app_name);
}

/* Reopen the application menu so following item, separator, and role calls fill
 * it. Extra entries are inserted after the About item (index 1), above the
 * standard Hide and Quit block. */
void peko_menu_app_open(void)
{
    g_current_submenu = g_app_submenu;
    g_app_insert_index = 1;
}

void peko_menu_submenu(const char *label)
{
    if (!g_menu_bar)
        return;
    NSString   *title = [NSString stringWithUTF8String:label];
    NSMenuItem *item  = [[NSMenuItem alloc] initWithTitle:title action:NULL keyEquivalent:@""];
    NSMenu     *sub   = [[NSMenu alloc] initWithTitle:title];
    [item setSubmenu:sub];      /* item retains sub */
    [g_menu_bar addItem:item];  /* bar retains item */
    [item release];
    [sub release];
    g_current_submenu = sub;    /* alive through the bar -> item -> submenu chain */
    g_app_insert_index = -1;    /* a normal submenu appends */
}

void peko_menu_item(const char *label, const char *action_id, const char *accel)
{
    if (!g_current_submenu)
        return;
    NSString  *key  = @"";
    NSUInteger mods = 0;
    peko_menu_parse_accel(accel, &key, &mods);

    NSMenuItem *item = [[NSMenuItem alloc]
        initWithTitle:[NSString stringWithUTF8String:label]
               action:@selector(pekoMenuAction:)
        keyEquivalent:key];
    [item setKeyEquivalentModifierMask:mods];
    [item setRepresentedObject:[NSString stringWithUTF8String:action_id]];
    [item setTarget:g_menu_target];
    peko_menu_add(g_current_submenu, item);
    [item release];
}

void peko_menu_separator(void)
{
    if (g_current_submenu)
        peko_menu_add(g_current_submenu, [NSMenuItem separatorItem]);
}

void peko_menu_role(const char *label, int role)
{
    if (!g_current_submenu)
        return;
    NSString  *key  = @"";
    NSUInteger mods = 0;
    SEL        sel  = peko_menu_role_selector(role, &key, &mods);

    NSMenuItem *item = [[NSMenuItem alloc]
        initWithTitle:[NSString stringWithUTF8String:label]
               action:sel
        keyEquivalent:key];
    [item setKeyEquivalentModifierMask:mods];
    /* No target: standard actions travel the responder chain. */
    peko_menu_add(g_current_submenu, item);
    [item release];
}

void peko_menu_apply(void *callback, void *context, void *window)
{
    (void)window; /* macOS installs a global menu bar; no window needed. */

    /* Reach a safepoint before the handle operations below take the GC lock.
     * pgc_handle_create/release take g_gc.lock, and a stop-the-world collection
     * running on another thread holds that lock while it waits for this thread
     * to park. Without parking first, this main-thread call blocks on the lock
     * and the collector waits on this thread: a deadlock. Parking and unparking
     * lets any in-progress collection finish first. */
    pgc_begin_blocking();
    pgc_end_blocking();

    g_menu_cb = (peko_menu_callback)callback;
    if (g_menu_ctx_set) {
        pgc_handle_release(g_menu_ctx);
        g_menu_ctx_set = 0;
    }
    if (context) {
        g_menu_ctx     = pgc_handle_create(context);
        g_menu_ctx_set = 1;
    }
    if (g_menu_bar)
        [[NSApplication sharedApplication] setMainMenu:g_menu_bar];
}

#endif /* TARGET_OS_IPHONE */

#endif /* __APPLE__ */
