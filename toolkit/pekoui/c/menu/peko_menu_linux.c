/*
 * peko_menu_linux.c
 *
 * The native menu bar on Linux, built as a GtkMenuBar and packed above the
 * WebKitGTK view. Linux has no global application menu, so the user's first
 * submenu is a normal top-level menu and the app-menu hooks are no-ops.
 *
 * The webview is the direct child of the top-level GtkWindow. To add a menu
 * bar the child is re-parented into a vertical GtkBox holding the menu bar and
 * the view. Menu item clicks fire GTK's "activate" signal on the UI thread
 * inside the parked webview run loop, mirroring the webview bind trampoline:
 * unpark, run the Peko closure, repark.
 *
 * Guarded to desktop Linux; Android defines __linux__ too but has no GTK, so it
 * falls to peko_menu_stub.c.
 */

#if defined(__linux__) && !defined(__ANDROID__)

#include <ctype.h>
#include <gtk/gtk.h>
#include <stdio.h>
#include <string.h>

/* The click callback is a Peko closure: its raw function pointer and its
 * managed context, kept reachable across collections by a handle. */
typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_handle_release(pgc_handle handle);
extern void       pgc_begin_blocking(void);
extern void       pgc_end_blocking(void);

/* The webview package exports the native window handle (the GtkWindow). */
extern void *webview_get_window(void *webview);

/* WebKitGTK editing command entry point, declared here so this file needs no
 * WebKit headers. The webview widget is a WebKitWebView. */
extern void webkit_web_view_execute_editing_command(void *web_view, const char *command);

typedef void (*peko_menu_callback)(void *context, const char *action_id);

static GtkWidget     *g_menubar        = NULL;
static GtkWidget     *g_current_menu   = NULL; /* the open submenu's GtkMenu */
static GtkWidget     *g_menu_window    = NULL; /* the top-level GtkWindow */
static GtkWidget     *g_webview_widget = NULL; /* the WebKitWebView, for roles */
static GtkAccelGroup *g_accel_group    = NULL; /* holds the menu key accelerators */

/* Parse an accelerator like "CmdOrCtrl+Shift+S" into a key value and modifier
 * mask. On Linux CmdOrCtrl maps to Control. A single-character key is taken
 * literally; a longer token is looked up as a GDK key name. */
static void peko_menu_parse_accel(const char *accel, guint *keyval,
                                  GdkModifierType *mods)
{
    *keyval = 0;
    *mods = 0;
    if (!accel || !accel[0])
        return;

    char spec[128];
    snprintf(spec, sizeof(spec), "%s", accel);
    char *save = NULL;
    for (char *token = strtok_r(spec, "+", &save); token;
         token = strtok_r(NULL, "+", &save)) {
        char lower[64];
        size_t i = 0;
        for (; token[i] && i < sizeof(lower) - 1; i++) {
            lower[i] = (char)tolower((unsigned char)token[i]);
        }
        lower[i] = '\0';

        if (!strcmp(lower, "cmdorctrl") || !strcmp(lower, "cmd") ||
            !strcmp(lower, "command") || !strcmp(lower, "ctrl") ||
            !strcmp(lower, "control") || !strcmp(lower, "super")) {
            *mods |= GDK_CONTROL_MASK;
        } else if (!strcmp(lower, "shift")) {
            *mods |= GDK_SHIFT_MASK;
        } else if (!strcmp(lower, "alt") || !strcmp(lower, "option")) {
            *mods |= GDK_MOD1_MASK;
        } else if (lower[0]) {
            *keyval = (lower[1] == '\0') ? gdk_unicode_to_keyval((guint)lower[0])
                                         : gdk_keyval_from_name(lower);
        }
    }
}
static peko_menu_callback g_menu_cb    = NULL;
static pgc_handle     g_menu_ctx       = 0;
static int            g_menu_ctx_set   = 0;

/* A plain item click: forward its action id to the Peko callback. Fires on the
 * UI thread inside the parked run loop, so unpark to run managed code and
 * repark before returning. */
static void peko_menu_item_activated(GtkMenuItem *item, gpointer user_data)
{
    (void)user_data;
    const char *action_id = (const char *)g_object_get_data(G_OBJECT(item), "peko_action");
    if (!g_menu_cb || !action_id)
        return;

    pgc_end_blocking();
    void *context = g_menu_ctx_set ? pgc_handle_get(g_menu_ctx) : NULL;
    g_menu_cb(context, action_id);
    pgc_begin_blocking();
}

/* A standard role item. Window roles act on the GtkWindow; editing roles route
 * to the WebKit view's editing commands. Roles with no Linux equivalent (About,
 * Hide) are a no-op. Role codes match pekoui::menu::role_code. */
static void peko_menu_role_activated(GtkMenuItem *item, gpointer data)
{
    (void)item;
    int role = GPOINTER_TO_INT(data);
    switch (role) {
        case 1:  /* Quit */
        case 10: /* Close */
            gtk_main_quit();
            break;
        case 9: /* Minimize */
            if (g_menu_window)
                gtk_window_iconify(GTK_WINDOW(g_menu_window));
            break;
        case 11: /* Fullscreen */
            if (g_menu_window)
                gtk_window_fullscreen(GTK_WINDOW(g_menu_window));
            break;
        case 3: /* Copy */
            if (g_webview_widget)
                webkit_web_view_execute_editing_command(g_webview_widget, "Copy");
            break;
        case 4: /* Cut */
            if (g_webview_widget)
                webkit_web_view_execute_editing_command(g_webview_widget, "Cut");
            break;
        case 5: /* Paste */
            if (g_webview_widget)
                webkit_web_view_execute_editing_command(g_webview_widget, "Paste");
            break;
        case 6: /* SelectAll */
            if (g_webview_widget)
                webkit_web_view_execute_editing_command(g_webview_widget, "SelectAll");
            break;
        case 7: /* Undo */
            if (g_webview_widget)
                webkit_web_view_execute_editing_command(g_webview_widget, "Undo");
            break;
        case 8: /* Redo */
            if (g_webview_widget)
                webkit_web_view_execute_editing_command(g_webview_widget, "Redo");
            break;
        default: /* About, Hide: no Linux equivalent for now. */
            break;
    }
}

void peko_menu_begin(const char *app_name)
{
    (void)app_name; /* Linux has no application menu. */
    g_menubar = gtk_menu_bar_new();
    g_current_menu = NULL;
    if (g_accel_group)
        g_object_unref(g_accel_group);
    g_accel_group = gtk_accel_group_new();
    g_object_ref_sink(g_menubar); /* hold it until apply packs it */
}

/* Linux has no application menu; app-menu extras have nowhere to go. Drop the
 * current menu so following item calls before a real submenu are ignored. */
void peko_menu_app_open(void)
{
    g_current_menu = NULL;
}

void peko_menu_submenu(const char *label)
{
    if (!g_menubar)
        return;
    GtkWidget *item = gtk_menu_item_new_with_label(label ? label : "");
    GtkWidget *menu = gtk_menu_new();
    gtk_menu_item_set_submenu(GTK_MENU_ITEM(item), menu);
    gtk_menu_shell_append(GTK_MENU_SHELL(g_menubar), item);
    g_current_menu = menu;
}

void peko_menu_item(const char *label, const char *action_id, const char *accel)
{
    if (!g_current_menu)
        return;
    GtkWidget *item = gtk_menu_item_new_with_label(label ? label : "");
    g_object_set_data_full(G_OBJECT(item), "peko_action",
                           g_strdup(action_id ? action_id : ""), g_free);
    g_signal_connect(item, "activate", G_CALLBACK(peko_menu_item_activated), NULL);
    if (accel && accel[0] && g_accel_group) {
        guint           keyval = 0;
        GdkModifierType mods   = 0;
        peko_menu_parse_accel(accel, &keyval, &mods);
        if (keyval != 0) {
            gtk_widget_add_accelerator(item, "activate", g_accel_group, keyval,
                                       mods, GTK_ACCEL_VISIBLE);
        }
    }
    gtk_menu_shell_append(GTK_MENU_SHELL(g_current_menu), item);
}

void peko_menu_separator(void)
{
    if (g_current_menu)
        gtk_menu_shell_append(GTK_MENU_SHELL(g_current_menu),
                              gtk_separator_menu_item_new());
}

void peko_menu_role(const char *label, int role)
{
    if (!g_current_menu)
        return;
    GtkWidget *item = gtk_menu_item_new_with_label(label ? label : "");
    g_signal_connect(item, "activate", G_CALLBACK(peko_menu_role_activated),
                     GINT_TO_POINTER(role));
    gtk_menu_shell_append(GTK_MENU_SHELL(g_current_menu), item);
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

    GtkWidget *win = (GtkWidget *)webview_get_window(window);
    if (!win || !g_menubar)
        return;
    g_menu_window = win;
    if (g_accel_group)
        gtk_window_add_accel_group(GTK_WINDOW(win), g_accel_group);

    /* The webview is the window's current child. Re-parent it under a vertical
     * box holding the menu bar on top and the view filling the rest. */
    GtkWidget *view = gtk_bin_get_child(GTK_BIN(win));
    if (view) {
        g_webview_widget = view;
        g_object_ref(view);
        gtk_container_remove(GTK_CONTAINER(win), view);
        GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
        gtk_box_pack_start(GTK_BOX(box), g_menubar, FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(box), view, TRUE, TRUE, 0);
        g_object_unref(view);
        gtk_container_add(GTK_CONTAINER(win), box);
    }
    g_object_unref(g_menubar); /* the box holds it now */
    gtk_widget_show_all(win);
}

#endif /* __linux__ && !__ANDROID__ */
