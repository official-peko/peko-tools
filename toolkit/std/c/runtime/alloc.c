/*
 * alloc.c
 * Allocation: the thread-local fast path and the public allocation entry
 * points (pgc_alloc_managed, pgc_alloc_atomic).
 *
 * Each attached thread owns a TLAB (thread-local allocation buffer): a chunk
 * carved from the global heap that the thread bump-allocates from with no
 * lock. When the TLAB cannot satisfy a request, the thread locks the heap and
 * either refills the TLAB (small objects) or allocates the object directly
 * from the heap (objects too large to ever fit a TLAB). If the heap itself is
 * exhausted, a collection is triggered and the request retried once.
 *
 * The header (size, forward, descriptor) is written here at allocation. The
 * compiler is unaware of the header and only ever passes the payload size.
 */

#include "./include/pgc_internal.h"

#include <string.h>
#include <stdlib.h>
#include <stdatomic.h>

/* Debug: PEKO_GC_STRESS=N forces a collection every N allocations so races that
 * only appear under GC pressure reproduce quickly. 0 (unset) disables it. Read
 * once and cached. */
static unsigned long pgc_stress_interval(void)
{
    static int cached = -1;
    static unsigned long n = 0;
    if (cached < 0) {
        const char *s = getenv("PEKO_GC_STRESS");
        n = (s != NULL) ? strtoul(s, NULL, 10) : 0UL;
        cached = 1;
    }
    return n;
}

/* Size of a fresh TLAB chunk. Objects larger than this never use a TLAB and
 * are allocated directly from the heap under the lock. 256 KiB balances lock
 * traffic against per-thread footprint. */
#define PGC_TLAB_BYTES  ((size_t)256 * 1024)

/* Same alignment the heap enforces; allocations are rounded up to this. */
#define PGC_ALLOC_ALIGN  ((size_t)16)

static size_t pgc_align_up_alloc(size_t n)
{
    return (n + (PGC_ALLOC_ALIGN - 1)) & ~(PGC_ALLOC_ALIGN - 1);
}

/* =========================================================================
 * Header initialization
 *
 * Write the three header words and return the payload pointer. payload_size is
 * the aligned payload size (what the collector will read back to walk the
 * heap). The forward word starts at 0 (unmarked, no forwarding address).
 * ====================================================================== */

static void *pgc_init_header(unsigned char *raw, size_t payload_size,
                             const void *descriptor)
{
    pgc_header *h = (pgc_header *)raw;
    h->size       = (uintptr_t)payload_size;
    h->forward    = 0;
    h->descriptor = descriptor;

    void *payload = PGC_PAYLOAD_OF(h);

    /* Zero the payload before handing it out. The heap is reused after
     * compaction, so a fresh allocation's bytes are otherwise whatever a
     * previously-collected object left there -- frequently old managed
     * pointers and string fragments. A traced object (class instance, managed
     * array) is reachable by the collector the moment it is allocated, but its
     * managed fields are written by the caller AFTER this returns. If a
     * collection is triggered (e.g. by the very next allocation during
     * multi-field construction) before those fields are initialized, the
     * collector reads the descriptor-named offsets and would follow the stale
     * bytes as managed pointers -- dereferencing garbage as object headers.
     * Zeroing guarantees every not-yet-written managed field reads as NULL,
     * which the tracer skips. (Atomic objects do not strictly need this, but
     * a single unconditional memset keeps the allocator branch-free and is
     * cheap relative to the allocation itself.) */
    memset(payload, 0, payload_size);

    return payload;
}

/* =========================================================================
 * Slow path: refill a TLAB or allocate directly, under the heap lock.
 *
 * Returns the raw (header) pointer for a block of total_bytes, or NULL if the
 * heap is exhausted even after the caller's retry logic. For requests that fit
 * a TLAB, this refills the calling thread's TLAB and serves the request from
 * it; for larger requests it bump-allocates the block directly so a big object
 * does not waste a whole TLAB.
 * ====================================================================== */

static unsigned char *pgc_alloc_slow(pgc_thread *self, size_t total_bytes)
{
    unsigned char *raw = NULL;

    pgc_lock();

    if (total_bytes > PGC_TLAB_BYTES) {
        /* Too large for a TLAB: allocate the block directly from the heap. The
         * thread keeps its current TLAB (this large object bypasses it), so no
         * tail is abandoned here. */
        raw = pgc_heap_bump(total_bytes);
    } else {
        /* Refilling abandons the current TLAB's unused tail. Fill that tail with
         * a filler object so the heap stays densely walkable; otherwise the
         * collector's heap walks would hit the zeroed gap. self->tlab.top/end
         * are NULL for a thread that has never had a TLAB (fill is a no-op). */
        pgc_fill_gap(self->tlab.top, self->tlab.end);

        unsigned char *chunk = pgc_heap_bump(PGC_TLAB_BYTES);
        if (chunk != NULL) {
            self->tlab.top = chunk;
            self->tlab.end = chunk + PGC_TLAB_BYTES;
            raw = self->tlab.top;
            self->tlab.top += total_bytes;
        }
    }

    pgc_unlock();
    return raw;
}

/* =========================================================================
 * Core allocation
 *
 * Compute the aligned total size, try the TLAB fast path, fall back to the
 * slow path, and on heap exhaustion trigger a collection and retry once.
 * descriptor is the GC type descriptor for traced objects, or
 * PGC_ATOMIC_DESCRIPTOR for no-scan objects.
 * ====================================================================== */

/* After serving `payload` bytes (object total = header + payload) at `raw`
 * within `self`'s TLAB (with tlab.top already advanced past it), a tail of
 * tlab.end - tlab.top bytes remains. A tail of 1..PGC_HEADER_SIZE-1 bytes is
 * too small to ever hold a filler header, which would later strand an
 * unfillable sub-header gap and derail the heap walk. Absorb such a tail into
 * THIS object: extend its payload to consume the tail and advance tlab.top to
 * tlab.end, so the only possible tails are 0 or >= PGC_HEADER_SIZE (fillable).
 * Returns the (possibly enlarged) payload size to write into the header. Only
 * valid when the object was served from the TLAB (not a direct large alloc). */
static size_t pgc_absorb_tiny_tail(pgc_thread *self, unsigned char *raw,
                                   size_t payload)
{
    unsigned char *obj_end = raw + PGC_HEADER_SIZE + payload;
    /* Only applies when this object sits at the current TLAB front. */
    if (self->tlab.top != obj_end || self->tlab.end == NULL)
        return payload;
    size_t tail = (size_t)(self->tlab.end - self->tlab.top);
    if (tail == 0 || tail >= PGC_HEADER_SIZE)
        return payload;  /* tail is 0 or fillable: leave it */
    /* Swallow the 1..23 byte tail. */
    self->tlab.top = self->tlab.end;
    return payload + tail;
}

static void *pgc_alloc_internal(size_t payload_size, const void *descriptor)
{
    size_t payload = pgc_align_up_alloc(payload_size);
    size_t total   = PGC_HEADER_SIZE + payload;

    pgc_thread *self = pgc_current_thread();

    /* Debug: force periodic collections under PEKO_GC_STRESS. Called from a
     * Peko allocation site (a statepoint), so the caller's roots are recorded
     * and it is a valid point to collect. */
    unsigned long stress = pgc_stress_interval();
    if (stress != 0 && self != NULL) {
        static _Atomic unsigned long alloc_counter = 0;
        unsigned long c = atomic_fetch_add_explicit(&alloc_counter, 1,
                                                    memory_order_relaxed);
        if (c % stress == 0)
            pgc_collect();
    }

    /* Fast path: bump within the thread's TLAB, no lock. */
    if (self != NULL) {
        unsigned char *raw = self->tlab.top;
        if (raw != NULL && raw + total <= self->tlab.end) {
            self->tlab.top = raw + total;
            payload = pgc_absorb_tiny_tail(self, raw, payload);
            return pgc_init_header(raw, payload, descriptor);
        }
    }

    /* Slow path: refill TLAB or direct-allocate under the lock. */
    if (self != NULL) {
        unsigned char *raw = pgc_alloc_slow(self, total);
        if (raw != NULL) {
            payload = pgc_absorb_tiny_tail(self, raw, payload);
            return pgc_init_header(raw, payload, descriptor);
        }

        /* Heap exhausted: collect and retry once. */
        pgc_collect();
        raw = pgc_alloc_slow(self, total);
        if (raw != NULL) {
            payload = pgc_absorb_tiny_tail(self, raw, payload);
            return pgc_init_header(raw, payload, descriptor);
        }

        /* Still no memory after a collection: out of heap. */
        return NULL;
    }

    /* No attached thread context. A thread must attach before allocating;
     * fall back to a direct, locked allocation so early/bootstrap paths do
     * not crash, but this indicates a missing pgc_thread_attach. */
    pgc_lock();
    unsigned char *raw = pgc_heap_bump(total);
    pgc_unlock();
    if (raw == NULL) {
        pgc_collect();
        pgc_lock();
        raw = pgc_heap_bump(total);
        pgc_unlock();
    }
    if (raw == NULL)
        return NULL;
    return pgc_init_header(raw, payload, descriptor);
}

/* =========================================================================
 * Public allocation entry points
 * ====================================================================== */

void *pgc_alloc_managed(const void *descriptor, size_t size)
{
    return pgc_alloc_internal(size, descriptor);
}

void *pgc_alloc_atomic(size_t size)
{
    return pgc_alloc_internal(size, PGC_ATOMIC_DESCRIPTOR);
}
