/*
 * peko_deeplink_windows.c
 *
 * Deep-link delivery on Windows. A launch through a registered scheme puts the
 * URL on the command line, read at startup. register writes the scheme into
 * the per-user registry so `start scheme://path` launches the app. Since that
 * spawns a new process, single_instance forwards the URL over a named pipe to
 * the already-running instance and exits, so its window is reused instead of
 * opening a second one.
 */

#if defined(_WIN32)

#include <windows.h>
#include <stdio.h>
#include <string.h>

/* The deep-link callback is a Peko closure kept reachable across collections by
 * a handle. The forwarding-pipe listener runs the closure on its own attached
 * thread. */
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
static HWND                   g_deeplink_window  = NULL;

void peko_deeplink_set_window(void *window)
{
    g_deeplink_window = (HWND)window;
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

/* Copy the command-line token that holds a scheme URL into out. Returns 1 when
 * one is found. Tokens are bounded by spaces and quotes. */
static int peko_deeplink_launch_url(char *out, size_t out_len)
{
    const char *command = GetCommandLineA();
    if (!command)
        return 0;
    const char *marker = strstr(command, "://");
    if (!marker)
        return 0;

    const char *start = marker;
    while (start > command && start[-1] != ' ' && start[-1] != '"')
        start--;
    const char *end = marker;
    while (*end && *end != ' ' && *end != '"')
        end++;

    size_t length = (size_t)(end - start);
    if (length >= out_len)
        length = out_len - 1;
    memcpy(out, start, length);
    out[length] = '\0';
    return 1;
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

static char g_pipe_name[256];

/* Accept forwarded URLs on the owner pipe and deliver each through the
 * callback. One client is served at a time, which is enough for occasional
 * deep-link launches. */
static DWORD WINAPI peko_deeplink_listener(LPVOID arg)
{
    (void)arg;
    pgc_thread_attach();
    for (;;) {
        HANDLE pipe = CreateNamedPipeA(
            g_pipe_name, PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES, 0, 4096, 0, NULL);
        if (pipe == INVALID_HANDLE_VALUE)
            return 0;

        pgc_begin_blocking();
        BOOL connected = ConnectNamedPipe(pipe, NULL) ||
                         GetLastError() == ERROR_PIPE_CONNECTED;
        char  buffer[2048];
        DWORD read_count = 0;
        if (connected)
            ReadFile(pipe, buffer, sizeof(buffer) - 1, &read_count, NULL);
        pgc_end_blocking();

        if (connected && read_count > 0) {
            buffer[read_count] = '\0';
            char route[2048];
            peko_deeplink_route(buffer, route, sizeof(route));
            /* Raise the window: a minimized window is restored and brought to
             * the front (the forwarding instance granted foreground rights). */
            if (g_deeplink_window) {
                ShowWindow(g_deeplink_window, SW_RESTORE);
                SetForegroundWindow(g_deeplink_window);
            }
            peko_deeplink_invoke(route);
        }
        DisconnectNamedPipe(pipe);
        CloseHandle(pipe);
    }
}

void peko_deeplink_single_instance(const char *scheme)
{
    if (!scheme || !scheme[0])
        return;

    char mutex_name[256];
    snprintf(mutex_name, sizeof(mutex_name), "peko-deeplink-%s", scheme);
    snprintf(g_pipe_name, sizeof(g_pipe_name), "\\\\.\\pipe\\peko-deeplink-%s", scheme);

    HANDLE mutex = CreateMutexA(NULL, TRUE, mutex_name);
    if (mutex && GetLastError() == ERROR_ALREADY_EXISTS) {
        /* Another instance owns the mutex: forward this launch URL and exit so
         * the running window handles it. Fall through to run as an ordinary
         * extra instance if there is no URL or the send fails. */
        char url[2048];
        if (peko_deeplink_launch_url(url, sizeof(url))) {
            HANDLE pipe = CreateFileA(g_pipe_name, GENERIC_WRITE, 0, NULL,
                                      OPEN_EXISTING, 0, NULL);
            if (pipe != INVALID_HANDLE_VALUE) {
                /* Let the running instance take the foreground when it raises
                 * its window in response to this forward. */
                AllowSetForegroundWindow(ASFW_ANY);
                DWORD written = 0;
                BOOL  ok = WriteFile(pipe, url, (DWORD)strlen(url), &written, NULL);
                CloseHandle(pipe);
                if (ok) {
                    ExitProcess(0);
                }
            }
        }
        return;
    }

    /* First instance: accept forwarded URLs on the pipe. The mutex is held for
     * the process lifetime. */
    HANDLE thread = CreateThread(NULL, 0, peko_deeplink_listener, NULL, 0, NULL);
    if (thread)
        CloseHandle(thread);
}

/* Register the scheme in the per-user registry so `start scheme://path` (and a
 * clicked link) launches this app with the URL as its argument. Rewritten on
 * every run, which keeps it current if the app moves. */
void peko_deeplink_register(const char *scheme, const char *name)
{
    (void)name;
    if (!scheme || !scheme[0])
        return;

    char exe[MAX_PATH];
    if (GetModuleFileNameA(NULL, exe, MAX_PATH) == 0)
        return;

    char base[512];
    snprintf(base, sizeof(base), "Software\\Classes\\%s", scheme);
    HKEY key;
    if (RegCreateKeyExA(HKEY_CURRENT_USER, base, 0, NULL, 0, KEY_WRITE, NULL,
                        &key, NULL) != ERROR_SUCCESS)
        return;
    char description[256];
    snprintf(description, sizeof(description), "URL:%s", scheme);
    RegSetValueExA(key, NULL, 0, REG_SZ, (const BYTE *)description,
                   (DWORD)(strlen(description) + 1));
    RegSetValueExA(key, "URL Protocol", 0, REG_SZ, (const BYTE *)"", 1);
    RegCloseKey(key);

    char command_key[512];
    snprintf(command_key, sizeof(command_key),
             "Software\\Classes\\%s\\shell\\open\\command", scheme);
    if (RegCreateKeyExA(HKEY_CURRENT_USER, command_key, 0, NULL, 0, KEY_WRITE,
                        NULL, &key, NULL) != ERROR_SUCCESS)
        return;
    char command[MAX_PATH + 16];
    snprintf(command, sizeof(command), "\"%s\" \"%%1\"", exe);
    RegSetValueExA(key, NULL, 0, REG_SZ, (const BYTE *)command,
                   (DWORD)(strlen(command) + 1));
    RegCloseKey(key);
}

#endif /* _WIN32 */
