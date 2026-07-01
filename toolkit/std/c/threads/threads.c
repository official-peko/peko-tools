/*
 * peko_threads.c
 * Mutex, condition variable, thread creation, and cancellation token
 * implementation for the Peko threads library.
 */

#include "peko_threads.h"
#include <stdio.h>

/* =========================================================================
 * Thread closure lifetime management
 *
 * The precise GC uses handles to keep closure data alive between
 * peko_thread_create() returning and the OS thread calling
 * pgc_thread_attach(). A handle is a stable integer reference that
 * survives collections and keeps the referent alive until released.
 *
 * fd->handle holds the handle for the closure context. It is created before the
 * thread is spawned and released inside the trampoline after
 * pgc_thread_attach() is called (at which point the thread's stack
 * holds a live reference to the data).
 * ====================================================================== */



/* Allocate a mutex on the C heap (not GC heap) so it cannot be moved
 * by the collector. Returns an opaque pointer stored in Peko's Mutex.handle. */
peko_mutex_t *peko_mutex_new(void)
{
    peko_mutex_t *m = (peko_mutex_t *)malloc(sizeof(peko_mutex_t));
    if (m) peko_mutex_init(m);
    return m;
}

void peko_mutex_free(peko_mutex_t *m)
{
    if (!m) return;
    peko_mutex_destroy(m);
    free(m);
}

void peko_mutex_init(peko_mutex_t *m)
{
#ifdef _WIN32
    InitializeCriticalSection(&m->cs);
    InitializeConditionVariable(&m->cv);
#else
    pthread_mutex_init(&m->mutex, NULL);
    pthread_cond_init(&m->cond, NULL);
#endif
}

void peko_mutex_destroy(peko_mutex_t *m)
{
#ifdef _WIN32
    DeleteCriticalSection(&m->cs);
    /* Windows CONDITION_VARIABLE needs no cleanup. */
#else
    pthread_mutex_destroy(&m->mutex);
    pthread_cond_destroy(&m->cond);
#endif
}

void peko_mutex_lock(peko_mutex_t *m)
{
#ifdef _WIN32
    EnterCriticalSection(&m->cs);
#else
    pthread_mutex_lock(&m->mutex);
#endif
}

void peko_mutex_unlock(peko_mutex_t *m)
{
#ifdef _WIN32
    LeaveCriticalSection(&m->cs);
#else
    pthread_mutex_unlock(&m->mutex);
#endif
}

bool peko_cond_wait(peko_mutex_t *m, int timeout_ms)
{
#ifdef _WIN32
    if (timeout_ms < 0) {
        SleepConditionVariableCS(&m->cv, &m->cs, INFINITE);
        return true;
    }
    return SleepConditionVariableCS(&m->cv, &m->cs,
                                    (DWORD)timeout_ms) != 0;
#else
    if (timeout_ms < 0) {
        pthread_cond_wait(&m->cond, &m->mutex);
        return true;
    }

    struct timeval  tv;
    struct timespec ts;
    gettimeofday(&tv, NULL);
    ts.tv_sec  = tv.tv_sec  + timeout_ms / 1000;
    ts.tv_nsec = tv.tv_usec * 1000 + (timeout_ms % 1000) * 1000000L;
    if (ts.tv_nsec >= 1000000000L) {
        ts.tv_sec++;
        ts.tv_nsec -= 1000000000L;
    }
    return pthread_cond_timedwait(&m->cond, &m->mutex, &ts) != ETIMEDOUT;
#endif
}

void peko_cond_signal(peko_mutex_t *m)
{
#ifdef _WIN32
    WakeConditionVariable(&m->cv);
#else
    pthread_cond_signal(&m->cond);
#endif
}

void peko_cond_broadcast(peko_mutex_t *m)
{
#ifdef _WIN32
    WakeAllConditionVariable(&m->cv);
#else
    pthread_cond_broadcast(&m->cond);
#endif
}

/* =========================================================================
 * Thread worker trampoline
 * ====================================================================== */

#ifdef _WIN32
static DWORD WINAPI peko_thread_trampoline(void *arg)
#else
static void *peko_thread_trampoline(void *arg)
#endif
{
    peko_func_data_t *fd = (peko_func_data_t *)arg;
    pgc_thread_attach();
    /* Thread is now attached. Fetch the current (possibly moved) address of
     * the closure context via the handle, then release it. After attach the
     * GC's stack map walk will keep the context alive through the worker's
     * own stack frames, so the handle is no longer needed. */
    void *ctx = pgc_handle_get(fd->handle);
    pgc_handle_release(fd->handle);
    fd->handle = PGC_NULL_HANDLE;
    fd->worker(ctx);
    pgc_thread_detach();
    free(fd);

#ifdef _WIN32
    return 0;
#else
    return NULL;
#endif
}

/* =========================================================================
 * Thread create / kill / free
 * ====================================================================== */

peko_thread_t *peko_thread_create(void (*worker)(void *), void *data,
                                   bool synchronous)
{
    peko_func_data_t *fd = (peko_func_data_t *)malloc(sizeof(peko_func_data_t));
    if (!fd)
        return NULL;

    fd->worker = worker;
    fd->handle = pgc_handle_create(data);  /* keep data alive until thread attaches */

    peko_thread_t *t = (peko_thread_t *)malloc(sizeof(peko_thread_t));
    if (!t) {
        pgc_handle_release(fd->handle);
        free(fd);
        return NULL;
    }

    t->func_data = fd;
    atomic_store(&t->detached, 0);

#ifdef _WIN32
    DWORD id;
    t->handle = CreateThread(NULL, 0, peko_thread_trampoline, fd, 0, &id);
    if (!t->handle) {
        free(fd);
        free(t);
        return NULL;
    }

    if (synchronous) {
        PGC_BLOCKING(WaitForSingleObject(t->handle, INFINITE));
        CloseHandle(t->handle);
        t->handle = NULL;
        /* The trampoline already freed fd; null the pointer so that
         * peko_thread_kill cannot dereference freed memory. */
        t->func_data = NULL;
        atomic_store(&t->detached, 1);
    }
#else
    int rc = pthread_create(&t->handle, NULL, peko_thread_trampoline, fd);
    if (rc != 0) {
        free(fd);
        free(t);
        return NULL;
    }

    if (synchronous) {
        PGC_BLOCKING(pthread_join(t->handle, NULL));
        /* The trampoline already freed fd; null the pointer so that
         * peko_thread_kill cannot dereference freed memory. */
        t->func_data = NULL;
        atomic_store(&t->detached, 1);
    } else {
        pthread_detach(t->handle);
        atomic_store(&t->detached, 1);
    }
#endif

    return t;
}

void peko_thread_kill(peko_thread_t *t)
{
    if (!t)
        return;

    /* func_data may already be freed by the trampoline (e.g. after a
     * synchronous join). Only touch it if it is still non-NULL, which
     * peko_thread_create guarantees only for non-joined threads. */
    if (t->func_data) {
        if (t->func_data->handle != PGC_NULL_HANDLE) {
            pgc_handle_release(t->func_data->handle);
            t->func_data->handle = PGC_NULL_HANDLE;
        }
        free(t->func_data);
        t->func_data = NULL;
    }

#ifdef _WIN32
    if (t->handle) {
        TerminateThread(t->handle, 0);
        CloseHandle(t->handle);
        t->handle = NULL;
    }
#else
    /* pthread_cancel is documented as unsafe. Documented in the Peko API.
     * Users should prefer CancelToken for safe cooperative cancellation.
     * Android's Bionic libc does not implement pthread_cancel so we use
     * pthread_kill with SIGUSR2 as a best-effort fallback. */
#ifdef __ANDROID__
    pthread_kill(t->handle, SIGUSR2);
#else
    pthread_cancel(t->handle);
#endif
#endif
}

void peko_thread_free(peko_thread_t *t)
{
    if (!t)
        return;
#ifdef _WIN32
    if (t->handle && !atomic_load(&t->detached)) {
        PGC_BLOCKING(WaitForSingleObject(t->handle, INFINITE));
        CloseHandle(t->handle);
    }
#endif
    free(t);
}

/* =========================================================================
 * Cancellation token
 * ====================================================================== */

peko_cancel_token_t *peko_cancel_token_new(void)
{
    peko_cancel_token_t *t =
        (peko_cancel_token_t *)malloc(sizeof(peko_cancel_token_t));
    if (!t)
        return NULL;

    atomic_store(&t->cancelled, 0);
    t->on_cancel        = NULL;
    t->on_cancel_handle = PGC_NULL_HANDLE;
    peko_mutex_init(&t->lock);
    return t;
}

void peko_cancel_token_free(peko_cancel_token_t *t)
{
    if (!t)
        return;
    /* Release the callback handle if the token was never cancelled.
     * Without this, on_cancel() + free() without cancel() leaks the
     * closure handle, keeping the closure context alive permanently. */
    if (t->on_cancel_handle != PGC_NULL_HANDLE) {
        pgc_handle_release(t->on_cancel_handle);
        t->on_cancel_handle = PGC_NULL_HANDLE;
    }
    peko_mutex_destroy(&t->lock);
    free(t);
}

bool peko_cancel_token_is_cancelled(peko_cancel_token_t *t)
{
    return atomic_load(&t->cancelled) != 0;
}

void peko_cancel_token_cancel(peko_cancel_token_t *t)
{
    if (!t)
        return;

    PGC_BLOCKING(peko_mutex_lock(&t->lock));
    int was            = atomic_exchange(&t->cancelled, 1);
    void (*cb)(void *) = t->on_cancel;
    pgc_handle h       = t->on_cancel_handle;
    /* Clear under the lock so a concurrent on_cancel() call cannot
     * see the stale handle and double-release it. */
    t->on_cancel_handle = PGC_NULL_HANDLE;
    t->on_cancel        = NULL;
    peko_mutex_unlock(&t->lock);

    /* Fire the callback exactly once outside the lock. */
    if (!was && cb) {
        void *ctx = pgc_handle_get(h);
        pgc_handle_release(h);
        cb(ctx);
    }
}

void peko_cancel_token_on_cancel(peko_cancel_token_t *t,
                                  void (*cb)(void *), void *cb_data)
{
    if (!t)
        return;

    /* Create handle immediately while cb_data is a fresh managed pointer
     * and the calling thread is attached to the GC. */
    pgc_handle h = pgc_handle_create(cb_data);

    PGC_BLOCKING(peko_mutex_lock(&t->lock));

    if (atomic_load(&t->cancelled)) {
        /* Already cancelled. Do not store the handle, so a concurrent
         * cancel() cannot also see and release it. Fire the callback once
         * outside the lock and release the handle here. */
        peko_mutex_unlock(&t->lock);
        void *immediate_ctx = pgc_handle_get(h);
        pgc_handle_release(h);
        if (cb)
            cb(immediate_ctx);
        return;
    }

    /* Release any previously registered handle before overwriting.
     * Without this, repeated on_cancel() calls on the same token leak
     * one GC handle per call, keeping closures alive indefinitely. */
    if (t->on_cancel_handle != PGC_NULL_HANDLE) {
        pgc_handle_release(t->on_cancel_handle);
        t->on_cancel_handle = PGC_NULL_HANDLE;
    }
    t->on_cancel        = cb;
    t->on_cancel_handle = h;
    peko_mutex_unlock(&t->lock);
}

/* =========================================================================
 * Future
 * ====================================================================== */

peko_future_t *peko_future_new(void)
{
    peko_future_t *f = (peko_future_t *)malloc(sizeof(peko_future_t));
    if (!f)
        return NULL;

    f->result = NULL;
    atomic_store(&f->complete,  0);
    atomic_store(&f->cancelled, 0);
    peko_mutex_init(&f->lock);
    return f;
}

void peko_future_free(peko_future_t *f)
{
    if (!f)
        return;
    /* If the future completed but was never awaited, the result handle
     * is still stored in f->result. Release it to avoid leaking the
     * GC handle and keeping the result object alive permanently. */
    if (atomic_load(&f->complete) && f->result != NULL) {
        pgc_handle h = (pgc_handle)(uintptr_t)f->result;
        f->result = NULL;
        pgc_handle_release(h);
    }
    peko_mutex_destroy(&f->lock);
    free(f);
}

void peko_future_set_result(peko_future_t *f, void *result)
{
    if (!f)
        return;

    /* Pin the result via a GC handle so it survives any collection that
     * occurs between set_result and await. Store the handle integer as
     * a void* (via uintptr_t); await unwraps it. This is safe because
     * pgc_handle is an unsigned int and sizeof(void*) >= sizeof(uint). */
    pgc_handle h = pgc_handle_create(result);
    PGC_BLOCKING(peko_mutex_lock(&f->lock));
    f->result = (void *)(uintptr_t)h;
    atomic_store(&f->complete, 1);
    peko_cond_broadcast(&f->lock);
    peko_mutex_unlock(&f->lock);
}

void *peko_future_await(peko_future_t *f)
{
    if (!f)
        return NULL;

    /* Bracket only the cond_wait, not the lock acquisition. This way the
     * GC sees this thread as blocked exactly while it is parked, and as
     * running while it holds the lock and checks state. Ending blocking
     * before the wait loop and beginning it again inside would mismatch the
     * begin/end pairing if pgc_begin/end_blocking use a reference count. */
    pgc_begin_blocking();
    peko_mutex_lock(&f->lock);
    pgc_end_blocking();
    while (!atomic_load(&f->complete) && !atomic_load(&f->cancelled)) {
        pgc_begin_blocking();
        peko_cond_wait(&f->lock, -1);
        pgc_end_blocking();
    }
    /* Unwrap the handle stored by peko_future_set_result. */
    void *result = NULL;
    if (atomic_load(&f->complete)) {
        pgc_handle h = (pgc_handle)(uintptr_t)f->result;
        f->result = NULL;
        peko_mutex_unlock(&f->lock);
        result = pgc_handle_get(h);
        pgc_handle_release(h);
    } else {
        peko_mutex_unlock(&f->lock);
    }
    return result;
}

void *peko_future_await_timeout(peko_future_t *f, int timeout_ms,
                                 bool *out_timed_out)
{
    if (out_timed_out)
        *out_timed_out = false;

    if (!f)
        return NULL;

    pgc_begin_blocking();
    peko_mutex_lock(&f->lock);
    pgc_end_blocking();
    if (!atomic_load(&f->complete) && !atomic_load(&f->cancelled)) {
        pgc_begin_blocking();
        bool signalled = peko_cond_wait(&f->lock, timeout_ms);
        pgc_end_blocking();
        if (!signalled && out_timed_out)
            *out_timed_out = true;
    }
    void *result = NULL;
    if (atomic_load(&f->complete)) {
        pgc_handle h = (pgc_handle)(uintptr_t)f->result;
        f->result = NULL;
        peko_mutex_unlock(&f->lock);
        result = pgc_handle_get(h);
        pgc_handle_release(h);
    } else {
        peko_mutex_unlock(&f->lock);
    }
    return result;
}

bool peko_future_is_complete(peko_future_t *f)
{
    return f && atomic_load(&f->complete) != 0;
}

void peko_future_cancel(peko_future_t *f)
{
    if (!f)
        return;

    PGC_BLOCKING(peko_mutex_lock(&f->lock));
    atomic_store(&f->cancelled, 1);
    peko_cond_broadcast(&f->lock);
    peko_mutex_unlock(&f->lock);
}

/* =========================================================================
 * Channel
 * ====================================================================== */

peko_channel_t *peko_channel_new(int buffer_size)
{
    peko_channel_t *c = (peko_channel_t *)malloc(sizeof(peko_channel_t));
    if (!c)
        return NULL;

    c->capacity = buffer_size;
    c->head     = 0;
    c->tail     = 0;
    c->count    = 0;
    atomic_store(&c->closed, 0);
    peko_mutex_init(&c->lock);

    if (buffer_size > 0) {
        c->buf = (pgc_handle *)malloc(sizeof(pgc_handle) * (size_t)buffer_size);
        if (!c->buf) {
            peko_mutex_destroy(&c->lock);
            free(c);
            return NULL;
        }
        for (int i = 0; i < buffer_size; i++)
            c->buf[i] = PGC_NULL_HANDLE;
    } else {
        c->buf = NULL; /* rendezvous - no buffer */
    }

    return c;
}

void peko_channel_free(peko_channel_t *c)
{
    if (!c)
        return;
    /* Release all GC handles still in the buffer. Without this, items
     * queued in a channel that is freed before being fully drained leak
     * one handle per item, keeping those objects alive permanently. */
    if (c->buf && c->capacity > 0) {
        for (int i = 0; i < c->capacity; i++) {
            if (c->buf[i] != PGC_NULL_HANDLE) {
                pgc_handle_release(c->buf[i]);
                c->buf[i] = PGC_NULL_HANDLE;
            }
        }
    } else if (c->buf && c->capacity == 0 && c->count > 0) {
        /* Rendezvous channel: buf is malloc'd per-send, count==1 means
         * a sender is blocked mid-rendezvous. Release its handle. */
        if (c->buf[0] != PGC_NULL_HANDLE)
            pgc_handle_release(c->buf[0]);
    }
    peko_mutex_destroy(&c->lock);
    free(c->buf);
    free(c);
}

bool peko_channel_send(peko_channel_t *c, void *item)
{
    if (!c || atomic_load(&c->closed))
        return false;

    /* Create the handle while still in managed state (not parked).
     * pgc_handle_create is a GC operation and must not be called
     * while pgc_begin_blocking is active. */
    pgc_handle h = pgc_handle_create(item);

    pgc_begin_blocking();
    peko_mutex_lock(&c->lock);
    pgc_end_blocking();

    if (c->capacity == 0) {
        /* Rendezvous: wait for space. Bracket only the cond_wait. */
        while (c->count > 0 && !atomic_load(&c->closed)) {
            pgc_begin_blocking();
            peko_cond_wait(&c->lock, -1);
            pgc_end_blocking();
        }

        if (atomic_load(&c->closed)) {
            peko_mutex_unlock(&c->lock);
            pgc_handle_release(h);
            return false;
        }

        c->buf = (pgc_handle *)malloc(sizeof(pgc_handle));
        if (!c->buf) {
            peko_mutex_unlock(&c->lock);
            pgc_handle_release(h);
            return false;
        }
        c->buf[0] = h;
        c->count  = 1;
        peko_cond_broadcast(&c->lock);

        /* Wait until receiver consumes it. */
        while (c->count > 0 && !atomic_load(&c->closed)) {
            pgc_begin_blocking();
            peko_cond_wait(&c->lock, -1);
            pgc_end_blocking();
        }

        free(c->buf);
        c->buf = NULL;
        peko_mutex_unlock(&c->lock);
        return true;
    }

    /* Buffered: wait until there is space. */
    while (c->count >= c->capacity && !atomic_load(&c->closed)) {
        pgc_begin_blocking();
        peko_cond_wait(&c->lock, -1);
        pgc_end_blocking();
    }

    if (atomic_load(&c->closed)) {
        peko_mutex_unlock(&c->lock);
        pgc_handle_release(h);
        return false;
    }

    c->buf[c->tail] = h;
    c->tail = (c->tail + 1) % c->capacity;
    c->count++;
    peko_cond_broadcast(&c->lock);
    peko_mutex_unlock(&c->lock);
    return true;
}

bool peko_channel_try_send(peko_channel_t *c, void *item)
{
    if (!c || atomic_load(&c->closed))
        return false;

    /* Create handle before lock - pgc_handle_create must not be called
     * while the mutex is held (another thread may be in pgc_begin_blocking
     * waiting on this lock, so we must not do GC ops under it). */
    pgc_handle h = pgc_handle_create(item);

    PGC_BLOCKING(peko_mutex_lock(&c->lock));

    if (c->capacity == 0 || c->count >= c->capacity ||
        atomic_load(&c->closed)) {
        peko_mutex_unlock(&c->lock);
        pgc_handle_release(h);
        return false;
    }

    c->buf[c->tail] = h;
    c->tail = (c->tail + 1) % c->capacity;
    c->count++;
    peko_cond_broadcast(&c->lock);
    peko_mutex_unlock(&c->lock);
    return true;
}

void *peko_channel_recv(peko_channel_t *c, bool *out_ok)
{
    if (out_ok) *out_ok = false;
    if (!c) return NULL;

    pgc_begin_blocking();
    peko_mutex_lock(&c->lock);
    pgc_end_blocking();

    if (c->capacity == 0) {
        /* Rendezvous: wait for sender. Bracket only the cond_wait so the
         * thread is truly quiescent (parked) only while blocked, not while
         * executing dequeue logic. */
        while (c->count == 0 && !atomic_load(&c->closed)) {
            pgc_begin_blocking();
            peko_cond_wait(&c->lock, -1);
            pgc_end_blocking();
        }

        if (c->count == 0) {
            peko_mutex_unlock(&c->lock);
            return NULL;
        }

        pgc_handle h = c->buf ? c->buf[0] : PGC_NULL_HANDLE;
        c->count = 0;
        peko_cond_broadcast(&c->lock);
        peko_mutex_unlock(&c->lock);
        void *item = pgc_handle_get(h);
        pgc_handle_release(h);
        if (out_ok) *out_ok = (h != PGC_NULL_HANDLE);
        return item;
    }

    /* Buffered: wait until there is an item. */
    while (c->count == 0 && !atomic_load(&c->closed)) {
        pgc_begin_blocking();
        peko_cond_wait(&c->lock, -1);
        pgc_end_blocking();
    }

    if (c->count == 0) {
        peko_mutex_unlock(&c->lock);
        return NULL;
    }

    pgc_handle h = c->buf[c->head];
    c->buf[c->head] = PGC_NULL_HANDLE;
    c->head = (c->head + 1) % c->capacity;
    c->count--;
    peko_cond_broadcast(&c->lock); /* wake senders */
    peko_mutex_unlock(&c->lock);
    if (out_ok) *out_ok = true;
    void *item = pgc_handle_get(h);
    pgc_handle_release(h);
    return item;
}

void *peko_channel_try_recv(peko_channel_t *c, bool *out_ok)
{
    if (out_ok) *out_ok = false;
    if (!c) return NULL;

    PGC_BLOCKING(peko_mutex_lock(&c->lock));

    if (c->count == 0) {
        peko_mutex_unlock(&c->lock);
        return NULL;
    }

    void *item;
    if (c->capacity == 0) {
        pgc_handle h0 = c->buf ? c->buf[0] : PGC_NULL_HANDLE;
        c->count = 0;
        if (c->buf) { free(c->buf); c->buf = NULL; }
        peko_cond_broadcast(&c->lock);
        peko_mutex_unlock(&c->lock);
        if (out_ok) *out_ok = (h0 != PGC_NULL_HANDLE);
        item = pgc_handle_get(h0);
        pgc_handle_release(h0);
        return item;
    } else {
        pgc_handle h = c->buf[c->head];
        c->buf[c->head] = PGC_NULL_HANDLE;
        c->head = (c->head + 1) % c->capacity;
        c->count--;
        peko_cond_broadcast(&c->lock);
        peko_mutex_unlock(&c->lock);
        if (out_ok) *out_ok = (h != PGC_NULL_HANDLE);
        item = pgc_handle_get(h);
        pgc_handle_release(h);
        return item;
    }
}

void peko_channel_close(peko_channel_t *c)
{
    if (!c) return;
    PGC_BLOCKING(peko_mutex_lock(&c->lock));
    atomic_store(&c->closed, 1);
    peko_cond_broadcast(&c->lock); /* wake all waiters so they can exit */
    peko_mutex_unlock(&c->lock);
}

bool peko_channel_is_closed(peko_channel_t *c)
{
    return c && atomic_load(&c->closed) != 0;
}

/* -------------------------------------------------------------------------
 * Out-parameter-free wrappers. The success flag stays a C stack local so it
 * is stable across the blocking call, rather than a GC buffer that could move
 * while the thread is parked. A managed channel item is never NULL, so a NULL
 * return distinguishes a closed or empty channel.
 * ---------------------------------------------------------------------- */

void *peko_channel_recv_value(peko_channel_t *c)
{
    bool ok = false;
    void *result = peko_channel_recv(c, &ok);
    return ok ? result : NULL;
}

void *peko_channel_try_recv_value(peko_channel_t *c)
{
    bool ok = false;
    void *result = peko_channel_try_recv(c, &ok);
    return ok ? result : NULL;
}

void *peko_future_await_timeout_value(peko_future_t *f, int timeout_ms)
{
    bool timed_out = false;
    void *result = peko_future_await_timeout(f, timeout_ms, &timed_out);
    return timed_out ? NULL : result;
}
