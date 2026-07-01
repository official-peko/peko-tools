/*
 * fs.c
 * Filesystem primitives backing std::fs. Pure C99 over stdio and POSIX stat.
 * String reads return a fresh GC-managed atomic byte buffer the collector
 * owns; paths and write text are read synchronously from GC string buffers.
 * Binary buffers, directory listing, and buffered readers live in a later
 * phase.
 */

#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <windows.h>
#  include <io.h>
#  ifndef S_ISDIR
#    define S_ISDIR(m) (((m) & _S_IFMT) == _S_IFDIR)
#  endif
#  ifndef S_ISREG
#    define S_ISREG(m) (((m) & _S_IFMT) == _S_IFREG)
#  endif
#  ifndef S_ISLNK
#    define S_ISLNK(m) (0)
#  endif
#  ifndef S_ISBLK
#    define S_ISBLK(m) (0)
#  endif
#else
#  include <dirent.h>
#  include <unistd.h>
#endif

extern void *pgc_alloc_atomic(size_t size);

/* Initial capacity and growth chunk for reads of an unknown-size stream. */
#define FS_READ_INITIAL_SIZE 4096
#define FS_READ_CHUNK 4096

/* Open mode bits. These match the OpenMode enum mapping in fs.peko. */
#define FS_MODE_READ 1
#define FS_MODE_WRITE 2
#define FS_MODE_APPEND 4
#define FS_MODE_BINARY 8
#define FS_MODE_READ_WRITE 16

/* Maps a Peko mode bitmask to a C fopen mode string. The returned pointer is
 * static and is not freed. */
static const char *fs_mode_string(int mode)
{
    int rw = (mode & FS_MODE_READ_WRITE) != 0;
    int rd = (mode & FS_MODE_READ) != 0;
    int wr = (mode & FS_MODE_WRITE) != 0;
    int ap = (mode & FS_MODE_APPEND) != 0;
    int bin = (mode & FS_MODE_BINARY) != 0;

    if (rw) return bin ? "r+b" : "r+";
    if (ap) return bin ? "ab" : "a";
    if (wr) return bin ? "wb" : "w";
    if (rd) return bin ? "rb" : "r";
    return "r";
}

/* The size in bytes of an open FILE, or -1 when the stream is not seekable.
 * The original file position is restored before returning. */
static long fs_file_size(FILE *fp)
{
    long original = ftell(fp);
    if (original < 0)
        return -1;
    if (fseek(fp, 0, SEEK_END) != 0)
        return -1;
    long size = ftell(fp);
    fseek(fp, original, SEEK_SET);
    return size;
}

/* A cross-platform fopen wrapper that returns NULL on failure. */
static FILE *fs_fopen(const char *path, const char *mode)
{
#ifdef _WIN32
    FILE *fp = NULL;
    fopen_s(&fp, path, mode);
    return fp;
#else
    return fopen(path, mode);
#endif
}

/* =========================================================================
 * Metadata
 * ====================================================================== */

bool fs_exists(const char *fpath)
{
    struct stat buf;
    return stat(fpath, &buf) == 0;
}

int fs_get_mode(const char *fpath)
{
    struct stat buf;
    if (stat(fpath, &buf) != 0)
        return -1;
    return (int)buf.st_mode;
}

bool fs_is_directory(const char *fpath)
{
    struct stat buf;
    if (stat(fpath, &buf) != 0)
        return false;
    return S_ISDIR(buf.st_mode);
}

bool fs_is_regular(const char *fpath)
{
    struct stat buf;
    if (stat(fpath, &buf) != 0)
        return false;
    return S_ISREG(buf.st_mode);
}

bool fs_is_link(const char *fpath)
{
    struct stat buf;
#ifdef _WIN32
    if (stat(fpath, &buf) != 0)
        return false;
#else
    /* lstat does not follow the final symlink in the path. */
    if (lstat(fpath, &buf) != 0)
        return false;
#endif
    return S_ISLNK(buf.st_mode);
}

bool fs_is_block(const char *fpath)
{
    struct stat buf;
    if (stat(fpath, &buf) != 0)
        return false;
#ifdef S_ISBLK
    return S_ISBLK(buf.st_mode);
#else
    return false;
#endif
}

bool fs_chmod(const char *fpath, int mode)
{
#ifndef _WIN32
    return chmod(fpath, (mode_t)mode) == 0;
#else
    int win_mode = 0;
    if (mode & 0444) win_mode |= _S_IREAD;
    if (mode & 0222) win_mode |= _S_IWRITE;
    return _chmod(fpath, win_mode) == 0;
#endif
}

/* =========================================================================
 * Handle open and close
 * ====================================================================== */

void *fs_open_handle(const char *fpath, int mode)
{
    return (void *)fs_fopen(fpath, fs_mode_string(mode));
}

void fs_close_handle(void *handle)
{
    if (handle)
        fclose((FILE *)handle);
}

/* =========================================================================
 * String reads. The buffer is GC-managed and NUL-terminated.
 * ====================================================================== */

char *fs_read_string(void *handle, int n)
{
    FILE *fp = (FILE *)handle;
    char *buf = (char *)pgc_alloc_atomic((size_t)(n + 1));
    if (!buf)
        return NULL;

    int bytes_read = (int)fread(buf, 1, (size_t)n, fp);
    if (bytes_read <= 0)
        return NULL;

    buf[bytes_read] = '\0';
    return buf;
}

char *fs_read_all_string(void *handle)
{
    FILE *fp = (FILE *)handle;
    long size = fs_file_size(fp);

    if (size >= 0) {
        char *buf = (char *)pgc_alloc_atomic((size_t)(size + 1));
        if (!buf)
            return NULL;
        int bytes_read = (int)fread(buf, 1, (size_t)size, fp);
        buf[bytes_read] = '\0';
        return buf;
    }

    /* A non-seekable stream grows a scratch buffer, then copies the result
     * into GC memory once the length is known. */
    size_t capacity = FS_READ_INITIAL_SIZE;
    size_t length = 0;
    char *buf = (char *)malloc(capacity);
    if (!buf)
        return NULL;

    for (;;) {
        if (length + FS_READ_CHUNK + 1 > capacity) {
            capacity *= 2;
            char *tmp = (char *)realloc(buf, capacity);
            if (!tmp) {
                free(buf);
                return NULL;
            }
            buf = tmp;
        }
        size_t n = fread(buf + length, 1, FS_READ_CHUNK, fp);
        length += n;
        if (n < FS_READ_CHUNK)
            break;
    }
    buf[length] = '\0';

    char *gc_buf = (char *)pgc_alloc_atomic(length + 1);
    if (!gc_buf) {
        free(buf);
        return NULL;
    }
    memcpy(gc_buf, buf, length + 1);
    free(buf);
    return gc_buf;
}

/* =========================================================================
 * Writes, seek, tell, flush
 * ====================================================================== */

int fs_write_string(void *handle, const char *text)
{
    int len = (int)strlen(text);
    int result = fputs(text, (FILE *)handle);
    return (result >= 0) ? len : -1;
}

int fs_seek(void *handle, long offset, int origin)
{
    return fseek((FILE *)handle, offset, origin) == 0 ? 0 : -1;
}

long fs_tell(void *handle)
{
    return ftell((FILE *)handle);
}

int fs_flush(void *handle)
{
    return fflush((FILE *)handle) == 0 ? 0 : -1;
}

/* =========================================================================
 * Filesystem operations
 * ====================================================================== */

bool fs_mkdir(const char *dirpath)
{
#ifndef _WIN32
    return mkdir(dirpath, 0777) == 0;
#else
    return CreateDirectoryA(dirpath, NULL) != 0;
#endif
}

bool fs_remove(const char *fpath)
{
    return remove(fpath) == 0;
}

bool fs_copy(const char *src, const char *dst)
{
    FILE *in = fs_fopen(src, "rb");
    FILE *out = fs_fopen(dst, "wb");
    char chunk[FS_READ_CHUNK];
    size_t n;
    bool ok = true;

    if (!in || !out) {
        if (in) fclose(in);
        if (out) fclose(out);
        return false;
    }

    while ((n = fread(chunk, 1, sizeof(chunk), in)) > 0) {
        if (fwrite(chunk, 1, n, out) != n) {
            ok = false;
            break;
        }
    }

    fclose(in);
    fclose(out);
    return ok;
}

bool fs_move(const char *src, const char *dst)
{
    if (rename(src, dst) == 0)
        return true;

    /* A cross-volume rename fails, so fall back to copy then remove. */
    if (!fs_copy(src, dst))
        return false;
    return fs_remove(src);
}

/* =========================================================================
 * Convenience helpers that open, operate, and close in one call
 * ====================================================================== */

char *fs_helper_read_file(const char *fpath)
{
    FILE *fp = fs_fopen(fpath, "r");
    if (!fp)
        return NULL;
    char *result = fs_read_all_string((void *)fp);
    fclose(fp);
    return result;
}

bool fs_helper_write_file(const char *fpath, const char *text)
{
    FILE *fp = fs_fopen(fpath, "w");
    if (!fp)
        return false;
    int rc = fs_write_string((void *)fp, text);
    fclose(fp);
    return rc >= 0;
}

bool fs_helper_append_file(const char *fpath, const char *text)
{
    FILE *fp = fs_fopen(fpath, "a");
    if (!fp)
        return false;
    int rc = fs_write_string((void *)fp, text);
    fclose(fp);
    return rc >= 0;
}

bool fs_helper_copy(const char *src, const char *dst)
{
    return fs_copy(src, dst);
}

bool fs_helper_move(const char *src, const char *dst)
{
    return fs_move(src, dst);
}

/* =========================================================================
 * Binary reads and writes. The buffer is a GC-managed buffer the caller
 * allocates and owns, filled or drained synchronously without allocating.
 * ====================================================================== */

/* The number of bytes from the cursor to the end of the file, or -1 when the
 * stream is not seekable. The original cursor is restored. */
long fs_remaining(void *handle)
{
    FILE *fp = (FILE *)handle;
    long cur = ftell(fp);
    if (cur < 0)
        return -1;
    if (fseek(fp, 0, SEEK_END) != 0)
        return -1;
    long end = ftell(fp);
    fseek(fp, cur, SEEK_SET);
    return end - cur;
}

int fs_read_into(void *handle, void *buf, int n)
{
    return (int)fread(buf, 1, (size_t)n, (FILE *)handle);
}

int fs_write_from(void *handle, const void *buf, int n)
{
    size_t written = fwrite(buf, 1, (size_t)n, (FILE *)handle);
    return (written == (size_t)n) ? (int)written : -1;
}

/* =========================================================================
 * Directory iteration. The handle is an unmanaged OS directory stream. Each
 * entry name is valid until the next step or close, so the caller copies it
 * into managed memory right away.
 * ====================================================================== */

#ifdef _WIN32

typedef struct {
    HANDLE handle;
    WIN32_FIND_DATAA entry;
    int pending;
    int done;
} fs_dir_t;

void *fs_dir_open(const char *dirpath)
{
    char pattern[MAX_PATH];
    snprintf(pattern, sizeof(pattern), "%s\\*", dirpath);

    fs_dir_t *dir = (fs_dir_t *)malloc(sizeof(fs_dir_t));
    if (!dir)
        return NULL;

    dir->handle = FindFirstFileA(pattern, &dir->entry);
    dir->done = 0;
    dir->pending = (dir->handle != INVALID_HANDLE_VALUE) ? 1 : 0;
    if (dir->handle == INVALID_HANDLE_VALUE)
        dir->done = 1;
    return (void *)dir;
}

const char *fs_dir_next(void *handle)
{
    fs_dir_t *dir = (fs_dir_t *)handle;
    if (!dir || dir->done)
        return NULL;

    for (;;) {
        if (!dir->pending) {
            if (!FindNextFileA(dir->handle, &dir->entry)) {
                dir->done = 1;
                return NULL;
            }
        }
        dir->pending = 0;

        const char *name = dir->entry.cFileName;
        if (strcmp(name, ".") != 0 && strcmp(name, "..") != 0)
            return name;
    }
}

void fs_dir_close(void *handle)
{
    fs_dir_t *dir = (fs_dir_t *)handle;
    if (!dir)
        return;
    if (dir->handle != INVALID_HANDLE_VALUE)
        FindClose(dir->handle);
    free(dir);
}

#else

void *fs_dir_open(const char *dirpath)
{
    return (void *)opendir(dirpath);
}

const char *fs_dir_next(void *handle)
{
    DIR *dir = (DIR *)handle;
    if (!dir)
        return NULL;

    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0)
            return entry->d_name;
    }
    return NULL;
}

void fs_dir_close(void *handle)
{
    if (handle)
        closedir((DIR *)handle);
}

#endif

/* =========================================================================
 * Buffered comparison
 * ====================================================================== */

/* Compares the file at fpath against snapshot as it reads, without loading the
 * whole file. Returns true when the contents differ. */
bool fs_content_changed(const char *fpath, const char *snapshot, int snapshot_len)
{
    FILE *fp = fs_fopen(fpath, "r");
    if (!fp)
        return true;

    int i = 0;
    int ch;
    bool same = true;

    while ((ch = fgetc(fp)) != EOF) {
        if (i >= snapshot_len || ch != (unsigned char)snapshot[i]) {
            same = false;
            break;
        }
        i++;
    }

    if (same && i != snapshot_len)
        same = false;

    fclose(fp);
    return !same;
}
