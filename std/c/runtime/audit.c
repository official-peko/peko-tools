/* =========================================================================
 * audit.c -- a READ-ONLY heap auditor.
 *
 * Walks the heap exactly once and validates every object against a strict
 * predicate, WITHOUT changing any collector behavior. Intended to be called at
 * the very start of pgc_collect (after stop-the-world + TLAB fill, before mark)
 * so it sees the same heap the real passes are about to walk.
 *
 * The collector's own passes trust the heap and truncate on the first bad
 * object, so any inconsistency surfaces far from its origin. This auditor
 * forces the FIRST inconsistency to announce itself at its origin (which
 * object, which field, why) instead of faulting elsewhere on a later pass.
 *
 * It validates using only what the runtime already has:
 *   - descriptors live in the binary's const section, never inside the GC heap;
 *     so a "descriptor" pointing into the heap is corrupt.
 *   - a FIXED descriptor's largest managed offset must fit within the object's
 *     recorded payload size.
 *   - sizes must be nonzero, 8-aligned, in-bounds; the next object must begin
 *     exactly where this one ends, or the span must be an explicit gap.
 *
 * It does NOT allocate, move, or write anything. Findings go to stderr.
 * ====================================================================== */

#include "./include/pgc_internal.h"
#include <stdio.h>
#include <stdint.h>
#include <stdbool.h>

/* The single global GC state (defined in the runtime). */
extern pgc_state g_gc;

/* Tunable: how many objects to keep in the trailing ring for context on a
 * failure (so we can print the objects leading up to the culprit). */
#define PGC_AUDIT_RING 4

/* Result of one audit pass. */
typedef struct {
    size_t objects;        /* well-formed objects walked                    */
    size_t gaps;           /* filler/zero gaps skipped                      */
    size_t bytes_live;     /* sum of total bytes of well-formed objects     */
    int    failed;         /* nonzero if validation tripped                 */
} pgc_audit_result;

/* ---- low-level predicates ------------------------------------------------ */

static bool addr_in_heap(const void *p)
{
    const unsigned char *c = (const unsigned char *)p;
    return c >= g_gc.heap.base && c < g_gc.heap.top;
}

/* A plausible descriptor: the atomic sentinel, OR a pointer-aligned address
 * that is OUTSIDE the heap (descriptors are emitted into the binary's const
 * section). The alignment and out-of-heap checks reject interior pointers and
 * misaligned values that are not real descriptor bases. */
static bool descriptor_plausible(const void *desc)
{
    if (desc == PGC_ATOMIC_DESCRIPTOR)
        return true;
    if (desc == NULL)
        return false;
    if (((uintptr_t)desc & 0x7u) != 0)        /* must be 8-aligned          */
        return false;
    if (addr_in_heap(desc))                    /* descriptors are never heap */
        return false;
    return true;
}

/* For a FIXED descriptor, the largest payload-relative offset of a managed
 * child + 8 (the pointer it names) must fit within the object's payload size.
 * Returns the required minimum payload size, or 0 if not FIXED / no children.
 * This is the check that catches an under-sized object whose construction
 * writes managed fields past its recorded extent. */
static size_t fixed_min_payload(const void *desc)
{
    if (desc == PGC_ATOMIC_DESCRIPTOR || desc == NULL)
        return 0;
    const pgc_descriptor *d = (const pgc_descriptor *)desc;
    if (d->kind != PGC_DESC_FIXED)
        return 0;
    const pgc_descriptor_fixed *f = (const pgc_descriptor_fixed *)desc;
    if (f->count <= 0)
        return 0;
    int64_t max_off = 0;
    for (int32_t i = 0; i < f->count; i++) {
        if (f->offsets[i] > max_off)
            max_off = f->offsets[i];
    }
    return (size_t)max_off + sizeof(void *);
}

/* Read the descriptor kind safely-ish (we already checked plausibility). */
static int desc_kind(const void *desc)
{
    if (desc == PGC_ATOMIC_DESCRIPTOR)
        return -1;                 /* atomic: no kind, no children          */
    return ((const pgc_descriptor *)desc)->kind;
}

/* ---- the audit walk ------------------------------------------------------ */

static void audit_report(const char *why,
                         const unsigned char *at,
                         const pgc_header *h,
                         const pgc_header *ring[],
                         int ring_len)
{
    fprintf(stderr,
            "[pgc][audit] FAIL: %s\n"
            "             at %p: size=%lu forward=%lu descriptor=%p\n",
            why, (const void *)at,
            (unsigned long)(h ? h->size : 0),
            (unsigned long)(h ? h->forward : 0),
            (const void *)(h ? h->descriptor : NULL));
    /* Print the trailing objects that led up to the culprit, oldest first. */
    for (int i = 0; i < ring_len; i++) {
        const pgc_header *r = ring[i];
        if (!r) continue;
        fprintf(stderr,
                "             prev[-%d] %p: size=%lu descriptor=%p\n",
                ring_len - i, (const void *)r,
                (unsigned long)r->size, (const void *)r->descriptor);
    }
}

/* Walk [base, top) once. Read-only. Returns counts and whether it tripped.
 * `stop_on_first` controls whether we abort the walk at the first failure
 * (true: pinpoint the origin) or keep going to tally how widespread it is. */
pgc_audit_result pgc_audit_heap(int stop_on_first)
{
    pgc_audit_result res = {0, 0, 0, 0};

    unsigned char *scan = g_gc.heap.base;
    unsigned char *top  = g_gc.heap.top;

    const pgc_header *ring[PGC_AUDIT_RING] = {0};
    int ring_len = 0;

    while (scan < top) {
        pgc_header *h = (pgc_header *)scan;

        /* (a) header must fit. */
        if (scan + PGC_HEADER_SIZE > top) {
            audit_report("header runs past heap top", scan, NULL, ring, ring_len);
            res.failed = 1;
            break;
        }

        size_t size  = (size_t)h->size;
        size_t total = PGC_HEADER_SIZE + size;

        /* A zero-payload object with a PLAUSIBLE descriptor is a legitimate,
         * walkable 24-byte object, NOT a gap: pgc_fill_gap fillers (atomic) and
         * zero-capture closure contexts (real descriptor, codegen allocates
         * payload size 0) both look like this. Step over it. Only a size-0
         * object with an implausible descriptor is a genuine gap/desync. */
        if (size == 0 && descriptor_plausible(h->descriptor)) {
            res.gaps++;            /* count it as an (intentional) empty object */
            if (ring_len < PGC_AUDIT_RING) {
                ring[ring_len++] = h;
            } else {
                for (int i = 1; i < PGC_AUDIT_RING; i++) ring[i - 1] = ring[i];
                ring[PGC_AUDIT_RING - 1] = h;
            }
            scan += PGC_HEADER_SIZE;
            continue;
        }

        /* (b) a zero size with an implausible descriptor means we walked into a
         * gap or off the rails. A real object never has size 0. */
        if (size == 0) {
            audit_report("object has size 0 (gap or desync)", scan, h, ring, ring_len);
            res.failed = 1;

            /* Measure the gap: scan forward word-by-word until we find a word
             * that looks like the START of a valid object header, i.e. a
             * plausible (nonzero, 8-aligned, in-bounds) size whose descriptor
             * word (two words later) is a plausible descriptor. Report the gap
             * size and the resuming object's descriptor -- this names which
             * allocation path left the hole (TLAB-tail-sized vs single-object,
             * and the type that resumes after it). */
            {
                unsigned char *g = scan;
                unsigned char *resume = NULL;
                while (g + PGC_HEADER_SIZE <= top) {
                    pgc_header *cand = (pgc_header *)g;
                    size_t csz = (size_t)cand->size;
                    if (csz != 0 && (csz & 0x7u) == 0 &&
                        g + PGC_HEADER_SIZE + csz <= top &&
                        descriptor_plausible(cand->descriptor)) {
                        resume = g;
                        break;
                    }
                    g += sizeof(void *);
                }
                if (resume) {
                    pgc_header *rh = (pgc_header *)resume;
                    fprintf(stderr,
                            "[pgc][audit] gap is %ld bytes [%p, %p); resumes with "
                            "object size=%lu descriptor=%p\n",
                            (long)(resume - scan), (void *)scan, (void *)resume,
                            (unsigned long)rh->size, (const void *)rh->descriptor);
                } else {
                    fprintf(stderr,
                            "[pgc][audit] gap from %p runs to heap_top %p "
                            "(%ld bytes); no valid object resumes\n",
                            (void *)scan, (void *)top, (long)(top - scan));
                }
            }
            if (stop_on_first) break;
            /* Best-effort resync: step one word and keep tallying. */
            scan += sizeof(void *);
            continue;
        }

        /* (c) size sanity: 8-aligned, no overflow, in-bounds. */
        if ((size & 0x7u) != 0) {
            audit_report("size not 8-aligned", scan, h, ring, ring_len);
            res.failed = 1;
            if (stop_on_first) break;
        }
        if (scan + total > top) {
            audit_report("object extends past heap top", scan, h, ring, ring_len);
            res.failed = 1;
            if (stop_on_first) break;
        }

        /* (d) descriptor must be plausible (atomic, or aligned + out-of-heap).
         * Catches the interior-descriptor garbage (e.g. a header whose
         * "descriptor" points into the heap or is misaligned). */
        if (!descriptor_plausible(h->descriptor)) {
            audit_report("implausible descriptor (interior/in-heap/misaligned)",
                         scan, h, ring, ring_len);
            res.failed = 1;
            if (stop_on_first) break;
        } else {
            /* (e) THE KEY CHECK: a FIXED descriptor's children must fit inside
             * the recorded payload size. An under-sized object (size smaller
             * than its descriptor implies) is caught here, at its origin. */
            int k = desc_kind(h->descriptor);
            if (k == PGC_DESC_FIXED) {
                size_t need = fixed_min_payload(h->descriptor);
                if (need > size) {
                    char msg[160];
                    snprintf(msg, sizeof(msg),
                             "object too small for descriptor: size=%lu but "
                             "descriptor needs >= %lu (under-allocation)",
                             (unsigned long)size, (unsigned long)need);
                    audit_report(msg, scan, h, ring, ring_len);
                    res.failed = 1;
                    if (stop_on_first) break;
                }
            } else if (k != PGC_DESC_ARRAY && k != -1) {
                audit_report("descriptor kind is neither FIXED nor ARRAY nor atomic",
                             scan, h, ring, ring_len);
                res.failed = 1;
                if (stop_on_first) break;
            }
        }

        /* Record and advance. */
        res.objects++;
        res.bytes_live += total;

        /* maintain trailing ring */
        if (ring_len < PGC_AUDIT_RING) {
            ring[ring_len++] = h;
        } else {
            for (int i = 1; i < PGC_AUDIT_RING; i++) ring[i - 1] = ring[i];
            ring[PGC_AUDIT_RING - 1] = h;
        }

        scan += total;
    }

    return res;
}

/* Public entry point: audit, and if it trips, print a one-line summary. Safe to
 * call from pgc_collect while the world is stopped. Returns nonzero on failure
 * so the caller may choose to abort() in debug builds. */
int pgc_audit(void)
{
    pgc_audit_result r = pgc_audit_heap(/*stop_on_first=*/1);
    if (r.failed) {
        fprintf(stderr,
                "[pgc][audit] heap invalid after %lu well-formed objects "
                "(%lu live bytes) -- see FAIL above for the culprit.\n",
                (unsigned long)r.objects, (unsigned long)r.bytes_live);
        return 1;
    }
    return 0;
}
