/*
 * console.h
 * Types, constants, and function declarations for std::io console fd I/O.
 * Covers fd I/O, terminal input, and console setup. Pure C99, runs on
 * Windows, macOS, Linux, iOS, and Android.
 */

#ifndef PEKO_IO_CONSOLE_H
#define PEKO_IO_CONSOLE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <windows.h>
#  include <io.h>
#  include <fcntl.h>
#  include <sys/stat.h>
#else
#  include <unistd.h>
#  include <fcntl.h>
#endif

/* -------------------------------------------------------------------------
 * Peko GC allocator. read_line copies its result into an atomic GC buffer
 * the collector owns.
 * ---------------------------------------------------------------------- */

extern void *pgc_alloc_atomic(size_t size);

/* -------------------------------------------------------------------------
 * Standard fd constants. These match the Fd module values in io.peko.
 * ---------------------------------------------------------------------- */

#define PEKO_FD_STDIN  0
#define PEKO_FD_STDOUT 1
#define PEKO_FD_STDERR 2

/* -------------------------------------------------------------------------
 * Open flag constants. These match the FdFlags module values in io.peko.
 * ---------------------------------------------------------------------- */

#define PEKO_FD_FLAG_READ     0x01
#define PEKO_FD_FLAG_WRITE    0x02
#define PEKO_FD_FLAG_APPEND   0x04
#define PEKO_FD_FLAG_CREATE   0x08
#define PEKO_FD_FLAG_TRUNCATE 0x10

/* -------------------------------------------------------------------------
 * Function declarations
 * ---------------------------------------------------------------------- */

/*
 * Initializes the console subsystem.
 * On Windows, enables ANSI escape sequences for stdout and stderr.
 * On Unix, does nothing since ANSI is supported natively.
 * The Peko runtime calls this at program start.
 */
int peko_console_init(void);

/*
 * Writes len bytes from buf to fd.
 * Returns the number of bytes written, or -1 on error.
 */
int peko_write_fd(int fd, const char *buf, int len);

/*
 * Writes a null-terminated string to fd.
 * Returns the number of bytes written, or -1 on error.
 */
int peko_write_fd_string(int fd, const char *str);

/*
 * Flushes the write buffer for fd.
 * Returns 0 on success, -1 on error.
 */
int peko_flush_fd(int fd);

/*
 * Reads exactly one byte from fd into out. Blocks until a byte is available.
 * Returns 1 on success, 0 on EOF, -1 on error.
 */
int peko_read_char(int fd, char *out);

/*
 * Reads bytes from fd until a newline or EOF.
 * The newline is not included in the returned string.
 * Returns a null-terminated GC string, or NULL on error.
 */
char *peko_read_line(int fd);

/*
 * Opens the file at path with the given flag bitmask.
 * Returns the file descriptor on success, or -1 on error.
 */
int peko_open_fd(const char *path, int flags);

/*
 * Closes a file descriptor previously opened with peko_open_fd.
 * Returns 0 on success, -1 on error.
 */
int peko_close_fd(int fd);

/*
 * Opens /dev/null (Unix) or NUL (Windows) for writing.
 * Returns the fd on success, or -1 on error.
 */
int peko_open_null_fd(void);

#endif /* PEKO_IO_CONSOLE_H */
