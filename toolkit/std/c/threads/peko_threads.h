/*
 * peko_threads.h
 * Shared types and declarations for the Peko threads library.
 * Include this header in all threads C implementation files.
 */

#ifndef PEKO_THREADS_H
#define PEKO_THREADS_H

#include <stdbool.h>

/* New GC API - implemented in the pgc runtime linked from the runtime package. */
void pgc_thread_attach(void);
void pgc_thread_detach(void);
void pgc_begin_blocking(void);
void pgc_end_blocking(void);
void pgc_add_root(void **slot);
void pgc_remove_root(void **slot);

/* Wraps any blocking C call so the GC can proceed while this thread waits.
 * Every pthread_mutex_lock, peko_cond_wait, WaitForSingleObject, send, recv,
 * accept and similar blocking primitive called from an attached thread must
 * use this or pgc_begin_blocking/pgc_end_blocking directly. */
#define PGC_BLOCKING(expr) do { pgc_begin_blocking(); (expr); pgc_end_blocking(); } while(0)
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>

/* -------------------------------------------------------------------------
 * Platform includes
 * ---------------------------------------------------------------------- */

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <windows.h>
#else
#  include <pthread.h>
#  include <signal.h>
#  include <errno.h>
#  include <sys/time.h>
#endif

/* -------------------------------------------------------------------------
 * Peko GC interface (new pgc runtime)
 * ---------------------------------------------------------------------- */

/* Handles: stable integer references to managed objects that survive moves. */
typedef unsigned int pgc_handle;
#define PGC_NULL_HANDLE ((pgc_handle)0)
pgc_handle pgc_handle_create(void *object);
void      *pgc_handle_get(pgc_handle handle);
void       pgc_handle_release(pgc_handle handle);

/* -------------------------------------------------------------------------
 * Cross-platform mutex type
 * Wraps pthread_mutex_t on Unix and CRITICAL_SECTION on Windows.
 * ---------------------------------------------------------------------- */

typedef struct {
#ifdef _WIN32
    CRITICAL_SECTION cs;
    CONDITION_VARIABLE cv;
#else
    pthread_mutex_t mutex;
    pthread_cond_t  cond;
#endif
} peko_mutex_t;

peko_mutex_t *peko_mutex_new(void);
void peko_mutex_free(peko_mutex_t *m);
void peko_mutex_init(peko_mutex_t *m);
void peko_mutex_destroy(peko_mutex_t *m);
void peko_mutex_lock(peko_mutex_t *m);
void peko_mutex_unlock(peko_mutex_t *m);

/*
 * Waits on the condition variable associated with m.
 * The caller must hold m before calling this.
 * Returns true if the condition was signalled, false if timed out.
 * timeout_ms < 0 means wait indefinitely.
 */
bool peko_cond_wait(peko_mutex_t *m, int timeout_ms);

/* Signals one waiter on the condition variable. */
void peko_cond_signal(peko_mutex_t *m);

/* Signals all waiters on the condition variable. */
void peko_cond_broadcast(peko_mutex_t *m);

/* -------------------------------------------------------------------------
 * Thread function data
 * Allocated with malloc (not GC) so the GC cannot interfere with
 * thread bookkeeping while a thread is running.
 * ---------------------------------------------------------------------- */

typedef struct {
    void  (*worker)(void *);
    pgc_handle handle;  /* GC handle; pgc_handle_get gives current context address */
} peko_func_data_t;

/* -------------------------------------------------------------------------
 * Thread handle
 * ---------------------------------------------------------------------- */

typedef struct {
#ifdef _WIN32
    HANDLE          handle;
#else
    pthread_t       handle;
#endif
    peko_func_data_t *func_data;
    atomic_int       detached;
} peko_thread_t;

/* -------------------------------------------------------------------------
 * Thread functions
 * ---------------------------------------------------------------------- */

/*
 * Creates a new thread running worker(data).
 * Returns a malloc'd peko_thread_t. The caller owns the pointer.
 * If synchronous is true, blocks until the thread finishes.
 */
peko_thread_t *peko_thread_create(void (*worker)(void *), void *data,
                                  bool synchronous);

/*
 * Force-terminates a thread.
 * UNSAFE: may leave locks held and memory in inconsistent state.
 * Documented as unsafe in the Peko API. Prefer CancelToken instead.
 */
void peko_thread_kill(peko_thread_t *t);

/* Frees a thread handle. */
void peko_thread_free(peko_thread_t *t);

/* -------------------------------------------------------------------------
 * Cancellation token
 * ---------------------------------------------------------------------- */

typedef struct {
    atomic_int        cancelled;
    void            (*on_cancel)(void *);
    pgc_handle        on_cancel_handle;
    peko_mutex_t      lock;
} peko_cancel_token_t;

peko_cancel_token_t *peko_cancel_token_new(void);
void                 peko_cancel_token_free(peko_cancel_token_t *t);
bool                 peko_cancel_token_is_cancelled(peko_cancel_token_t *t);
void                 peko_cancel_token_cancel(peko_cancel_token_t *t);
void                 peko_cancel_token_on_cancel(peko_cancel_token_t *t,
                                                  void (*cb)(void *),
                                                  void *cb_data);

/* -------------------------------------------------------------------------
 * Future
 * ---------------------------------------------------------------------- */

typedef struct {
    void         *result;        /* GC-managed result value              */
    atomic_int    complete;      /* 1 when result is ready               */
    atomic_int    cancelled;     /* 1 if cancelled before completion     */
    peko_mutex_t  lock;          /* protects result + condition          */
} peko_future_t;

peko_future_t *peko_future_new(void);
void           peko_future_free(peko_future_t *f);
void           peko_future_set_result(peko_future_t *f, void *result);

/*
 * Blocks until the future is complete.
 * Returns the result pointer, or NULL if cancelled.
 */
void          *peko_future_await(peko_future_t *f);

/*
 * Blocks until complete or timeout_ms elapses.
 * Returns the result pointer, or NULL on timeout or cancellation.
 * out_timed_out is set to true if the call returned due to timeout.
 */
void          *peko_future_await_timeout(peko_future_t *f, int timeout_ms,
                                         bool *out_timed_out);

bool           peko_future_is_complete(peko_future_t *f);
void           peko_future_cancel(peko_future_t *f);

/* -------------------------------------------------------------------------
 * Channel
 * ---------------------------------------------------------------------- */

typedef struct {
    pgc_handle   *buf;           /* ring buffer of GC handles (survive moves) */
    int           capacity;      /* 0 = rendezvous, >0 = buffered        */
    int           head;
    int           tail;
    int           count;
    atomic_int    closed;
    peko_mutex_t  lock;          /* protects buf + not-full + not-empty  */
} peko_channel_t;

peko_channel_t *peko_channel_new(int buffer_size);
void            peko_channel_free(peko_channel_t *c);

/*
 * Sends item into the channel. Blocks if the buffer is full.
 * Returns false if the channel is closed.
 */
bool  peko_channel_send(peko_channel_t *c, void *item);

/*
 * Tries to send without blocking.
 * Returns true if the item was sent, false if full or closed.
 */
bool  peko_channel_try_send(peko_channel_t *c, void *item);

/*
 * Receives an item from the channel. Blocks if the buffer is empty.
 * Returns NULL if the channel is closed and empty.
 * out_ok is set to false if the channel is closed and empty.
 */
void *peko_channel_recv(peko_channel_t *c, bool *out_ok);

/*
 * Tries to receive without blocking.
 * Returns NULL and sets out_ok to false if empty or closed.
 */
void *peko_channel_try_recv(peko_channel_t *c, bool *out_ok);

void  peko_channel_close(peko_channel_t *c);
bool  peko_channel_is_closed(peko_channel_t *c);

/* -------------------------------------------------------------------------
 * Main thread dispatch
 * Declared here, implemented per-platform in peko_dispatch.c.
 * ---------------------------------------------------------------------- */

void peko_dispatch_main(void (*worker)(void *), void *data);

#endif /* PEKO_THREADS_H */
