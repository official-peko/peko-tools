/*
 * heap.c
 * Managed heap region, the raw bump primitive, and object-size helpers.
 *
 * The heap is a single contiguous region. Allocation bumps g_gc.heap.top
 * toward .end; compaction (the three passes below) slides live objects toward
 * .base and resets .top. This file owns the low-level heap mechanics and the
 * size-of-an-object computation that both the allocator and the collector
 * need. The compaction passes are implemented in the compaction step; they are
 * present here as the heap-walking half of mark-compact.
 */

#include "./include/pgc_internal.h"

#include <string.h>
#include <stdlib.h>

#ifndef _WIN32
#  include <sys/mman.h>
#endif

/* Default heap size when pgc_init is given 0: 64 MiB. */
#define PGC_DEFAULT_HEAP_BYTES  ((size_t)64 * 1024 * 1024)

/* All allocations are rounded up to this alignment so every object (and thus
 * every header and payload) is at least 16-byte aligned. This keeps the mark
 * bit in the header's low bits safe and satisfies the alignment of any field
 * the payload might hold. */
#define PGC_ALIGN  ((size_t)16)

static size_t pgc_align_up(size_t n)
{
    return (n + (PGC_ALIGN - 1)) & ~(PGC_ALIGN - 1);
}

/* =========================================================================
 * Heap reservation / release
 *
 * Reserve a fixed region up front. On POSIX we mmap anonymous memory; on
 * Windows we VirtualAlloc commit. A single committed region keeps addresses
 * stable for the life of the process and lets compaction slide freely within
 * it.
 * ====================================================================== */

int pgc_heap_create(size_t heap_bytes)
{
    if (heap_bytes == 0)
        heap_bytes = PGC_DEFAULT_HEAP_BYTES;
    heap_bytes = pgc_align_up(heap_bytes);

    unsigned char *base;

#ifdef _WIN32
    base = (unsigned char *)VirtualAlloc(NULL, heap_bytes,
                                         MEM_COMMIT | MEM_RESERVE,
                                         PAGE_READWRITE);
    if (base == NULL)
        return 0;
#else
    base = (unsigned char *)mmap(NULL, heap_bytes,
                                 PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED)
        return 0;
#endif

    g_gc.heap.base = base;
    g_gc.heap.top  = base;
    g_gc.heap.end  = base + heap_bytes;
    g_gc.heap.size = heap_bytes;
    return 1;
}

void pgc_heap_destroy(void)
{
    if (g_gc.heap.base == NULL)
        return;

#ifdef _WIN32
    VirtualFree(g_gc.heap.base, 0, MEM_RELEASE);
#else
    munmap(g_gc.heap.base, g_gc.heap.size);
#endif

    g_gc.heap.base = NULL;
    g_gc.heap.top  = NULL;
    g_gc.heap.end  = NULL;
    g_gc.heap.size = 0;
}

/* =========================================================================
 * Raw bump
 *
 * Carve total_bytes (header + payload, already aligned by the caller path)
 * from the global heap. Returns NULL when the heap is exhausted; the caller
 * decides whether to trigger a collection and retry. The global lock must be
 * held by the caller, because this mutates the shared bump pointer (TLAB
 * refills and direct large allocations both come through here under the lock).
 * ====================================================================== */

unsigned char *pgc_heap_bump(size_t total_bytes)
{
    unsigned char *result = g_gc.heap.top;
    if (result + total_bytes > g_gc.heap.end)
        return NULL;
    g_gc.heap.top = result + total_bytes;
    return result;
}

/* =========================================================================
 * Object sizing
 *
 * The payload size is recorded in the header's size word at allocation, so the
 * collector can step object-to-object when walking the heap during compaction.
 * No descriptor variant encodes the object's own byte size (fixed and array
 * objects took their size from the allocation call, atomic objects from the
 * caller), which is why the size is stored in the header rather than derived.
 * ====================================================================== */

size_t pgc_object_size(const pgc_header *header)
{
    return (size_t)header->size;
}

/* Is `descriptor` a plausible descriptor base (atomic sentinel, or an aligned,
 * not-implausibly-low, out-of-heap pointer)? Mirrors mark.c's validity check.
 * Used to decide whether a size-0 object is a legitimate zero-payload object
 * (e.g. a closure context with no captures, or a pgc_fill_gap filler) versus
 * genuine garbage. We do NOT dereference here -- address plausibility only. */
static int pgc_desc_plausible_heapc(const void *descriptor)
{
    if (descriptor == NULL || descriptor == PGC_ATOMIC_DESCRIPTOR)
        return 1;
    uintptr_t d = (uintptr_t)descriptor;
    if ((d & 0x7u) != 0)
        return 0;
    if (d < 0x10000u)
        return 0;
    {
        const unsigned char *p = (const unsigned char *)descriptor;
        if (p >= g_gc.heap.base && p < g_gc.heap.end)
            return 0;   /* descriptors are never inside the GC heap */
    }
    return 1;
}

size_t pgc_object_total(const pgc_header *header)
{
    /* A ZERO-PAYLOAD object is legitimate and walkable, not a desync. Two real
     * sources produce them: pgc_fill_gap fillers for an exactly-24-byte gap
     * (atomic descriptor), and zero-capture closure contexts (a real descriptor
     * with no managed offsets) which codegen allocates at payload size 0. In
     * both cases the object is a valid header-only 24-byte unit. Every heap walk
     * must step over it rather than treat size==0 as the end-of-heap / desync
     * sentinel.
     *
     * We accept size==0 as walkable when the descriptor is PLAUSIBLE (atomic, or
     * an aligned out-of-heap base). A size==0 object with an IMPLAUSIBLE
     * descriptor is genuine garbage and still totals 0, tripping the desync
     * guards. */
    if (header->size == 0 && pgc_desc_plausible_heapc(header->descriptor))
        return PGC_HEADER_SIZE;
    return PGC_HEADER_SIZE + (size_t)header->size;
}

/* =========================================================================
 * TLAB gap filling
 *
 * TLABs make the heap non-contiguous: the unused tail of a TLAB (abandoned when
 * a thread refills, or current at collection time) is reserved space that holds
 * no objects. The collector's heap walks step object-to-object assuming a dense
 * base..top layout, so an unfilled gap derails them (they read zeroed bytes as a
 * zero-size "object" and either stall or overrun).
 *
 * pgc_fill_gap writes a single atomic filler object spanning [gap_start,
 * gap_end) so the region walks as one well-formed, unmarked (therefore dead and
 * reclaimable) object. The walk steps exactly gap_size and lands on whatever
 * follows.
 *
 * Precondition: a non-empty gap is either >= PGC_HEADER_SIZE (fillable) or zero.
 * The allocator guarantees this by never stranding a 1..PGC_HEADER_SIZE-1 byte
 * tail (see pgc_alloc.c: tiny tails are absorbed into the preceding object). If
 * a sub-header gap is ever seen here it is a bug; we report and refuse rather
 * than corrupt the heap.
 * ====================================================================== */

void pgc_fill_gap(unsigned char *gap_start, unsigned char *gap_end)
{
    if (gap_end <= gap_start)
        return;  /* empty gap: nothing to fill */

    size_t gap = (size_t)(gap_end - gap_start);

    if (gap < PGC_HEADER_SIZE) {
        /* Cannot place a header. The allocator's tiny-tail absorption should
         * prevent this. Zero the bytes rather than writing past gap_end. */
        memset(gap_start, 0, gap);
        return;
    }

    pgc_header *h = (pgc_header *)gap_start;
    h->size       = (uintptr_t)(gap - PGC_HEADER_SIZE);
    h->forward    = 0;                      /* unmarked -> dead -> reclaimed   */
    h->descriptor = PGC_ATOMIC_DESCRIPTOR;  /* atomic: tracer skips it         */
    /* NOTE: when gap == PGC_HEADER_SIZE (a 24-byte hole), size is 0. That is a
     * VALID zero-payload filler, not a desync: pgc_object_total() and the heap
     * walks recognize (size==0 && descriptor==ATOMIC) as a walkable 24-byte
     * object and step over it. */
}

/* =========================================================================
 * Object-start index for interior-pointer resolution.
 *
 * A flat array of every object's payload start, in ascending address order
 * (the heap walk naturally produces them sorted). pgc_resolve_base binary-
 * searches for the object whose extent contains a given address, so an
 * interior or one-past-end pointer can be mapped back to its base.
 *
 * Why this is needed: managed-buffer element references (string[i], escaping
 * array element pointers) are interior pointers. The root scanner and the
 * reference-update pass would otherwise treat them as object bases and read
 * the bytes 24 below them as a header -- corrupting marking and relocation.
 *
 * Built once per collection while the world is stopped (before any object
 * moves) and freed at the end of the cycle.
 * ====================================================================== */

typedef struct pgc_obj_rec {
    unsigned char *payload;   /* object payload start                        */
    size_t         total;     /* header + payload bytes (object extent)      */
} pgc_obj_rec;

static pgc_obj_rec *g_obj_index;
static size_t       g_obj_count;
static size_t       g_obj_capacity;

void pgc_object_index_build(void)
{
    g_obj_count = 0;

    unsigned char *scan = g_gc.heap.base;
    unsigned char *prev_scan = NULL;       /* start of the previous object   */
    while (scan < g_gc.heap.top) {
        pgc_header *h = (pgc_header *)scan;
        size_t total = pgc_object_total(h);
        if (total == 0)
            break;  /* defensive: never advance by zero */

        /* Desync detection WITHOUT dereferencing the descriptor (an arbitrary
         * descriptor value may not be a readable address -- dereferencing it to
         * read its kind is exactly what would fault). Instead validate the
         * object's own SIZE, which a desynced walk corrupts. Accept any size
         * that is nonzero, pointer-aligned (8), and does not run past the heap
         * top. Note: real object payload sizes are 16-aligned, but TLAB-gap
         * FILLER objects have size = gap - PGC_HEADER_SIZE, which is 8-aligned
         * (gap is 16-aligned, the 24-byte header is 8-aligned), so the check
         * must permit 8-alignment, not require 16, or it false-flags fillers.
         * A walk that stepped into string/garbage data sees a size that is
         * unaligned, zero, or overruns -- a genuine desync; log and stop. */
        size_t this_size  = (size_t)h->size;
        unsigned char *this_end = scan + total;
        /* A zero-payload atomic filler (24-byte gap filler) is legitimate and
         * walkable: total is already PGC_HEADER_SIZE via pgc_object_total, and
         * this_end = scan + 24 is in-bounds. Treat it as valid explicitly so the
         * this_size != 0 rule below does not false-flag it as a desync. */
        int is_empty_filler = (this_size == 0) &&
                              pgc_desc_plausible_heapc(h->descriptor);
        int size_ok = is_empty_filler ||
                      ((this_size != 0) &&
                       ((this_size & 0x7u) == 0) &&     /* 8-aligned (perms fillers) */
                       (this_end > scan) &&             /* no overflow */
                       (this_end <= g_gc.heap.top));
        if (!size_ok) {
            /* Implausible size: stop the walk rather than stepping by a bad
             * total. Lookups past this point fail closed, conservatively
             * skipping those roots. */
            break;
        }

        if (g_obj_count == g_obj_capacity) {
            size_t new_cap = (g_obj_capacity == 0) ? 1024 : g_obj_capacity * 2;
            pgc_obj_rec *grown =
                (pgc_obj_rec *)realloc(g_obj_index, new_cap * sizeof(pgc_obj_rec));
            if (grown == NULL) {
                /* Out of memory building the index: leave what we have. Lookups
                 * for objects past this point will fail closed (return NULL),
                 * which conservatively skips those roots rather than crashing. */
                break;
            }
            g_obj_index    = grown;
            g_obj_capacity = new_cap;
        }

        g_obj_index[g_obj_count].payload = (unsigned char *)PGC_PAYLOAD_OF(h);
        g_obj_index[g_obj_count].total   = total;
        g_obj_count++;

        prev_scan = scan;
        (void)prev_scan;
        scan += total;
    }
}

void pgc_object_index_free(void)
{
    free(g_obj_index);
    g_obj_index    = NULL;
    g_obj_count    = 0;
    g_obj_capacity = 0;
}

/* Map an arbitrary in-heap address to the payload start of the object that
 * contains it. Accepts the object's payload start, any interior byte, and the
 * one-past-end address (a forward cursor that finished filling a buffer points
 * exactly at payload_end; with no inter-object gaps that address falls in the
 * next object's HEADER, never its payload, so it is unambiguously attributed to
 * the object it is the end of). Returns NULL when addr is outside every object.
 *
 * O(log n) via binary search for the greatest payload start <= addr, then a
 * containment check against that object's extent. */
void *pgc_resolve_base(void *addr)
{
    unsigned char *p = (unsigned char *)addr;

    if (p < g_gc.heap.base + PGC_HEADER_SIZE || p > g_gc.heap.top)
        return NULL;
    if (g_obj_count == 0)
        return NULL;

    size_t lo = 0, hi = g_obj_count, ans = (size_t)-1;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (g_obj_index[mid].payload <= p) {
            ans = mid;
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if (ans == (size_t)-1)
        return NULL;

    unsigned char *payload   = g_obj_index[ans].payload;
    /* End of the object block = its header start + total. The header start is
     * payload - PGC_HEADER_SIZE, so the block end is payload - HEADER + total.
     * Accept [payload, block_end] inclusive so a one-past-payload cursor maps
     * here. */
    unsigned char *block_end = payload - PGC_HEADER_SIZE + g_obj_index[ans].total;
    if (p >= payload && p <= block_end)
        return payload;

    return NULL;
}

/* =========================================================================
 * Compaction: sliding mark-compact (Lisp2), three passes over the heap.
 *
 * Run only while the world is stopped, after marking. The header forward word
 * is repurposed across these passes:
 *
 *   after marking          - low bit is the mark bit (live objects marked)
 *   after compute_forward  - holds the object's NEW payload address if live,
 *                            or 0 if dead. A valid payload address is never 0
 *                            and is 16-aligned, so "forward != 0" cleanly means
 *                            "live" for the update and move passes, and the
 *                            mark bit is no longer consulted.
 *
 * Ordering is essential: update_references must run before move_objects,
 * because it reads each target's forward word (its new address) while objects
 * are still in their old positions. Moving first would destroy that mapping.
 *
 * Pinning: a pinned object keeps its exact address (it does not slide), so any
 * raw pointer foreign code holds to it stays valid. The forwarding cursor
 * skips over pinned objects' fixed extents, which leaves holes (fragmentation)
 * but keeps every address correct.
 * ====================================================================== */

/* Does an object placed at `dest` collide with a pinned live object that keeps
 * its address? If so, return the end of that pinned object so the caller can
 * advance the cursor past it; otherwise NULL.
 *
 * This runs during pgc_compute_forwarding, which sweeps in address order with
 * the destination cursor always at or behind the sweep cursor. By the time a
 * destination is chosen, the object physically at `dest` has already been swept
 * and its forward word overwritten: a dead object holds 0, a live moved object
 * holds its new payload address, and a live pinned object holds its own payload
 * address (the pinned branch sets forward = payload). The mark bit is therefore
 * gone and cannot be used here. A pinned object is identified instead by pin
 * membership together with the self-referential forward word that the pinned
 * branch wrote. */
static unsigned char *pgc_skip_pinned_at(unsigned char *dest)
{
    if (dest + PGC_HEADER_SIZE > g_gc.heap.top || dest < g_gc.heap.base)
        return NULL;
    pgc_header *h = (pgc_header *)dest;
    void *payload = PGC_PAYLOAD_OF(h);
    if (pgc_is_pinned(payload) && h->forward == (uintptr_t)payload) {
        return dest + pgc_object_total(h);
    }
    return NULL;
}

void pgc_compute_forwarding(void)
{
    unsigned char *scan    = g_gc.heap.base;  /* sweeps every object        */
    unsigned char *compact = g_gc.heap.base;  /* next free destination      */

    while (scan < g_gc.heap.top) {
        pgc_header *h = (pgc_header *)scan;
        size_t total = pgc_object_total(h);
        /* A size-0 atomic filler is a walkable 24-byte object (pgc_object_total
         * reports 24). Only size-0 with a NON-atomic descriptor is genuine
         * garbage / desync -> stop. */
        if (h->size == 0 && !pgc_desc_plausible_heapc(h->descriptor))
            break;  /* size-0 + implausible desc = desync; valid size-0 walks (total=24) */

        bool live   = (h->forward & PGC_MARK_BIT) != 0;
        void *payload = PGC_PAYLOAD_OF(h);

        if (!live) {
            h->forward = 0;          /* dead: clear forward (==> not live)   */
            scan += total;
            continue;
        }

        if (pgc_is_pinned(payload)) {
            /* Pinned: stays exactly where it is. Its new payload address is
             * its current payload address. Ensure the compaction cursor does
             * not try to place a later object on top of it. */
            h->forward = (uintptr_t)payload;
            unsigned char *pin_end = scan + total;
            if (compact < pin_end)
                compact = pin_end;
            scan += total;
            continue;
        }

        /* Unpinned live object: place it at `compact`, skipping past any
         * pinned object that currently occupies the destination. */
        unsigned char *past;
        while ((past = pgc_skip_pinned_at(compact)) != NULL)
            compact = past;

        h->forward = (uintptr_t)PGC_PAYLOAD_OF((pgc_header *)compact);
        compact += total;
        scan    += total;
    }
}

/* Rewrite a single slot to its target's forwarding address. The visitor used
 * by update_references for roots, and applied to each managed field of each
 * live object.
 *
 * The target may be an INTERIOR pointer into a managed buffer (an escaping
 * element reference such as string[i]), not the object's payload start. Resolve
 * it to the base object, then rewrite the slot to the base's new location plus
 * the original interior offset, so the reference still points at the same
 * element after the object slides. A base pointer is just the offset-0 case. */
static void pgc_forward_slot(void **slot)
{
    if (slot == NULL)
        return;
    void *target = *slot;
    if (target == NULL)
        return;

    /* Only heap addresses have forwarding info. */
    unsigned char *p = (unsigned char *)target;
    if (p < g_gc.heap.base + PGC_HEADER_SIZE || p > g_gc.heap.top)
        return;

    /* Resolve interior/one-past-end pointers back to the containing object. */
    unsigned char *base = (unsigned char *)pgc_resolve_base(target);
    if (base == NULL)
        return;

    size_t offset = (size_t)(p - base);   /* preserved across the move */

    pgc_header *bh = PGC_HEADER_OF(base);
    if (bh->forward != 0) {
        *slot = (void *)((unsigned char *)bh->forward + offset);  /* new base + offset */
    }
}

/* Trace and forward every managed field of a live object, using its
 * descriptor (mirrors mark.c's tracing but rewrites instead of marks). */
static void pgc_forward_object_fields(void *object, const pgc_header *h)
{
    const void *descriptor = h->descriptor;
    if (descriptor == NULL || descriptor == PGC_ATOMIC_DESCRIPTOR)
        return;

    /* Defensive guard, symmetric with mark.c's trace guard: a valid descriptor
     * is at least 8-aligned and its kind reads as a known descriptor kind. If
     * a corrupt object reached this pass (heap damage upstream), skip it rather
     * than dereferencing string/garbage bytes as a descriptor and crashing.
     * Mirror mark.c's checks: reject misaligned, implausibly-low, and in-heap
     * descriptor pointers BEFORE dereferencing (descriptors are static globals,
     * never tiny values and never in the GC heap). */
    {
        uintptr_t dv = (uintptr_t)descriptor;
        const unsigned char *dp = (const unsigned char *)descriptor;
        if ((dv & 0x7u) != 0 || dv < 0x10000u ||
            (dp >= g_gc.heap.base && dp < g_gc.heap.end)) {
            return;
        }
    }

    const pgc_descriptor *d = (const pgc_descriptor *)descriptor;
    if (d->kind != PGC_DESC_FIXED && d->kind != PGC_DESC_ARRAY) {
        return;
    }

    if (d->kind == PGC_DESC_FIXED) {
        const pgc_descriptor_fixed *df = (const pgc_descriptor_fixed *)d;
        for (int32_t i = 0; i < df->count; i++) {
            void **field = (void **)((unsigned char *)object + df->offsets[i]);
            pgc_forward_slot(field);
        }
    } else if (d->kind == PGC_DESC_ARRAY) {
        const pgc_descriptor_array *da = (const pgc_descriptor_array *)d;
        if (da->stride <= 0)
            return;
        const void *ed = da->element;
        if (ed == NULL || ed == PGC_ATOMIC_DESCRIPTOR)
            return;
        size_t count = pgc_object_size(h) / (size_t)da->stride;
        const pgc_descriptor *eld = (const pgc_descriptor *)ed;
        for (size_t e = 0; e < count; e++) {
            unsigned char *element = (unsigned char *)object + e * (size_t)da->stride;
            if (eld->kind == PGC_DESC_FIXED) {
                const pgc_descriptor_fixed *ef = (const pgc_descriptor_fixed *)eld;
                for (int32_t i = 0; i < ef->count; i++) {
                    void **field = (void **)(element + ef->offsets[i]);
                    pgc_forward_slot(field);
                }
            } else if (eld->kind == PGC_DESC_ARRAY) {
                pgc_forward_slot((void **)element);
            }
        }
    }
}

void pgc_update_references(void)
{
    /* Roots first. */
    pgc_visit_global_roots(pgc_forward_slot);
    pgc_visit_handle_roots(pgc_forward_slot);
    pgc_visit_stack_roots(pgc_forward_slot);

    /* Then every managed field of every live object. Liveness is "forward
     * word non-zero" after compute_forwarding. */
    unsigned char *scan = g_gc.heap.base;
    while (scan < g_gc.heap.top) {
        pgc_header *h = (pgc_header *)scan;
        size_t total = pgc_object_total(h);
        if (h->size == 0 && !pgc_desc_plausible_heapc(h->descriptor))
            break;  /* size-0 + implausible desc = desync; valid size-0 walks (total=24) */
        if (h->forward != 0)
            pgc_forward_object_fields(PGC_PAYLOAD_OF(h), h);
        scan += total;
    }
}

/* =========================================================================
 * Move snapshot
 *
 * The compaction copy runs in two passes. The first pass reads each live
 * object's source header, destination, and extent while every header is still
 * intact and records them here. The second pass performs the memmoves from the
 * snapshot.
 *
 * Splitting the passes keeps every header read off the path of any byte copy.
 * A single pass reads each object's size and forwarding word from a header that
 * an earlier copy in the same pass may have written over, which desyncs the
 * walk. Reading all extents first removes that dependency.
 * ====================================================================== */

typedef struct pgc_move_rec {
    unsigned char *src_header;   /* object header at its pre-compaction site */
    unsigned char *dst_header;   /* object header at its post-compaction site */
    size_t         total;        /* header + payload bytes to copy           */
} pgc_move_rec;

static pgc_move_rec *g_move_list;
static size_t        g_move_count;
static size_t        g_move_capacity;

static void pgc_move_list_push(unsigned char *src_header,
                               unsigned char *dst_header, size_t total)
{
    if (g_move_count == g_move_capacity) {
        size_t new_capacity = (g_move_capacity == 0) ? 1024 : g_move_capacity * 2;
        pgc_move_rec *grown =
            (pgc_move_rec *)realloc(g_move_list, new_capacity * sizeof(pgc_move_rec));
        if (grown == NULL) {
            abort();
        }
        g_move_list     = grown;
        g_move_capacity = new_capacity;
    }
    g_move_list[g_move_count].src_header = src_header;
    g_move_list[g_move_count].dst_header = dst_header;
    g_move_list[g_move_count].total      = total;
    g_move_count++;
}

void pgc_move_objects(void)
{
    /* Pass one: record every live object's source, destination, and extent
     * from the intact headers. A live object has a non-zero forward word
     * holding its destination payload address. */
    g_move_count = 0;

    unsigned char *scan = g_gc.heap.base;
    while (scan < g_gc.heap.top) {
        pgc_header *h = (pgc_header *)scan;
        size_t total = pgc_object_total(h);
        if (h->size == 0 && !pgc_desc_plausible_heapc(h->descriptor))
            break;  /* size-0 + implausible desc = desync; valid size-0 walks (total=24) */

        if (h->forward != 0) {
            unsigned char *dest_payload = (unsigned char *)h->forward;
            unsigned char *dest_header  = dest_payload - PGC_HEADER_SIZE;
            pgc_move_list_push(scan, dest_header, total);
        }
        scan += total;
    }

    /* Pass two: copy each object to its destination, then clear the moved
     * copy's forward word so the destination header is clean (forward 0 =
     * unmarked, no pending forward). The records are in ascending source order,
     * destinations slide toward the base (each destination is at or below its
     * source), so a copy never writes past the next object's source. memmove
     * handles the per-object overlap when a destination overlaps its own
     * source. */
    unsigned char *new_top = g_gc.heap.base;
    for (size_t i = 0; i < g_move_count; i++) {
        unsigned char *src_header = g_move_list[i].src_header;
        unsigned char *dst_header = g_move_list[i].dst_header;
        size_t         total      = g_move_list[i].total;

        if (dst_header != src_header)
            memmove(dst_header, src_header, total);
        ((pgc_header *)dst_header)->forward = 0;

        unsigned char *dst_end = dst_header + total;
        if (dst_end > new_top)
            new_top = dst_end;
    }

    /* The heap is now compacted; resume bump allocation from the new top. */
    g_gc.heap.top = new_top;
}
