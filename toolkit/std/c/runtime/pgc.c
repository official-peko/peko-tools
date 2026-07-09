/*
 * pgc.c
 * Top-level lifecycle and the collection driver.
 *
 * Defines the single global collector state, the lock primitives the other
 * modules use, init/shutdown, and pgc_collect: the driver that sequences a
 * full stop-the-world sliding mark-compact cycle.
 *
 * Collection sequence (world stopped throughout the passes):
 *   1. stop the world      - all mutators parked, stacks quiescent
 *   2. mark                - reachable objects marked from every root
 *   3. compute forwarding  - assign each live object its post-move address
 *   4. update references   - rewrite every root and field to forwarded targets
 *   5. move objects         - slide live objects to their addresses
 *   6. reset TLABs          - every thread re-acquires from the compacted heap
 *   7. start the world      - mutators resume
 *
 * Marks are cleared as part of moving (the move pass zeroes each survivor's
 * forward word), so no separate clear pass is needed after a successful cycle.
 */

#include "./include/pgc.h"
#include "./include/pgc_internal.h"

#include <string.h>
#include <stdlib.h>

/* The single global collector instance. */
pgc_state g_gc;

/* A non-recursive guard so a collection triggered from within allocation does
 * not re-enter while the world is stopped. Only the thread that wins the right
 * to collect runs the driver; others that arrive meanwhile wait at a
 * safepoint via the normal flag/poll path. */
static atomic_int g_collecting;

/* =========================================================================
 * Lock primitives
 *
 * Thin wrappers over the platform mutex in g_gc. Used by every module for
 * structural mutation of shared GC state.
 * ====================================================================== */

void pgc_lock(void)
{
#ifdef _WIN32
    EnterCriticalSection(&g_gc.lock);
#else
    pthread_mutex_lock(&g_gc.lock);
#endif
}

void pgc_unlock(void)
{
#ifdef _WIN32
    LeaveCriticalSection(&g_gc.lock);
#else
    pthread_mutex_unlock(&g_gc.lock);
#endif
}

/* =========================================================================
 * Synchronization init / teardown
 * ====================================================================== */

static void pgc_sync_init(void)
{
#ifdef _WIN32
    InitializeCriticalSection(&g_gc.lock);
    InitializeConditionVariable(&g_gc.resume);
    InitializeConditionVariable(&g_gc.parked_cv);
#else
    pthread_mutex_init(&g_gc.lock, NULL);
    pthread_cond_init(&g_gc.resume, NULL);
    pthread_cond_init(&g_gc.parked_cv, NULL);
#endif
}

static void pgc_sync_destroy(void)
{
#ifdef _WIN32
    DeleteCriticalSection(&g_gc.lock);
    /* Condition variables need no explicit destruction on Windows. */
#else
    pthread_mutex_destroy(&g_gc.lock);
    pthread_cond_destroy(&g_gc.resume);
    pthread_cond_destroy(&g_gc.parked_cv);
#endif
}

/* =========================================================================
 * Lifecycle
 * ====================================================================== */

int pgc_init(size_t heap_bytes)
{
    if (g_gc.initialized)
        return 1;

    memset(&g_gc, 0, sizeof(g_gc));

    pgc_sync_init();

    if (!pgc_heap_create(heap_bytes)) {
        pgc_sync_destroy();
        return 0;
    }

    atomic_store(&g_collecting, 0);
    pgc_collection_requested = 0;

    g_gc.initialized = true;

    /* The initializing thread is a mutator: attach it so its stack is scanned
     * and it owns a TLAB. */
    pgc_thread_attach();

    /* Parse the stack maps up front so the first collection does not pay the
     * cost (and so a missing/empty table is handled before any GC). */
    pgc_stackmap_init();

    return 1;
}

void pgc_shutdown(void)
{
    if (!g_gc.initialized)
        return;

    pgc_thread_detach();
    pgc_heap_destroy();
    pgc_sync_destroy();

    /* Release growable side tables. */
    free(g_gc.roots.slots);
    free(g_gc.handles.entries);
    free(g_gc.pins.entries);

    memset(&g_gc, 0, sizeof(g_gc));
}

/* =========================================================================
 * Collection driver
 *
 * pgc_collect runs one full mark-compact cycle. It is safe to call from any
 * attached thread and from the allocator's out-of-memory path. Only one
 * collection runs at a time: the first caller to claim g_collecting drives the
 * cycle; a concurrent caller parks at the safepoint the driver raises and
 * returns once the cycle completes.
 * ====================================================================== */
void pgc_collect(void)
{
    /* Claim the right to collect. If another thread is already collecting,
     * cooperate by entering a safepoint (which parks until that collection
     * finishes), then return: the collection the caller wanted has happened. */
    int expected = 0;
    if (!atomic_compare_exchange_strong(&g_collecting, &expected, 1)) {
        pgc_enter_safepoint();
        return;
    }

    /* 1. Stop the world: every other attached thread parks at a safepoint. */
    pgc_stop_the_world();

    /* Capture this thread's own stack anchor from a frame that stays live
     * through the mark phase. This thread drives the collection and does not
     * park, so the stack walk scans its roots starting from this frame. */
    {
        pgc_thread *self = pgc_current_thread();
        if (self != NULL) {
#ifdef _WIN32
            RtlCaptureContext(&self->win_context);
            self->stack_top = (void *)self->win_context.Rsp;
#else
            self->stack_top = __builtin_frame_address(0);
#endif
            self->blocking_ret_addr = NULL;
        }
    }

    /* 1b. Fill every thread's current TLAB tail with a filler object. TLABs
     * leave reserved-but-empty gaps in the heap; with the world stopped it is
     * safe to read each thread's TLAB and fill its tail so the subsequent heap
     * walks (index build, mark clear, forwarding, update, move) see a dense,
     * well-formed base..top layout. Fillers are unmarked and thus reclaimed. */
    pgc_fill_all_tlab_gaps();

    /* Debug: structural heap audit before mark, gated by PEKO_GC_AUDIT. */
    static int s_audit_env = -1;
    static int s_verify_env = -1;
    static unsigned long s_generation = 0;
    if (s_audit_env < 0)
        s_audit_env = getenv("PEKO_GC_AUDIT") != NULL ? 1 : 0;
    if (s_verify_env < 0)
        s_verify_env = getenv("PEKO_GC_VERIFY") != NULL ? 1 : 0;
    s_generation++;
    if (s_audit_env)
        pgc_audit();

    /* 2. Mark from all roots (global, handle, and precise stack roots). */
    pgc_mark_all();

    /* Debug: conservative mark-completeness cross-check, gated by
     * PEKO_GC_VERIFY. Runs after mark, before any object moves. */
    if (s_verify_env)
        pgc_verify_mark(s_generation);

    /* 3-5. Sliding compaction: compute new addresses, rewrite all references
     * to them, then move the objects. Order matters: references must be
     * updated while the forwarding addresses are still readable in the old
     * object headers, before the move overwrites them. */
    pgc_compute_forwarding();
    pgc_update_references();
    pgc_move_objects();

    /* The object-start index (built in pgc_mark_all) described pre-move layout
     * and is consumed by marking and update_references. Objects have now moved,
     * so it is stale; release it. The next cycle rebuilds it. */
    pgc_object_index_free();

    /* 6. Every TLAB now points into stale space; reset them so threads
     * re-acquire from the compacted heap. */
    pgc_reset_all_tlabs();

    /* 7. Resume the world. */
    pgc_start_the_world();

    atomic_store(&g_collecting, 0);
}
