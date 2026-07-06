/*
 * peko_menu_stub.c
 *
 * The native menu bar on platforms without an implementation. Android and any
 * other platform compile these no-ops so a project links and runs there while
 * set_menu simply has no effect. macOS lives in peko_menu_apple.m, Windows in
 * peko_menu_windows.c, and Linux in peko_menu_linux.c.
 */

#if !defined(__APPLE__) && !defined(_WIN32) && !(defined(__linux__) && !defined(__ANDROID__))

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

#endif /* stub platforms */
