/*
 * threads.c
 * Thread registry and stop-the-world coordination.
 *
 * Every thread that touches managed memory attaches (recording its stack base
 * and a TLAB) and detaches when done. To collect, the collector raises the
 * pgc_collection_requested flag and waits until every other attached thread is
 * parked at a safepoint; compiler-inserted polls call pgc_enter_safepoint,
 * which captures the thread's frame pointer and blocks until the collection
 * finishes. Because the collector only proceeds once all mutators are parked,
 * the marking, compaction, and pointer-update passes run with the heap and all
 * thread stacks quiescent.
 *
 * Cross-platform: pthread + a thread-local on Unix, Win32 threads + TLS on
 * Windows. The collector owns its own lock and condition variables (in g_gc)
 * so it never depends on an external threading library.
 */

#include "./include/pgc_internal.h"

#include <string.h>
#include <stdio.h>

/* The flag the compiler-inlined safepoint poll reads. Non-zero requests that
 * threads park at their next poll. Defined here; declared in pgc.h. Plain int,
 * read without synchronization on the hot path (a stale read merely delays a
 * thread by one poll); the real synchronization is in pgc_enter_safepoint. */
volatile int pgc_collection_requested = 0;

/* =========================================================================
 * Thread-local pointer to the calling thread's registry entry
 * ====================================================================== */

#ifdef _WIN32
static DWORD g_tls_index = TLS_OUT_OF_INDEXES;

static void pgc_tls_init(void)
{
    if (g_tls_index == TLS_OUT_OF_INDEXES)
        g_tls_index = TlsAlloc();
}
static void pgc_tls_set(pgc_thread *t) { TlsSetValue(g_tls_index, t); }
static pgc_thread *pgc_tls_get(void)
{
    if (g_tls_index == TLS_OUT_OF_INDEXES)
        return NULL;
    return (pgc_thread *)TlsGetValue(g_tls_index);
}
#else
static pthread_key_t g_tls_key;
static pthread_once_t g_tls_once = PTHREAD_ONCE_INIT;

static void pgc_tls_make_key(void) { pthread_key_create(&g_tls_key, NULL); }
static void pgc_tls_init(void)     { pthread_once(&g_tls_once, pgc_tls_make_key); }
static void pgc_tls_set(pgc_thread *t) { pthread_setspecific(g_tls_key, t); }
static pgc_thread *pgc_tls_get(void)
{
    return (pgc_thread *)pthread_getspecific(g_tls_key);
}
#endif

pgc_thread *pgc_current_thread(void)
{
    return pgc_tls_get();
}

/* =========================================================================
 * Condition-variable helpers over g_gc's lock
 *
 * The collector's lock and the two condition variables live in g_gc. These
 * thin wrappers keep the platform branching in one place. The caller must
 * hold g_gc.lock around wait/signal, as usual for condition variables.
 * ====================================================================== */

#ifdef _WIN32
static void pgc_cond_wait_resume(void)
{
    SleepConditionVariableCS(&g_gc.resume, &g_gc.lock, INFINITE);
}
static void pgc_cond_wait_parked(void)
{
    SleepConditionVariableCS(&g_gc.parked_cv, &g_gc.lock, INFINITE);
}
static void pgc_cond_wake_resume(void)   { WakeAllConditionVariable(&g_gc.resume); }
static void pgc_cond_wake_parked(void)   { WakeAllConditionVariable(&g_gc.parked_cv); }
#else
static void pgc_cond_wait_resume(void)
{
    pthread_cond_wait(&g_gc.resume, &g_gc.lock);
}
static void pgc_cond_wait_parked(void)
{
    pthread_cond_wait(&g_gc.parked_cv, &g_gc.lock);
}
static void pgc_cond_wake_resume(void)   { pthread_cond_broadcast(&g_gc.resume); }
static void pgc_cond_wake_parked(void)   { pthread_cond_broadcast(&g_gc.parked_cv); }
#endif

/* =========================================================================
 * Attach / detach
 * ====================================================================== */

#ifdef _WIN32
static DWORD pgc_self_id(void) { return GetCurrentThreadId(); }
#else
static pthread_t pgc_self_handle(void) { return pthread_self(); }
#endif

void pgc_thread_attach(void)
{
    pgc_tls_init();

    /* Already attached on this thread: nothing to do. */
    if (pgc_tls_get() != NULL)
        return;

    pgc_lock();

    /* Find a free registry slot. */
    pgc_thread *entry = NULL;
    for (int i = 0; i < PGC_MAX_THREADS; i++) {
        if (!g_gc.threads[i].in_use) {
            entry = &g_gc.threads[i];
            if (i >= g_gc.thread_count)
                g_gc.thread_count = i + 1;
            break;
        }
    }

    if (entry == NULL) {
        /* Registry full: cannot safely run managed code on this thread. */
        pgc_unlock();
        return;
    }

    memset(entry, 0, sizeof(*entry));
    entry->in_use = true;

    /* Capture the stack base: the frame address of the attach point is a
     * conservative high-water mark for this thread's stack (stacks grow down
     * on the supported targets, so the base is the highest address the walk
     * should reach). */
    entry->stack_base = __builtin_frame_address(0);
    entry->stack_top  = NULL;
    atomic_store(&entry->parked, 0);

#ifdef _WIN32
    entry->os_id = pgc_self_id();
#else
    entry->os_handle = pgc_self_handle();
#endif

    pgc_tls_set(entry);

    pgc_unlock();
}

void pgc_thread_detach(void)
{
    pgc_thread *entry = pgc_tls_get();
    if (entry == NULL)
        return;

    pgc_lock();
    /* Fill this thread's unused TLAB tail with a filler object BEFORE marking
     * the slot free. Once in_use is false, pgc_fill_all_tlab_gaps (which only
     * visits in_use threads) will never fill it, so the tail would otherwise
     * become a PERMANENT unfilled gap in the heap -- a hole the collector's
     * linear compaction walks (compute_forwarding / update_references /
     * move_objects) cannot step past, desyncing the walk and leaving every
     * object after the gap unrelocated (stale references -> use-after-move
     * corruption). Threads that come and go (e.g. per-connection socket
     * threads) detach frequently, so these gaps accumulate; filling here keeps
     * the heap densely walkable. The fill is a no-op for a thread that never
     * acquired a TLAB (top/end NULL). */
    pgc_fill_gap(entry->tlab.top, entry->tlab.end);
    entry->tlab.top  = NULL;
    entry->tlab.end  = NULL;
    entry->in_use    = false;
    entry->stack_top = NULL;
    /* Leave thread_count as a high-water mark; freed slots are reused by
     * attach. Compacting thread_count is unnecessary and would race with
     * indices other code may hold. */
    pgc_unlock();

    pgc_tls_set(NULL);
}

/* =========================================================================
 * Safepoint parking
 *
 * Called from the compiler-inlined poll when pgc_collection_requested is set.
 * Records the thread's frame pointer (so the collector can walk its stack),
 * marks itself parked, wakes the collector in case it is waiting for the last
 * straggler, and blocks until the collection clears the flag.
 * ====================================================================== */

void pgc_enter_safepoint(void)
{
    pgc_thread *self = pgc_tls_get();
    if (self == NULL)
        return;  /* unattached thread: no roots to scan, nothing to park */

#ifdef _WIN32
    /* Capture the register context at the park point. The Win64 unwinder walks
     * from here through this frame into the managed caller that polled. */
    CONTEXT ctx;
    RtlCaptureContext(&ctx);
#else
    /* Capture the frame pointer at the park point. The stack-root walk starts
     * here and chains up through the caller (the Peko function that polled)
     * via the saved-frame-pointer links. */
    void *fp = __builtin_frame_address(0);
#endif

    pgc_lock();

#ifdef _WIN32
    self->win_context = ctx;
    self->stack_top   = (void *)ctx.Rsp;  /* non-null marks a captured frame */
#else
    self->stack_top   = fp;
#endif
    /* Parked at a poll, not in a blocking call. The captured frame carries the
     * managed caller's safepoint, so no separate blocking capture applies. */
    self->blocking_ret_addr = NULL;
    atomic_store(&self->parked, 1);

    /* Wake the collector: it may be waiting for all mutators to park. */
    pgc_cond_wake_parked();

    /* Block until the collection finishes (the flag is cleared). */
    while (pgc_collection_requested != 0)
        pgc_cond_wait_resume();

    atomic_store(&self->parked, 0);
    self->stack_top = NULL;

    pgc_unlock();
}

/* =========================================================================
 * Stop-the-world / start-the-world
 *
 * pgc_stop_the_world is called by the collecting thread with g_gc.lock NOT
 * held (it takes the lock itself). It raises the flag and waits until every
 * other attached thread is parked. The collecting thread is itself at a
 * safepoint (it called in from an allocation or explicit collect), so it does
 * not count itself among the threads that must park.
 * ====================================================================== */

void pgc_stop_the_world(void)
{
    pgc_thread *self = pgc_current_thread();

    pgc_lock();

    pgc_collection_requested = 1;

    /* Wait until every attached thread other than this one is parked. */
    int stall_count = 0;
    for (;;) {
        bool all_parked = true;
        int  unparked_idx = -1;
        for (int i = 0; i < g_gc.thread_count; i++) {
            pgc_thread *t = &g_gc.threads[i];
            if (!t->in_use || t == self)
                continue;
            if (atomic_load(&t->parked) == 0) {
                all_parked = false;
                unparked_idx = i;
                break;
            }
        }
        if (all_parked)
            break;

        stall_count++;
        if (stall_count == 1000) {
            for (int i = 0; i < g_gc.thread_count; i++) {
                pgc_thread *t = &g_gc.threads[i];
            }
            stall_count = 0;
        }
        pgc_cond_wait_parked();
    }

    pgc_unlock();
}

void pgc_start_the_world(void)
{
    pgc_lock();

    pgc_collection_requested = 0;

    /* Clear the collecting thread's captured stack pointer. */
    pgc_thread *self = pgc_current_thread();
    if (self != NULL)
        self->stack_top = NULL;

    /* Wake every parked thread so it can resume. */
    pgc_cond_wake_resume();

    pgc_unlock();
}

/* =========================================================================
 * Blocking-region transitions
 *
 * pgc_begin_blocking marks the calling thread parked before it blocks in a
 * native call, so a collection can proceed without waiting on a thread that
 * cannot reach a safepoint. The thread captures its frame pointer (so its
 * stack is still scannable for any roots live across the blocking call) and
 * sets parked, then returns to make the blocking call.
 *
 * pgc_end_blocking is called on return. If a collection is in progress it
 * waits for it to finish (the thread must not resume mutating mid-collection),
 * then clears parked. This mirrors pgc_enter_safepoint's wait, but is driven
 * explicitly around a native blocking call rather than by the poll flag.
 * ====================================================================== */

void pgc_begin_blocking(void)
{
    pgc_thread *self = pgc_tls_get();
    if (self == NULL)
        return;

#ifdef _WIN32
    /* This frame does not survive the return into the blocking call, so the
     * managed caller's context is captured now. RtlCaptureContext records this
     * frame; one unwind step yields the caller's context, whose frame stays
     * live for the whole blocking call. The walk seeds from that context, so
     * its program counter must be a recorded return address, which holds when
     * the call into this function is a safepoint. */
    CONTEXT ctx;
    RtlCaptureContext(&ctx);
    {
        DWORD64 image_base = 0;
        PRUNTIME_FUNCTION fn =
            RtlLookupFunctionEntry(ctx.Rip, &image_base, NULL);
        if (fn != NULL) {
            PVOID handler_data = NULL;
            DWORD64 establisher = 0;
            RtlVirtualUnwind(UNW_FLAG_NHANDLER, image_base, ctx.Rip, fn,
                             &ctx, &handler_data, &establisher, NULL);
        }
    }

    pgc_lock();
    self->win_context = ctx;
    self->stack_top   = (void *)ctx.Rsp;  /* non-null marks a captured frame */
    atomic_store(&self->parked, 1);
    /* A collector waiting for all threads to park may now proceed. */
    pgc_cond_wake_parked();
    pgc_unlock();
#else
    /* The managed caller's frame stays live for the whole blocking call, but
     * this frame does not survive the return into that caller. Capture the
     * caller's safepoint now: the return address into the caller, the caller's
     * stack pointer at the call site (one frame record above this frame
     * pointer), and the caller's frame pointer for the rest of the walk. The
     * collector reads these to scan the caller's roots while the thread is
     * blocked. */
    void *fp             = __builtin_frame_address(0);
    void *ret_to_caller  = __builtin_return_address(0);
    void *caller_fp      = *(void **)fp;
    void *caller_sp      = (void *)((unsigned char *)fp + PGC_FRAME_RECORD_BYTES);

    pgc_lock();
    self->blocking_ret_addr  = ret_to_caller;
    self->blocking_caller_sp = caller_sp;
    self->stack_top          = caller_fp;
    atomic_store(&self->parked, 1);
    /* A collector waiting for all threads to park may now proceed. */
    pgc_cond_wake_parked();
    pgc_unlock();
#endif
}

void pgc_end_blocking(void)
{
    pgc_thread *self = pgc_tls_get();
    if (self == NULL)
        return;

    pgc_lock();
    /* Do not resume managed execution while a collection is running. */
    while (pgc_collection_requested != 0)
        pgc_cond_wait_resume();
    atomic_store(&self->parked, 0);
    self->stack_top          = NULL;
    self->blocking_ret_addr  = NULL;
    self->blocking_caller_sp = NULL;
    pgc_unlock();
}

/* =========================================================================
 * TLAB reset
 *
 * After compaction every TLAB points into stale heap space (objects slid down
 * and the bump top moved), so every thread's TLAB is emptied. Threads grab a
 * fresh TLAB from the compacted heap on their next allocation. Called by the
 * collection driver while the world is stopped.
 * ====================================================================== */

void pgc_reset_all_tlabs(void)
{
    for (int i = 0; i < g_gc.thread_count; i++) {
        if (g_gc.threads[i].in_use) {
            g_gc.threads[i].tlab.top = NULL;
            g_gc.threads[i].tlab.end = NULL;
        }
    }
}

/* =========================================================================
 * Fill every current TLAB's unused tail with a filler object.
 *
 * At collection time each attached thread holds a TLAB whose [top, end) tail is
 * reserved heap space that contains no objects -- a gap that would derail the
 * collector's heap walks. Called by the collection driver immediately after the
 * world is stopped (so reading each thread's tlab is race-free) and before any
 * heap walk. After filling, the heap is densely walkable base..top. The TLABs
 * themselves are reset later (pgc_reset_all_tlabs) once compaction is done; the
 * fillers are unmarked, hence treated as dead and reclaimed by the move pass.
 * ====================================================================== */

void pgc_fill_all_tlab_gaps(void)
{
    for (int i = 0; i < g_gc.thread_count; i++) {
        if (g_gc.threads[i].in_use) {
            pgc_fill_gap(g_gc.threads[i].tlab.top, g_gc.threads[i].tlab.end);
            /* The tail is now a filler object, no longer free TLAB space. Clear
             * the TLAB bounds so a thread resuming mid-allocation cannot bump
             * into the space we just turned into an object; it will take the
             * slow path and refill from the compacted heap. */
            g_gc.threads[i].tlab.top = NULL;
            g_gc.threads[i].tlab.end = NULL;
        }
    }
}
