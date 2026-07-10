/*
 * peko_process.c
 *
 * Child process spawning and stdio piping for std::process. A process is
 * launched with an argument vector and an optional working directory; its
 * stdin, stdout, and stderr are connected to pipes the parent reads and writes.
 * Line reads and process waits block, so they are bracketed with the GC
 * park/unpark calls so a collection can proceed while the thread waits.
 *
 * Unix uses fork/exec with pipe(); Windows uses CreateProcess with CreatePipe.
 * Process handles are unmanaged malloc allocations the caller owns and frees.
 */

#include <stdlib.h>
#include <string.h>

/* The GC parks the calling thread across a blocking call. */
extern void pgc_begin_blocking(void);
extern void pgc_end_blocking(void);

/* A growable argument vector built up by the caller before spawning. */
typedef struct {
    char **items;
    int count;
    int cap;
} PekoArgv;

void *peko_argv_new(void)
{
    PekoArgv *a = (PekoArgv *)malloc(sizeof(PekoArgv));
    if (!a)
        return NULL;
    a->items = NULL;
    a->count = 0;
    a->cap = 0;
    return a;
}

void peko_argv_push(void *argv, const char *arg)
{
    PekoArgv *a = (PekoArgv *)argv;
    if (!a)
        return;
    if (a->count >= a->cap) {
        a->cap = a->cap ? a->cap * 2 : 8;
        a->items = (char **)realloc(a->items, sizeof(char *) * (size_t)a->cap);
    }
    a->items[a->count++] = strdup(arg ? arg : "");
}

void peko_argv_free(void *argv)
{
    PekoArgv *a = (PekoArgv *)argv;
    if (!a)
        return;
    for (int i = 0; i < a->count; i++)
        free(a->items[i]);
    free(a->items);
    free(a);
}

/* A buffered read stream over one pipe end, with a reusable line buffer. */
typedef struct {
#ifdef _WIN32
    void *handle; /* HANDLE */
#else
    int fd;
#endif
    unsigned char rbuf[4096];
    int rpos;
    int rlen;
    int eof;
    char *line;
    size_t line_cap;
} PekoStream;

typedef struct {
#ifdef _WIN32
    void *process; /* HANDLE */
    void *stdin_w;
    int pid;
#else
    int pid;
    int stdin_fd;
#endif
    int done; /* whether the child has been reaped */
    int code; /* the reaped exit code */
    PekoStream out;
    PekoStream err;
} PekoProcess;

/* ------------------------------------------------------------------------- */
/* Platform primitives: raw read/write/close, spawn, wait, kill.             */
/* ------------------------------------------------------------------------- */

#ifdef _WIN32

#include <windows.h>

static int stream_raw_read(PekoStream *s, unsigned char *buf, int len)
{
    DWORD got = 0;
    if (!ReadFile((HANDLE)s->handle, buf, (DWORD)len, &got, NULL))
        return 0; /* broken pipe reads as end of output */
    return (int)got;
}

/* Quote one argument for the Windows command-line parsing rules. */
static void append_quoted(char **cmd, size_t *len, size_t *cap, const char *arg)
{
    size_t need = strlen(arg) * 2 + 4;
    if (*len + need >= *cap) {
        *cap = (*len + need) * 2;
        *cmd = (char *)realloc(*cmd, *cap);
    }
    char *p = *cmd + *len;
    if (*len)
        *p++ = ' ';
    *p++ = '"';
    for (const char *c = arg; *c; c++) {
        if (*c == '"' || *c == '\\')
            *p++ = '\\';
        *p++ = *c;
    }
    *p++ = '"';
    *p = '\0';
    *len = (size_t)(p - *cmd);
}

void *peko_process_spawn(const char *program, void *argv, const char *cwd)
{
    PekoArgv *av = (PekoArgv *)argv;
    HANDLE in_r = NULL, in_w = NULL, out_r = NULL, out_w = NULL, err_r = NULL, err_w = NULL;
    SECURITY_ATTRIBUTES sa;
    sa.nLength = sizeof(sa);
    sa.bInheritHandle = TRUE;
    sa.lpSecurityDescriptor = NULL;
    if (!CreatePipe(&in_r, &in_w, &sa, 0) || !CreatePipe(&out_r, &out_w, &sa, 0) ||
        !CreatePipe(&err_r, &err_w, &sa, 0))
        return NULL;
    /* The parent ends are not inherited by the child. */
    SetHandleInformation(in_w, HANDLE_FLAG_INHERIT, 0);
    SetHandleInformation(out_r, HANDLE_FLAG_INHERIT, 0);
    SetHandleInformation(err_r, HANDLE_FLAG_INHERIT, 0);

    size_t len = 0, cap = 256;
    char *cmd = (char *)malloc(cap);
    cmd[0] = '\0';
    append_quoted(&cmd, &len, &cap, program);
    for (int i = 0; av && i < av->count; i++)
        append_quoted(&cmd, &len, &cap, av->items[i]);

    STARTUPINFOA si;
    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = in_r;
    si.hStdOutput = out_w;
    si.hStdError = err_w;
    PROCESS_INFORMATION pi;
    ZeroMemory(&pi, sizeof(pi));

    // CREATE_NO_WINDOW keeps a console child from popping a console window when
    // the parent is a GUI app (Peko Studio spawning curl, tar, the language
    // server, and so on). Output is still captured through the redirected pipes.
    BOOL ok = CreateProcessA(NULL, cmd, NULL, NULL, TRUE, CREATE_NO_WINDOW, NULL,
                             (cwd && cwd[0]) ? cwd : NULL, &si, &pi);
    free(cmd);
    CloseHandle(in_r);
    CloseHandle(out_w);
    CloseHandle(err_w);
    if (!ok) {
        CloseHandle(in_w);
        CloseHandle(out_r);
        CloseHandle(err_r);
        return NULL;
    }
    CloseHandle(pi.hThread);

    PekoProcess *p = (PekoProcess *)calloc(1, sizeof(PekoProcess));
    p->process = (void *)pi.hProcess;
    p->stdin_w = (void *)in_w;
    p->pid = (int)pi.dwProcessId;
    p->out.handle = (void *)out_r;
    p->err.handle = (void *)err_r;
    return p;
}

int peko_process_write(void *handle, const char *data, int len)
{
    PekoProcess *p = (PekoProcess *)handle;
    DWORD wrote = 0;
    if (!WriteFile((HANDLE)p->stdin_w, data, (DWORD)len, &wrote, NULL))
        return -1;
    return (int)wrote;
}

void peko_process_close_stdin(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (p->stdin_w) {
        CloseHandle((HANDLE)p->stdin_w);
        p->stdin_w = NULL;
    }
}

int peko_process_wait(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (p->done)
        return p->code;
    pgc_begin_blocking();
    WaitForSingleObject((HANDLE)p->process, INFINITE);
    pgc_end_blocking();
    DWORD code = 0;
    GetExitCodeProcess((HANDLE)p->process, &code);
    p->done = 1;
    p->code = (int)code;
    return p->code;
}

int peko_process_is_running(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (p->done)
        return 0;
    DWORD code = 0;
    if (!GetExitCodeProcess((HANDLE)p->process, &code))
        return 0;
    if (code == STILL_ACTIVE)
        return 1;
    p->done = 1;
    p->code = (int)code;
    return 0;
}

void peko_process_kill(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    TerminateProcess((HANDLE)p->process, 1);
}

static void stream_close(PekoStream *s)
{
    if (s->handle) {
        CloseHandle((HANDLE)s->handle);
        s->handle = NULL;
    }
}

void peko_process_free(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (!p)
        return;
    peko_process_close_stdin(handle);
    stream_close(&p->out);
    stream_close(&p->err);
    if (p->process)
        CloseHandle((HANDLE)p->process);
    free(p->out.line);
    free(p->err.line);
    free(p);
}

#else /* Unix */

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

static int stream_raw_read(PekoStream *s, unsigned char *buf, int len)
{
    ssize_t got;
    do {
        got = read(s->fd, buf, (size_t)len);
    } while (got < 0 && errno == EINTR);
    return (got < 0) ? 0 : (int)got;
}

void *peko_process_spawn(const char *program, void *argv, const char *cwd)
{
    PekoArgv *av = (PekoArgv *)argv;

    /* A child that exits or fails to start closes its stdin read end. A later
     * write to that pipe raises SIGPIPE, whose default action kills this
     * process. Ignore it so the write returns EPIPE instead. */
    signal(SIGPIPE, SIG_IGN);

    int in_pipe[2], out_pipe[2], err_pipe[2];
    if (pipe(in_pipe) != 0)
        return NULL;
    if (pipe(out_pipe) != 0) {
        close(in_pipe[0]);
        close(in_pipe[1]);
        return NULL;
    }
    if (pipe(err_pipe) != 0) {
        close(in_pipe[0]);
        close(in_pipe[1]);
        close(out_pipe[0]);
        close(out_pipe[1]);
        return NULL;
    }

    /* A close-on-exec pipe carries exec failure back to the parent. A
     * successful exec closes the write end and the parent reads end of file; a
     * failed exec writes errno and the parent reports no process. */
    int exec_pipe[2];
    if (pipe(exec_pipe) != 0) {
        close(in_pipe[0]);
        close(in_pipe[1]);
        close(out_pipe[0]);
        close(out_pipe[1]);
        close(err_pipe[0]);
        close(err_pipe[1]);
        return NULL;
    }
    fcntl(exec_pipe[1], F_SETFD, FD_CLOEXEC);

    /* argv for exec: [program, args..., NULL]. */
    int argc = av ? av->count : 0;
    char **argp = (char **)malloc(sizeof(char *) * (size_t)(argc + 2));
    argp[0] = (char *)program;
    for (int i = 0; i < argc; i++)
        argp[i + 1] = av->items[i];
    argp[argc + 1] = NULL;

    pid_t pid = fork();
    if (pid < 0) {
        free(argp);
        close(in_pipe[0]);
        close(in_pipe[1]);
        close(out_pipe[0]);
        close(out_pipe[1]);
        close(err_pipe[0]);
        close(err_pipe[1]);
        close(exec_pipe[0]);
        close(exec_pipe[1]);
        return NULL;
    }
    if (pid == 0) {
        /* Child: wire the pipe ends to stdio, then exec. Only async-signal-safe
         * calls run here. */
        dup2(in_pipe[0], 0);
        dup2(out_pipe[1], 1);
        dup2(err_pipe[1], 2);
        close(in_pipe[0]);
        close(in_pipe[1]);
        close(out_pipe[0]);
        close(out_pipe[1]);
        close(err_pipe[0]);
        close(err_pipe[1]);
        close(exec_pipe[0]);
        if (cwd && cwd[0]) {
            if (chdir(cwd) != 0) {
                int e = errno;
                (void)write(exec_pipe[1], &e, sizeof(e));
                _exit(127);
            }
        }
        execvp(program, argp);
        int e = errno;
        (void)write(exec_pipe[1], &e, sizeof(e));
        _exit(127);
    }

    /* Parent: keep the writable stdin and readable stdout/stderr ends. */
    free(argp);
    close(in_pipe[0]);
    close(out_pipe[1]);
    close(err_pipe[1]);

    /* Wait for the child to exec or report why it could not. */
    close(exec_pipe[1]);
    int child_errno = 0;
    ssize_t got;
    do {
        got = read(exec_pipe[0], &child_errno, sizeof(child_errno));
    } while (got < 0 && errno == EINTR);
    close(exec_pipe[0]);
    if (got > 0) {
        /* The program could not be started. Reap the child and report none. */
        int status;
        waitpid(pid, &status, 0);
        close(in_pipe[1]);
        close(out_pipe[0]);
        close(err_pipe[0]);
        return NULL;
    }

    PekoProcess *p = (PekoProcess *)calloc(1, sizeof(PekoProcess));
    p->pid = (int)pid;
    p->stdin_fd = in_pipe[1];
    p->out.fd = out_pipe[0];
    p->err.fd = err_pipe[0];
    return p;
}

int peko_process_write(void *handle, const char *data, int len)
{
    PekoProcess *p = (PekoProcess *)handle;
    ssize_t wrote = write(p->stdin_fd, data, (size_t)len);
    return (int)wrote;
}

void peko_process_close_stdin(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (p->stdin_fd >= 0) {
        close(p->stdin_fd);
        p->stdin_fd = -1;
    }
}

static int exit_code_of(int status)
{
    if (WIFEXITED(status))
        return WEXITSTATUS(status);
    if (WIFSIGNALED(status))
        return 128 + WTERMSIG(status);
    return -1;
}

int peko_process_wait(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (p->done)
        return p->code;
    int status = 0;
    pgc_begin_blocking();
    pid_t r;
    do {
        r = waitpid((pid_t)p->pid, &status, 0);
    } while (r < 0 && errno == EINTR);
    pgc_end_blocking();
    p->done = 1;
    p->code = exit_code_of(status);
    return p->code;
}

int peko_process_is_running(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (p->done)
        return 0;
    int status = 0;
    pid_t r = waitpid((pid_t)p->pid, &status, WNOHANG);
    if (r == 0)
        return 1; /* still running */
    if (r < 0)
        return 0;
    p->done = 1;
    p->code = exit_code_of(status);
    return 0;
}

void peko_process_kill(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    kill((pid_t)p->pid, SIGKILL);
}

static void stream_close(PekoStream *s)
{
    if (s->fd >= 0) {
        close(s->fd);
        s->fd = -1;
    }
}

void peko_process_free(void *handle)
{
    PekoProcess *p = (PekoProcess *)handle;
    if (!p)
        return;
    peko_process_close_stdin(handle);
    stream_close(&p->out);
    stream_close(&p->err);
    free(p->out.line);
    free(p->err.line);
    free(p);
}

#endif

/* ------------------------------------------------------------------------- */
/* Shared buffered line reading over a stream.                               */
/* ------------------------------------------------------------------------- */

static int stream_read_line(PekoStream *s)
{
    size_t n = 0;
    if (!s->line) {
        s->line_cap = 256;
        s->line = (char *)malloc(s->line_cap);
    }
    pgc_begin_blocking();
    for (;;) {
        while (s->rpos < s->rlen) {
            unsigned char c = s->rbuf[s->rpos++];
            if (c == '\n')
                goto done;
            if (c == '\r')
                continue;
            if (n + 1 >= s->line_cap) {
                s->line_cap *= 2;
                s->line = (char *)realloc(s->line, s->line_cap);
            }
            s->line[n++] = (char)c;
        }
        int r = stream_raw_read(s, s->rbuf, (int)sizeof(s->rbuf));
        if (r <= 0) {
            s->eof = 1;
            break;
        }
        s->rpos = 0;
        s->rlen = r;
    }
done:
    pgc_end_blocking();
    if (n == 0 && s->eof)
        return 0; /* end of output, no trailing partial line */
    s->line[n] = '\0';
    return 1;
}

/* Read up to n raw bytes into the stream's line buffer, draining any readahead
   first. NUL-terminates and returns the count read (0 at end of output). For
   framed protocols (LSP Content-Length bodies) where line reading does not fit. */
static int stream_read_bytes(PekoStream *s, int n)
{
    if (n < 0)
        n = 0;
    if (!s->line || s->line_cap < (size_t)n + 1) {
        s->line_cap = (size_t)n + 1;
        s->line = (char *)realloc(s->line, s->line_cap);
    }
    int got = 0;
    pgc_begin_blocking();
    while (got < n && s->rpos < s->rlen)
        s->line[got++] = (char)s->rbuf[s->rpos++];
    while (got < n) {
        int r = stream_raw_read(s, (unsigned char *)s->line + got, n - got);
        if (r <= 0) {
            s->eof = 1;
            break;
        }
        got += r;
    }
    pgc_end_blocking();
    s->line[got] = '\0';
    return got;
}

static PekoStream *stream_of(PekoProcess *p, int which)
{
    return which == 2 ? &p->err : &p->out;
}

/* Read up to n raw bytes from stdout (which=1) or stderr (which=2) into the
   stream buffer, returning the count read (0 at end of output). Blocking. */
int peko_process_read_bytes(void *handle, int which, int n)
{
    return stream_read_bytes(stream_of((PekoProcess *)handle, which), n);
}

/* Read the next line from stdout (which=1) or stderr (which=2), stripping the
   newline. Returns 1 when a line is available, 0 at end of output. Blocking. */
int peko_process_read_line(void *handle, int which)
{
    return stream_read_line(stream_of((PekoProcess *)handle, which));
}

/* The last line read from stdout (which=1) or stderr (which=2). */
const char *peko_process_line(void *handle, int which)
{
    PekoStream *s = stream_of((PekoProcess *)handle, which);
    return s->line ? s->line : "";
}

int peko_process_pid(void *handle)
{
    return ((PekoProcess *)handle)->pid;
}
