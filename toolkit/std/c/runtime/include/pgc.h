/*
 * pgc.h
 * Public interface to the Peko garbage collector.
 *
 * This header is the surface that embedders and foreign (C) code include.
 * It exposes lifecycle, allocation, root registration, FFI handles, object
 * pinning, and thread participation. It deliberately keeps the collector's
 * internal data structures opaque; those live in pgc_internal.h and are
 * private to the runtime's own translation units.
 *
 * The collector is a stop-the-world, sliding mark-compact (Lisp2) collector.
 * Managed objects may move during a collection. Any raw pointer into the
 * managed heap is therefore only valid until the next collection unless the
 * object is pinned or referenced through a handle.
 */

#ifndef PGC_H
#define PGC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* -------------------------------------------------------------------------
 * Lifecycle
 * ---------------------------------------------------------------------- */

/*
 * Initialize the collector. Must be called once, before any allocation or
 * thread attaches, on the thread that will act as the initial mutator.
 * heap_bytes is the total managed heap size; 0 selects a default. The heap
 * is split internally as the collector sees fit. Returns 1 on success, 0 on
 * failure (for example, the backing memory could not be reserved).
 */
int pgc_init(size_t heap_bytes);

/*
 * Tear down the collector and release the managed heap. After this call no
 * managed pointer remains valid. Intended for clean process shutdown and
 * tests; not required before exit.
 */
void pgc_shutdown(void);

/* -------------------------------------------------------------------------
 * Allocation
 *
 * pgc_alloc_managed is the descriptor-carrying allocation used for class
 * instances, closures, arrays, and any object the collector must trace. The
 * descriptor describes which fields are managed pointers (see the descriptor
 * formats in pgc_internal.h). size is the payload size in bytes; the runtime
 * prepends the object header and returns a pointer to the payload.
 *
 * pgc_alloc_atomic allocates managed memory with no internal managed
 * pointers (strings, byte buffers, primitive arrays). The collector relocates
 * it but never scans it for children. This is the path foreign code uses to
 * hand back GC-managed buffers to Pekoscript.
 * ---------------------------------------------------------------------- */

void *pgc_alloc_managed(const void *descriptor, size_t size);
void *pgc_alloc_atomic(size_t size);

/* -------------------------------------------------------------------------
 * Global roots
 *
 * A global root is the address of a storage location (a slot) that holds a
 * managed pointer and lives outside the managed heap (a global variable, for
 * instance). The collector reads the slot to find the root and writes it back
 * if the target moves. Registering the slot address (not the value) is what
 * lets a moving collector update the global after compaction.
 * ---------------------------------------------------------------------- */

void pgc_add_root(void **slot);
void pgc_remove_root(void **slot);

/* -------------------------------------------------------------------------
 * Write barrier
 *
 * Records that a managed pointer was stored into a slot inside the managed
 * heap. Both arguments are raw (address-space-0) pointers; the compiler casts
 * the managed slot and value down before the call. The barrier exists so the
 * collector can maintain its bookkeeping for stores; for a basic stop-the-world
 * mark-compact it may be a no-op, but the entry point is part of the stable
 * ABI and always present.
 * ---------------------------------------------------------------------- */

void pgc_write_barrier(void *slot, void *value);

/* -------------------------------------------------------------------------
 * FFI handles
 *
 * A handle is a stable, integer-named reference to a managed object that
 * survives collections. Foreign code that needs to retain a managed object
 * across time (beyond a single call) creates a handle, holds the integer, and
 * dereferences it through pgc_handle_get whenever it needs the object's
 * current address. The collector treats live handles as roots and updates the
 * handle table when objects move, so the integer stays valid even though the
 * object's address changes.
 *
 * Handle 0 is reserved as the null handle and never refers to an object.
 * ---------------------------------------------------------------------- */

typedef uint32_t pgc_handle;

#define PGC_NULL_HANDLE ((pgc_handle)0)

pgc_handle pgc_handle_create(void *object);
void      *pgc_handle_get(pgc_handle handle);
void       pgc_handle_release(pgc_handle handle);

/* -------------------------------------------------------------------------
 * Pinning
 *
 * Pinning tells the collector not to move a specific object, so foreign code
 * can use a raw pointer to it directly (passing a buffer to a syscall, say)
 * for the duration of the pin. Pins nest: an object stays pinned until the
 * number of pgc_unpin calls matches the number of pgc_pin calls. Pinned
 * objects fragment the heap, so pins should be short-lived. pgc_pin returns
 * the object's current (and, while pinned, stable) raw address.
 * ---------------------------------------------------------------------- */

void *pgc_pin(void *object);
void  pgc_unpin(void *object);

/* -------------------------------------------------------------------------
 * Thread participation
 *
 * Every thread that touches managed memory must attach before its first
 * managed access and detach when it is done. Attaching records the thread so
 * the collector can stop it at a safepoint and scan its stack for roots.
 * pgc_thread_attach captures the caller's stack base; it must be called from
 * the thread itself, near the top of its entry function.
 * ---------------------------------------------------------------------- */

void pgc_thread_attach(void);
void pgc_thread_detach(void);

/* -------------------------------------------------------------------------
 * Blocking-region transitions
 *
 * A thread that is about to block in a native call (joining a thread, waiting
 * on a channel, a blocking syscall) cannot reach a GC safepoint while blocked,
 * which would stall a collection that needs every thread parked. Before
 * blocking, the thread calls pgc_begin_blocking to declare itself parked (it
 * holds no live managed pointers it will touch while blocked, so the collector
 * may scan its last safepoint and proceed). On return it calls
 * pgc_end_blocking, which waits out any in-progress collection before the
 * thread resumes managed execution.
 *
 * These bracket native blocking calls made from attached threads.
 * ---------------------------------------------------------------------- */

void pgc_begin_blocking(void);
void pgc_end_blocking(void);

/* -------------------------------------------------------------------------
 * Safepoint interface
 *
 * These two symbols are referenced directly by compiler-generated code. The
 * inlined gc.safepoint_poll loads pgc_collection_requested and, when it is
 * non-zero, calls pgc_enter_safepoint to park the calling thread until the
 * in-progress collection finishes. The flag is read without synchronization
 * by the poll (a stale read merely delays the thread by one poll); the real
 * synchronization happens inside pgc_enter_safepoint.
 * ---------------------------------------------------------------------- */

extern volatile int pgc_collection_requested;

void pgc_enter_safepoint(void);

/*
 * Request a collection explicitly. Normally collection is driven by
 * allocation failure, but this lets an embedder force one (useful for tests
 * and for shutdown). Blocks until the collection completes.
 */
void pgc_collect(void);

#ifdef __cplusplus
}
#endif

#endif /* PGC_H */
