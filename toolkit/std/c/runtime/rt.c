/*
 * rt.c
 * Core runtime helpers for Pekoscript.
 * Pure C99. No file I/O (that lives in peko_fs.c).
 * No threading (that lives in peko_threads.c).
 *
 * Covers:
 *   - macOS/iOS bundle identifier injection
 *   - Windows console hiding
 *   - Cross-platform sleep
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>
#include <time.h>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <winsock2.h>
#  include <windows.h>
#else
#  include <unistd.h>
#endif


/* =========================================================================
 * Cross-platform sleep
 * Called with pgc_begin_blocking/pgc_end_blocking bracketing from Peko
 * so the GC can run collections while this thread is parked.
 * ====================================================================== */

void peko_sleep_ms(int ms)
{
#ifdef _WIN32
    Sleep((DWORD)ms);
#else
    struct timespec ts;
    ts.tv_sec  = ms / 1000;
    ts.tv_nsec = (ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
#endif
}

/* =========================================================================
 * peko_printf
 * Windows UCRT does not export printf as a linkable symbol in newer versions.
 * This wrapper has a unique name so there is no redefinition conflict with
 * the inline printf in the CRT headers. vprintf IS always exported.
 * ====================================================================== */

#include <stdarg.h>

int peko_printf(const char *fmt, ...)
{
    va_list args;
    int result;
    va_start(args, fmt);
    result = vprintf(fmt, args);
    va_end(args);
    return result;
}

/* =========================================================================
 * Windows socket lifecycle
 * Called from standard/main.peko on Windows before and after OnStart().
 * ====================================================================== */

#ifdef _WIN32
void windowsStart(void)
{
    WSADATA wsa;
    WSAStartup(MAKEWORD(2, 2), &wsa);
}

void windowsCleanup(void)
{
    WSACleanup();
}
#else
void windowsStart(void)   {}
void windowsCleanup(void) {}
#endif

/* =========================================================================
 * Windows GUI helpers
 * ====================================================================== */

#ifdef _WIN32
void windows_hide_console(void)
{
    HWND console_window = GetConsoleWindow();
    ShowWindow(console_window, 0);
}
#endif

/* Android application files-directory initialization lives in the UI package. */
