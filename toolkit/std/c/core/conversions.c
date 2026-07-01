/*
 * conversions.c
 * Value <-> string conversion primitives for std::core. The to-string helpers
 * format into a fresh GC-managed atomic buffer the collector owns, so the
 * result is a real managed byte buffer a Pekoscript `string` can wrap. The
 * from-string helpers parse a raw C string with libc.
 */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../runtime/include/pgc.h"

/* Copy a NUL-terminated buffer into a fresh managed atomic allocation. */
static void *managed_copy(const char *bytes, size_t length_with_nul)
{
    void *buffer = pgc_alloc_atomic(length_with_nul);
    memcpy(buffer, bytes, length_with_nul);
    return buffer;
}

void *peko_managed_from_cstr(const char *src)
{
    return managed_copy(src, strlen(src) + 1);
}

int64_t peko_buffer_length(const char *buffer)
{
    return (int64_t)strlen(buffer);
}

void *peko_int_to_cstr(int64_t value)
{
    char scratch[32];
    int written = snprintf(scratch, sizeof scratch, "%lld", (long long)value);
    return managed_copy(scratch, (size_t)written + 1);
}

void *peko_float_to_cstr(double value)
{
    char scratch[64];

    /* An integral value within the exact-integer range of an f64 prints with
       no decimal point or exponent, so a whole number reads as a whole
       number. */
    if (value == (double)(int64_t)value && value >= -9.007199254740992e15 &&
        value <= 9.007199254740992e15) {
        int written = snprintf(scratch, sizeof scratch, "%lld",
                               (long long)(int64_t)value);
        return managed_copy(scratch, (size_t)written + 1);
    }

    /* Otherwise pick the shortest decimal that round-trips back to the same
       f64, so no decimal precision is lost and the output stays compact. */
    for (int precision = 1; precision <= 17; precision++) {
        int written = snprintf(scratch, sizeof scratch, "%.*g", precision, value);
        if (strtod(scratch, NULL) == value)
            return managed_copy(scratch, (size_t)written + 1);
    }

    int written = snprintf(scratch, sizeof scratch, "%.17g", value);
    return managed_copy(scratch, (size_t)written + 1);
}

void *peko_bool_to_cstr(bool value)
{
    const char *text = value ? "true" : "false";
    return managed_copy(text, strlen(text) + 1);
}

void *peko_char_to_cstr(int8_t value)
{
    char scratch[2];
    scratch[0] = (char)value;
    scratch[1] = '\0';
    return managed_copy(scratch, 2);
}

int64_t peko_cstr_to_int(const char *text)
{
    return (int64_t)strtoll(text, NULL, 10);
}

double peko_cstr_to_float(const char *text)
{
    return strtod(text, NULL);
}

bool peko_cstr_to_bool(const char *text)
{
    return strcmp(text, "true") == 0;
}

int8_t peko_cstr_to_char(const char *text)
{
    return (int8_t)text[0];
}

/* -------------------------------------------------------------------------
 * Optional unwrap halt
 *
 * An unwrap of a None or Error in a non-optional function halts the program.
 * The Option walks its captured context chain and prints a backtrace: a header
 * line naming the failure, then one frame line per propagation site, newest
 * first. peko_halt_end flushes and exits with a non-zero status.
 * ---------------------------------------------------------------------- */

void peko_halt_begin(int is_error)
{
    fputs("\n", stderr);
    if (is_error) {
        fputs("halted: unwrapped an Error optional\n", stderr);
    } else {
        fputs("halted: unwrapped a None optional\n", stderr);
    }
}

void peko_halt_frame(const char *file, int line, int character)
{
    const char *name = (file && file[0]) ? file : "<unknown>";
    fprintf(stderr, "  at %s:%d:%d\n", name, line, character);
}

void peko_halt_end(void)
{
    fflush(stderr);
    exit(101);
}
