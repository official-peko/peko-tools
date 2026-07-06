/*
 * peko_deeplink_linux.c
 *
 * Deep-link delivery on Linux. A launch through a registered scheme runs the
 * app with the URL as an argument (Exec=exec %u), read from the process
 * arguments at startup. single_instance makes the first run the owner of an
 * abstract socket; a later launch forwards its URL over that socket to the
 * running instance (which delivers it through the deep-link callback) and
 * exits, so the existing window is reused instead of opening a second one.
 */

#if defined(__linux__) && !defined(__ANDROID__)

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

/* The deep-link callback is a Peko closure kept reachable across collections by
 * a handle. The forwarding-socket listener runs the closure on its own
 * attached thread. */
typedef unsigned int pgc_handle;
extern pgc_handle pgc_handle_create(void *object);
extern void      *pgc_handle_get(pgc_handle handle);
extern void       pgc_begin_blocking(void);
extern void       pgc_end_blocking(void);
extern void       pgc_thread_attach(void);

typedef void (*peko_deeplink_callback)(void *context, const char *path);

static peko_deeplink_callback g_deeplink_cb     = NULL;
static pgc_handle             g_deeplink_ctx     = 0;
static int                    g_deeplink_ctx_set = 0;
static void                  *g_deeplink_window  = NULL;

/* GTK entry points, declared here so this file needs no GTK headers; the
 * symbols link from the GTK libraries the webview pulls in. gtk_window_present
 * must run on the UI thread, so it is scheduled through g_idle_add. */
extern unsigned int g_idle_add(int (*function)(void *), void *data);
extern void         gtk_window_present(void *window);

void peko_deeplink_set_window(void *window)
{
    g_deeplink_window = window;
}

/* Raise the window on the UI thread. Returns 0 (G_SOURCE_REMOVE) to run once. */
static int peko_deeplink_present(void *data)
{
    gtk_window_present(data);
    return 0;
}

/* Reduce a scheme URL to the route path that follows the scheme, ensuring a
 * leading slash. A URL with no path becomes "/". */
static void peko_deeplink_route(const char *url, char *out, size_t out_len)
{
    const char *marker = strstr(url, "://");
    const char *route  = marker ? marker + 3 : url;
    if (route[0] == '/')
        snprintf(out, out_len, "%s", route);
    else
        snprintf(out, out_len, "/%s", route);
}

/* Copy the first argument that looks like a scheme URL into out. Returns 1 when
 * one is found. /proc/self/cmdline is the NUL-separated argument vector. */
static int peko_deeplink_launch_url(char *out, size_t out_len)
{
    FILE *file = fopen("/proc/self/cmdline", "rb");
    if (!file)
        return 0;
    char   buffer[4096];
    size_t count = fread(buffer, 1, sizeof(buffer) - 1, file);
    fclose(file);
    if (count == 0)
        return 0;
    buffer[count] = '\0';

    size_t offset = 0;
    while (offset < count) {
        const char *argument = buffer + offset;
        if (strstr(argument, "://")) {
            snprintf(out, out_len, "%s", argument);
            return 1;
        }
        offset += strlen(argument) + 1;
    }
    return 0;
}

static char g_initial_route[2048];
static int  g_initial_taken = 0;

const char *peko_deeplink_take_initial(void)
{
    if (g_initial_taken)
        return "";
    g_initial_taken    = 1;
    g_initial_route[0] = '\0';

    char url[2048];
    if (peko_deeplink_launch_url(url, sizeof(url)))
        peko_deeplink_route(url, g_initial_route, sizeof(g_initial_route));
    return g_initial_route;
}

void peko_deeplink_set_handler(void *callback, void *context)
{
    g_deeplink_cb = (peko_deeplink_callback)callback;
    if (context) {
        g_deeplink_ctx     = pgc_handle_create(context);
        g_deeplink_ctx_set = 1;
    }
}

/* Run the Peko callback with a route, resolving the context through its handle.
 * The caller must be an attached, unparked thread. */
static void peko_deeplink_invoke(const char *route)
{
    if (!g_deeplink_cb)
        return;
    void *context = g_deeplink_ctx_set ? pgc_handle_get(g_deeplink_ctx) : NULL;
    g_deeplink_cb(context, route);
}

/* Build the single-instance socket path for a scheme in the user runtime
 * directory (a real file, which unlike an abstract socket is not confined to a
 * network namespace, so it works under an AppImage). */
static void peko_deeplink_sock_path(char *out, size_t out_len, const char *scheme)
{
    const char *dir = getenv("XDG_RUNTIME_DIR");
    if (!dir || !dir[0])
        dir = "/tmp";
    snprintf(out, out_len, "%s/peko-deeplink-%s.sock", dir, scheme);
}

/* Accept forwarded URLs on the owner socket and deliver each through the
 * callback. */
static void *peko_deeplink_listener(void *arg)
{
    int listen_fd = (int)(long)arg;
    pgc_thread_attach();
    for (;;) {
        pgc_begin_blocking();
        int client = accept(listen_fd, NULL, NULL);
        char    buffer[2048];
        ssize_t n = (client >= 0) ? read(client, buffer, sizeof(buffer) - 1) : -1;
        if (client >= 0)
            close(client);
        pgc_end_blocking();

        if (n > 0) {
            buffer[n] = '\0';
            char route[2048];
            peko_deeplink_route(buffer, route, sizeof(route));
            fprintf(stderr, "[peko-deeplink] received forwarded url, route %s\n", route);
            if (g_deeplink_window)
                g_idle_add(peko_deeplink_present, g_deeplink_window);
            peko_deeplink_invoke(route);
        }
    }
    return NULL;
}

void peko_deeplink_single_instance(const char *scheme)
{
    if (!scheme || !scheme[0])
        return;

    char path[256];
    peko_deeplink_sock_path(path, sizeof(path), scheme);
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", path);

    /* Try to reach a running instance and forward this launch URL to it. */
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0)
        return;
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0) {
        char url[2048];
        if (peko_deeplink_launch_url(url, sizeof(url))) {
            size_t url_len = strlen(url);
            if (write(fd, url, url_len) == (ssize_t)url_len) {
                fprintf(stderr, "[peko-deeplink] forwarded %s to running instance\n", url);
                close(fd);
                _exit(0);
            }
        }
        fprintf(stderr, "[peko-deeplink] running instance found but no url to forward\n");
        close(fd);
        return;
    }
    close(fd);

    /* No running instance answered: remove any stale socket file and own it. */
    unlink(path);
    fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0)
        return;
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0 && listen(fd, 4) == 0) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, peko_deeplink_listener,
                           (void *)(long)fd) == 0) {
            pthread_detach(thread);
            fprintf(stderr, "[peko-deeplink] owning instance, listening at %s\n", path);
            return;
        }
    }
    fprintf(stderr, "[peko-deeplink] could not own socket %s (errno %d)\n", path, errno);
    close(fd);
}

/* Install a desktop entry that claims the scheme, so xdg-open and clicked
 * links route to the app. It runs once (skipped when the entry already
 * exists), only when running as an AppImage (APPIMAGE names the runnable file
 * and APPDIR the temporary mount that holds the bundled icon). Everything is
 * best-effort; a missing tool or write failure just leaves the app usable
 * through a direct launch. */
void peko_deeplink_register(const char *scheme, const char *name)
{
    if (!scheme || !scheme[0])
        return;
    const char *title    = (name && name[0]) ? name : scheme;
    const char *appimage = getenv("APPIMAGE");
    const char *home     = getenv("HOME");
    if (!appimage || !home)
        return;

    char applications[1024];
    snprintf(applications, sizeof(applications), "%s/.local/share/applications", home);
    char desktop_path[1200];
    snprintf(desktop_path, sizeof(desktop_path), "%s/peko-%s.desktop", applications, scheme);
    if (access(desktop_path, F_OK) == 0)
        return;

    char path[1024];
    snprintf(path, sizeof(path), "%s/.local", home);
    mkdir(path, 0755);
    snprintf(path, sizeof(path), "%s/.local/share", home);
    mkdir(path, 0755);
    mkdir(applications, 0755);
    char icons[1024];
    snprintf(icons, sizeof(icons), "%s/.local/share/icons", home);
    mkdir(icons, 0755);

    /* Copy the bundled icon to a stable path; APPDIR is a temporary mount that
     * is gone after this run, so its icon cannot be referenced directly. */
    char icon_dst[1200];
    icon_dst[0] = '\0';
    const char *appdir = getenv("APPDIR");
    if (appdir) {
        char icon_src[1024];
        snprintf(icon_src, sizeof(icon_src), "%s/icon.png", appdir);
        if (access(icon_src, R_OK) == 0) {
            snprintf(icon_dst, sizeof(icon_dst), "%s/peko-%s.png", icons, scheme);
            char copy[2600];
            snprintf(copy, sizeof(copy), "cp -f '%s' '%s' 2>/dev/null", icon_src, icon_dst);
            if (system(copy) != 0)
                icon_dst[0] = '\0';
        }
    }

    FILE *file = fopen(desktop_path, "wb");
    if (file) {
        fprintf(file,
                "[Desktop Entry]\n"
                "Type=Application\n"
                "Name=%s\n"
                "Exec=\"%s\" %%u\n"
                "Icon=%s\n"
                "Terminal=false\n"
                "StartupWMClass=Exec\n"
                "MimeType=x-scheme-handler/%s;\n",
                title, appimage,
                icon_dst[0] ? icon_dst : "application-x-executable", scheme);
        fclose(file);
    }

    /* Refresh the desktop database and make this entry the scheme's handler. */
    char activate[2600];
    snprintf(activate, sizeof(activate),
             "update-desktop-database '%s' 2>/dev/null; "
             "xdg-mime default peko-%s.desktop x-scheme-handler/%s 2>/dev/null",
             applications, scheme, scheme);
    (void)!system(activate);
}

#endif /* __linux__ && !__ANDROID__ */
