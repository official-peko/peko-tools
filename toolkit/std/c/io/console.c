/*
 * console.c
 * Console fd I/O implementation for std::io.
 * Pure C99, runs on Windows, macOS, Linux, iOS, and Android.
 */

#include "console.h"

/* =========================================================================
 * Internal helpers
 * ====================================================================== */

/*
 * Converts a Peko flag bitmask to the platform open flags.
 */
static int flags_to_native(int flags)
{
    int native = 0;

    int rd  = (flags & PEKO_FD_FLAG_READ)     != 0;
    int wr  = (flags & PEKO_FD_FLAG_WRITE)    != 0;
    int ap  = (flags & PEKO_FD_FLAG_APPEND)   != 0;
    int cr  = (flags & PEKO_FD_FLAG_CREATE)   != 0;
    int tr  = (flags & PEKO_FD_FLAG_TRUNCATE) != 0;

#ifdef _WIN32
    if (rd && wr) native |= _O_RDWR;
    else if (wr)  native |= _O_WRONLY;
    else          native |= _O_RDONLY;

    if (ap)  native |= _O_APPEND;
    if (cr)  native |= _O_CREAT;
    if (tr)  native |= _O_TRUNC;

    /* Always open in binary mode to avoid CRLF translation. */
    native |= _O_BINARY;
#else
    if (rd && wr) native |= O_RDWR;
    else if (wr)  native |= O_WRONLY;
    else          native |= O_RDONLY;

    if (ap) native |= O_APPEND;
    if (cr) native |= O_CREAT;
    if (tr) native |= O_TRUNC;
#endif

    return native;
}

/* =========================================================================
 * Console initialization
 * ====================================================================== */

int peko_console_init(void)
{
#ifdef _WIN32
    /*
     * Enable ANSI escape sequence processing on Windows 10 and later.
     * This is set for both stdout and stderr so styling and cursor
     * escape codes work on both streams.
     */
    HANDLE h_out = GetStdHandle(STD_OUTPUT_HANDLE);
    HANDLE h_err = GetStdHandle(STD_ERROR_HANDLE);
    DWORD  mode  = 0;

    if (h_out != INVALID_HANDLE_VALUE && GetConsoleMode(h_out, &mode))
        SetConsoleMode(h_out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);

    if (h_err != INVALID_HANDLE_VALUE && GetConsoleMode(h_err, &mode))
        SetConsoleMode(h_err, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);

    /*
     * Set stdout and stderr to UTF-8 so multi-byte characters print
     * correctly.
     */
    SetConsoleOutputCP(CP_UTF8);
#endif
    /* Does nothing on Unix. ANSI is supported natively. */
    return 0;
}

/* =========================================================================
 * Write operations
 * ====================================================================== */

int peko_write_fd(int fd, const char *buf, int len)
{
    if (!buf || len <= 0)
        return 0;

#ifdef _WIN32
    /*
     * On Windows, write to fd 1 and 2 through the standard handle with
     * WriteFile so the console subsystem processes the output. Other fds
     * use _write.
     */
    if (fd == PEKO_FD_STDOUT || fd == PEKO_FD_STDERR) {
        HANDLE h = (fd == PEKO_FD_STDOUT)
                   ? GetStdHandle(STD_OUTPUT_HANDLE)
                   : GetStdHandle(STD_ERROR_HANDLE);
        DWORD written = 0;
        if (!WriteFile(h, buf, (DWORD)len, &written, NULL))
            return -1;
        return (int)written;
    }
    return _write(fd, buf, len);
#else
    int total   = 0;
    int remaining = len;

    /* Retry on partial writes. */
    while (remaining > 0) {
        int n = (int)write(fd, buf + total, (size_t)remaining);
        if (n < 0)
            return -1;
        if (n == 0)
            break;
        total     += n;
        remaining -= n;
    }
    return total;
#endif
}

int peko_write_fd_string(int fd, const char *str)
{
    if (!str)
        return 0;
    return peko_write_fd(fd, str, (int)strlen(str));
}

/* =========================================================================
 * Flush
 * ====================================================================== */

int peko_flush_fd(int fd)
{
    /*
     * Use fflush for the standard streams. fsync on a tty fd can block
     * forever on macOS and many Linux kernels because a character device
     * cannot be synced. fflush flushes the libc buffer, which is all the
     * console needs.
     * Other fds use _commit (Windows) or fdatasync (Unix), which are safe
     * for regular files.
     */
#ifdef _WIN32
    if (fd == PEKO_FD_STDOUT) { fflush(stdout); return 0; }
    if (fd == PEKO_FD_STDERR) { fflush(stderr); return 0; }
    return _commit(fd) == 0 ? 0 : -1;
#else
    if (fd == PEKO_FD_STDOUT) { fflush(stdout); return 0; }
    if (fd == PEKO_FD_STDERR) { fflush(stderr); return 0; }
    if (fd == PEKO_FD_STDIN)  { return 0; }
#  if defined(__APPLE__)
    /* fdatasync is missing on iOS and unreliable on macOS. fsync is
     * safe here because the tty fds are already handled above. */
    return fsync(fd) == 0 ? 0 : -1;
#  else
    return fdatasync(fd) == 0 ? 0 : -1;
#  endif
#endif
}

/* =========================================================================
 * Read operations
 * ====================================================================== */

int peko_read_char(int fd, char *out)
{
    if (!out)
        return -1;

#ifdef _WIN32
    int n = _read(fd, out, 1);
#else
    int n = (int)read(fd, out, 1);
#endif

    if (n < 0) return -1;
    if (n == 0) return 0;
    return 1;
}

char *peko_read_line(int fd)
{
    size_t  capacity = 128;
    size_t  length   = 0;
    char   *buf      = (char *)malloc(capacity);
    char    ch;

    if (!buf)
        return NULL;

    for (;;) {
        int n = peko_read_char(fd, &ch);

        if (n < 0) {
            free(buf);
            return NULL;
        }

        /* EOF or newline ends the line. */
        if (n == 0 || ch == '\n')
            break;

        /* Strip carriage returns for cross-platform line endings. */
        if (ch == '\r')
            continue;

        /* Grow buffer if needed. */
        if (length + 1 >= capacity) {
            capacity *= 2;
            char *tmp = (char *)realloc(buf, capacity);
            if (!tmp) {
                free(buf);
                return NULL;
            }
            buf = tmp;
        }

        buf[length++] = ch;
    }

    buf[length] = '\0';

    /* Transfer to GC-managed memory. */
    char *gc_buf = (char *)pgc_alloc_atomic((size_t)(length + 1));
    if (!gc_buf) {
        free(buf);
        return NULL;
    }

    memcpy(gc_buf, buf, length + 1);
    free(buf);
    return gc_buf;
}

/* =========================================================================
 * File descriptor open / close
 * ====================================================================== */

int peko_open_fd(const char *path, int flags)
{
    int native = flags_to_native(flags);

#ifdef _WIN32
    int fd = -1;
    /*
     * _sopen_s is the safe MSVC version of open().
     * Use the default sharing mode (read and write) and 0666 permissions.
     */
    _sopen_s(&fd, path, native, _SH_DENYNO, _S_IREAD | _S_IWRITE);
    return fd;
#else
    /*
     * On Unix, O_CREAT needs a mode argument.
     * Use 0666, which umask adjusts at runtime.
     */
    if (native & O_CREAT)
        return open(path, native, 0666);
    return open(path, native);
#endif
}

int peko_close_fd(int fd)
{
#ifdef _WIN32
    return _close(fd) == 0 ? 0 : -1;
#else
    return close(fd) == 0 ? 0 : -1;
#endif
}

int peko_open_null_fd(void)
{
#ifdef _WIN32
    return peko_open_fd("NUL", PEKO_FD_FLAG_WRITE);
#else
    return peko_open_fd("/dev/null", PEKO_FD_FLAG_WRITE);
#endif
}
