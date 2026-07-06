/*
 * peko_menu_windows.c
 *
 * The native menu bar on Windows, built as an HMENU and attached to the
 * webview's top-level window with SetMenu. Windows has no global application
 * menu, so the user's first submenu is a normal top-level menu and the
 * app-menu hooks are no-ops.
 *
 * Menu clicks arrive as WM_COMMAND. The webview owns the window's WndProc, so
 * this file installs a window subclass (comctl32) to intercept WM_COMMAND
 * without editing the webview. A plain item forwards its action id to the Peko
 * closure on the UI thread inside the parked run loop (unpark, run, repark,
 * mirroring the webview bind trampoline); standard roles act on the window.
 *
 * A native menu bar sits in the window's non-client area, so it applies to a
 * decorated window. A frameless window (set_decorations(false)) removes the
 * non-client area, so its menu is drawn in HTML instead. Keyboard accelerators
 * need a message-loop accelerator table. This file builds one and exports
 * peko_menu_translate_accel, which the webview run loop calls per message so a
 * shortcut (for example Ctrl+S) dispatches its menu WM_COMMAND.
 */

#if defined(_WIN32)

#include <ctype.h>
#include <windows.h>
#include <commctrl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_handle_release(pgc_handle handle);
extern void       pgc_begin_blocking(void);
extern void       pgc_end_blocking(void);

/* The webview package exports the native window handle (the HWND). */
extern void *webview_get_window(void *webview);

typedef void (*peko_menu_callback)(void *context, const char *action_id);

/* Each menu command id maps to either a plain item's action id or a role code
 * (role != 0). */
#define PEKO_MENU_MAX_CMDS 4096
typedef struct {
    char *action_id;
    int   role;
} PekoMenuCmd;

static HMENU              g_menubar      = NULL;
static HMENU              g_current_menu = NULL; /* the open popup */
static HWND               g_menu_hwnd    = NULL;
static peko_menu_callback g_menu_cb      = NULL;
static pgc_handle         g_menu_ctx     = 0;
static int                g_menu_ctx_set = 0;
static PekoMenuCmd        g_cmds[PEKO_MENU_MAX_CMDS];
static UINT               g_next_cmd     = 1; /* command id 0 is reserved */
static ACCEL              g_accels[PEKO_MENU_MAX_CMDS]; /* item key accelerators */
static int                g_accel_count  = 0;
static HACCEL             g_accel_table  = NULL;

/* Parse an accelerator like "CmdOrCtrl+Shift+S" into an ACCEL virtual-key flag
 * mask and key code. Returns 1 when an alphanumeric key was found. On Windows
 * CmdOrCtrl maps to Control. */
static int peko_menu_parse_accel(const char *accel, BYTE *fVirt, WORD *key)
{
    *fVirt = FVIRTKEY;
    *key = 0;
    if (!accel || !accel[0])
        return 0;

    char spec[128];
    snprintf(spec, sizeof(spec), "%s", accel);
    char *ctx = NULL;
    for (char *token = strtok_s(spec, "+", &ctx); token;
         token = strtok_s(NULL, "+", &ctx)) {
        char lower[64];
        size_t i = 0;
        for (; token[i] && i < sizeof(lower) - 1; i++) {
            lower[i] = (char)tolower((unsigned char)token[i]);
        }
        lower[i] = '\0';

        if (!strcmp(lower, "cmdorctrl") || !strcmp(lower, "cmd") ||
            !strcmp(lower, "command") || !strcmp(lower, "ctrl") ||
            !strcmp(lower, "control") || !strcmp(lower, "super")) {
            *fVirt |= FCONTROL;
        } else if (!strcmp(lower, "shift")) {
            *fVirt |= FSHIFT;
        } else if (!strcmp(lower, "alt") || !strcmp(lower, "option")) {
            *fVirt |= FALT;
        } else if (lower[0] && lower[1] == '\0') {
            *key = (WORD)toupper((unsigned char)lower[0]);
        }
    }
    return *key != 0;
}

/* Called from the webview message loop so a menu accelerator dispatches its
 * WM_COMMAND. Returns nonzero when the message was consumed. */
int peko_menu_translate_accel(void *msg)
{
    if (!g_accel_table || !g_menu_hwnd)
        return 0;
    return TranslateAcceleratorW(g_menu_hwnd, g_accel_table, (MSG *)msg);
}

/* Dispatch a menu accelerator by virtual key and the current modifier state.
 * WebView2 runs the web content in a separate process, so key presses in the
 * page never reach the host message loop and TranslateAccelerator never sees
 * them. The controller's AcceleratorKeyPressed event fires on the host instead,
 * and calls this with the pressed virtual key. Returns nonzero when a matching
 * accelerator ran, so the caller marks the event handled. */
int peko_menu_dispatch_accel_key(unsigned int vkey)
{
    if (!g_menu_hwnd || g_accel_count == 0)
        return 0;
    BYTE mods = 0;
    if (GetKeyState(VK_CONTROL) < 0)
        mods |= FCONTROL;
    if (GetKeyState(VK_SHIFT) < 0)
        mods |= FSHIFT;
    if (GetKeyState(VK_MENU) < 0)
        mods |= FALT;
    /* A bare key with no modifier would fire on plain typing, so it is ignored
     * here. */
    if (mods == 0)
        return 0;
    for (int i = 0; i < g_accel_count; i++) {
        BYTE accel_mods = g_accels[i].fVirt & (FCONTROL | FSHIFT | FALT);
        if (g_accels[i].key == (WORD)vkey && accel_mods == mods) {
            SendMessageW(g_menu_hwnd, WM_COMMAND, MAKEWPARAM(g_accels[i].cmd, 0), 0);
            return 1;
        }
    }
    return 0;
}

/* Convert a UTF-8 string to a freshly allocated wide string. */
static WCHAR *peko_menu_widen(const char *s)
{
    if (!s)
        s = "";
    int len = MultiByteToWideChar(CP_UTF8, 0, s, -1, NULL, 0);
    if (len <= 0)
        len = 1;
    WCHAR *w = (WCHAR *)malloc((size_t)len * sizeof(WCHAR));
    if (!w)
        return NULL;
    MultiByteToWideChar(CP_UTF8, 0, s, -1, w, len);
    return w;
}

/* Drop every recorded command and its stored action id. */
static void peko_menu_reset_cmds(void)
{
    for (UINT i = 0; i < g_next_cmd && i < PEKO_MENU_MAX_CMDS; i++) {
        free(g_cmds[i].action_id);
        g_cmds[i].action_id = NULL;
        g_cmds[i].role = 0;
    }
    g_next_cmd = 1;
}

/* The window subclass: intercept menu WM_COMMAND, forward everything else. */
static LRESULT CALLBACK peko_menu_subproc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp,
                                          UINT_PTR id_subclass, DWORD_PTR ref)
{
    (void)id_subclass;
    (void)ref;
    if (msg == WM_COMMAND && HIWORD(wp) == 0 && lp == 0) {
        UINT cmd = LOWORD(wp);
        if (cmd > 0 && cmd < PEKO_MENU_MAX_CMDS) {
            PekoMenuCmd *mc = &g_cmds[cmd];
            if (mc->role != 0) {
                switch (mc->role) {
                    case 1:  /* Quit */
                    case 10: /* Close */
                        PostMessageW(hwnd, WM_CLOSE, 0, 0);
                        break;
                    case 9: /* Minimize */
                        ShowWindow(hwnd, SW_MINIMIZE);
                        break;
                    default: /* other roles have no Windows menu-bar action yet */
                        break;
                }
                return 0;
            }
            if (mc->action_id && g_menu_cb) {
                pgc_end_blocking();
                void *context = g_menu_ctx_set ? pgc_handle_get(g_menu_ctx) : NULL;
                g_menu_cb(context, mc->action_id);
                pgc_begin_blocking();
                return 0;
            }
        }
    }
    return DefSubclassProc(hwnd, msg, wp, lp);
}

void peko_menu_begin(const char *app_name)
{
    (void)app_name; /* Windows has no application menu. */
    if (g_menubar)
        DestroyMenu(g_menubar); /* frees the whole tree, popups included */
    peko_menu_reset_cmds();
    if (g_accel_table) {
        DestroyAcceleratorTable(g_accel_table);
        g_accel_table = NULL;
    }
    g_accel_count = 0;
    g_menubar = CreateMenu();
    g_current_menu = NULL;
}

/* Windows has no application menu; app-menu extras have nowhere to go. */
void peko_menu_app_open(void)
{
    g_current_menu = NULL;
}

void peko_menu_submenu(const char *label)
{
    if (!g_menubar)
        return;
    HMENU popup = CreatePopupMenu();
    WCHAR *wlabel = peko_menu_widen(label);
    AppendMenuW(g_menubar, MF_POPUP, (UINT_PTR)popup, wlabel ? wlabel : L"");
    free(wlabel);
    g_current_menu = popup;
}

void peko_menu_item(const char *label, const char *action_id, const char *accel)
{
    if (!g_current_menu || g_next_cmd >= PEKO_MENU_MAX_CMDS)
        return;
    UINT id = g_next_cmd++;
    g_cmds[id].action_id = _strdup(action_id ? action_id : "");
    g_cmds[id].role = 0;
    WCHAR *wlabel = peko_menu_widen(label);
    AppendMenuW(g_current_menu, MF_STRING, id, wlabel ? wlabel : L"");
    free(wlabel);

    if (accel && accel[0] && g_accel_count < PEKO_MENU_MAX_CMDS) {
        BYTE fVirt;
        WORD key;
        if (peko_menu_parse_accel(accel, &fVirt, &key)) {
            g_accels[g_accel_count].fVirt = fVirt;
            g_accels[g_accel_count].key = key;
            g_accels[g_accel_count].cmd = (WORD)id;
            g_accel_count++;
        }
    }
}

void peko_menu_separator(void)
{
    if (g_current_menu)
        AppendMenuW(g_current_menu, MF_SEPARATOR, 0, NULL);
}

void peko_menu_role(const char *label, int role)
{
    if (!g_current_menu || g_next_cmd >= PEKO_MENU_MAX_CMDS)
        return;
    UINT id = g_next_cmd++;
    g_cmds[id].action_id = NULL;
    g_cmds[id].role = role;
    WCHAR *wlabel = peko_menu_widen(label);
    AppendMenuW(g_current_menu, MF_STRING, id, wlabel ? wlabel : L"");
    free(wlabel);
}

void peko_menu_apply(void *callback, void *context, void *window)
{
    g_menu_cb = (peko_menu_callback)callback;
    if (g_menu_ctx_set) {
        pgc_handle_release(g_menu_ctx);
        g_menu_ctx_set = 0;
    }
    if (context) {
        g_menu_ctx = pgc_handle_create(context);
        g_menu_ctx_set = 1;
    }

    HWND hwnd = (HWND)webview_get_window(window);
    if (!hwnd || !g_menubar)
        return;
    g_menu_hwnd = hwnd;

    if (g_accel_count > 0) {
        g_accel_table = CreateAcceleratorTableW(g_accels, g_accel_count);
    }

    SetMenu(hwnd, g_menubar);
    SetWindowSubclass(hwnd, peko_menu_subproc, 1, 0);
    /* Force a non-client recompute so the webview refits under the menu bar
     * (its WM_SIZE handler reads GetClientRect, which now excludes the menu). */
    SetWindowPos(hwnd, NULL, 0, 0, 0, 0,
                 SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
    DrawMenuBar(hwnd);
}

#endif /* _WIN32 */
