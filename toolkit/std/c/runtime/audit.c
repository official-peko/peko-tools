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

/* dladdr and Dl_info are GNU extensions on glibc, gated behind _GNU_SOURCE, so
   define it before any system header. */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include "./include/pgc_internal.h"
#include <stdio.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>

/* dladdr symbolizes addresses for the verifier's diagnostics. It is absent on
   the mobile and cross-compile toolchains, so guard on the header's presence
   and fall back to no symbol name where it is missing. */
#if defined(__has_include)
#if __has_include(<dlfcn.h>)
#define PGC_HAVE_DLADDR 1
#include <dlfcn.h>
#endif
#endif

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

/* =========================================================================
 * pgc_verify_mark -- an INDEPENDENT mark-completeness check.
 *
 * Called after the precise mark (pgc_mark_all) and before compaction, while
 * the world is stopped. The precise mark trusts the compiler's stackmaps and
 * each type's descriptor. This pass distrusts both: it conservatively scans
 * every thread stack and every marked object's payload for words that point
 * exactly at a valid heap object. Any such object that the precise mark left
 * UNMARKED is a candidate missed reference:
 *   - from a stack word  -> a missed stack root (a stackmap gap), or
 *   - from a marked object's payload -> a missed field (a bad type descriptor).
 * The object is live (something points at it) but about to be reclaimed.
 *
 * Conservative scanning has false positives (an integer that happens to equal
 * an object address). Under PEKO_GC_STRESS the real bug reports the SAME child
 * descriptor / parent-offset every cycle; noise is random. Read-only: marks
 * nothing, moves nothing.
 * ====================================================================== */

/* A plausible descriptor is 8-aligned, not a tiny value, and outside the GC
 * heap (descriptors are static consts). Mirrors heap.c's pgc_desc_plausible. */
static int verify_desc_plausible(const void *descriptor)
{
    uintptr_t dv = (uintptr_t)descriptor;
    const unsigned char *dp = (const unsigned char *)descriptor;
    if (descriptor == PGC_ATOMIC_DESCRIPTOR)
        return 1;
    if ((dv & 0x7u) != 0 || dv < 0x10000u)
        return 0;
    if (dp >= g_gc.heap.base && dp < g_gc.heap.end)
        return 0;
    return 1;
}

typedef struct {
    uintptr_t payload;  /* object payload start                          */
    int       marked;   /* precise mark bit                              */
} verify_obj;

/* Print a descriptor's structure so a missed reference names the exact type
 * shape the mark phase used: FIXED lists its traced offsets; ARRAY lists its
 * stride and element descriptor. */
static const char *verify_symbolize(const void *p)
{
#ifdef PGC_HAVE_DLADDR
    Dl_info info;
    if (p != NULL && dladdr(p, &info) != 0 && info.dli_sname != NULL)
        return info.dli_sname;
#else
    (void)p;
#endif
    return "?";
}

static void verify_dump_desc(const void *descriptor)
{
    if (descriptor == NULL) {
        fprintf(stderr, "    desc=NULL\n");
        return;
    }
    if (descriptor == PGC_ATOMIC_DESCRIPTOR) {
        fprintf(stderr, "    desc=ATOMIC (no-scan)\n");
        return;
    }
    if (!verify_desc_plausible(descriptor)) {
        fprintf(stderr, "    desc=%p (implausible)\n", descriptor);
        return;
    }
    const pgc_descriptor *d = (const pgc_descriptor *)descriptor;
    if (d->kind == PGC_DESC_FIXED) {
        const pgc_descriptor_fixed *df = (const pgc_descriptor_fixed *)d;
        fprintf(stderr, "    desc=%p <%s> FIXED count=%d offsets=[", descriptor,
                verify_symbolize(descriptor), df->count);
        for (int32_t k = 0; k < df->count && k < 16; k++)
            fprintf(stderr, "%lld ", (long long)df->offsets[k]);
        fprintf(stderr, "]\n");
    } else if (d->kind == PGC_DESC_ARRAY) {
        const pgc_descriptor_array *da = (const pgc_descriptor_array *)d;
        fprintf(stderr, "    desc=%p <%s> ARRAY stride=%lld element=%p <%s>\n",
                descriptor, verify_symbolize(descriptor), (long long)da->stride,
                da->element, verify_symbolize(da->element));
    } else {
        fprintf(stderr, "    desc=%p kind=%d (unknown)\n", descriptor, d->kind);
    }
}

/* Binary search the ascending object table for an exact payload-start match.
 * Returns the index, or -1 when addr is not a live object's payload start. */
static long verify_find(const verify_obj *tab, size_t n, uintptr_t addr)
{
    size_t lo = 0, hi = n;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (tab[mid].payload == addr)
            return (long)mid;
        if (tab[mid].payload < addr)
            lo = mid + 1;
        else
            hi = mid;
    }
    return -1;
}

int pgc_verify_mark(unsigned long generation)
{
    /* 1. Build an ascending table of every heap object's payload + mark bit. */
    size_t count = 0;
    unsigned char *scan = g_gc.heap.base;
    while (scan < g_gc.heap.top) {
        pgc_header *h = (pgc_header *)scan;
        if (h->size == 0 && !verify_desc_plausible(h->descriptor))
            break;
        size_t total = pgc_object_total(h);
        if (total < PGC_HEADER_SIZE)
            break;
        count++;
        scan += total;
    }
    if (count == 0)
        return 0;

    verify_obj *tab = (verify_obj *)malloc(count * sizeof(verify_obj));
    if (tab == NULL)
        return 0;

    size_t i = 0;
    scan = g_gc.heap.base;
    while (scan < g_gc.heap.top && i < count) {
        pgc_header *h = (pgc_header *)scan;
        if (h->size == 0 && !verify_desc_plausible(h->descriptor))
            break;
        size_t total = pgc_object_total(h);
        if (total < PGC_HEADER_SIZE)
            break;
        tab[i].payload = (uintptr_t)PGC_PAYLOAD_OF(h);
        tab[i].marked = pgc_is_marked(h) ? 1 : 0;
        i++;
        scan += total;
    }
    size_t n = i;

    int findings = 0;
    const uintptr_t heap_lo = (uintptr_t)g_gc.heap.base;
    const uintptr_t heap_hi = (uintptr_t)g_gc.heap.top;

    /* 2. Missed stack roots: conservatively scan each stopped thread's stack
     * between its captured frame and its base. */
    for (int t = 0; t < g_gc.thread_count && findings < 40; t++) {
        pgc_thread *thread = &g_gc.threads[t];
        if (!thread->in_use || thread->stack_top == NULL || thread->stack_base == NULL)
            continue;
        uintptr_t top = (uintptr_t)thread->stack_top;
        uintptr_t base = (uintptr_t)thread->stack_base;
        if (top > base) {
            uintptr_t tmp = top;
            top = base;
            base = tmp;
        }
        top &= ~(uintptr_t)7;
        for (uintptr_t w = top; w + sizeof(void *) <= base && findings < 40;
             w += sizeof(void *)) {
            uintptr_t p = *(uintptr_t *)w;
            if (p < heap_lo || p >= heap_hi)
                continue;
            long oi = verify_find(tab, n, p);
            if (oi >= 0 && !tab[oi].marked) {
                pgc_header *ch = PGC_HEADER_OF((void *)p);
                fprintf(stderr,
                        "[pgc][verify gen=%lu] MISSED STACK ROOT: thread %d "
                        "stack@%p -> unmarked obj payload=%p size=%lu desc=%p\n",
                        generation, t, (void *)w, (void *)p,
                        (unsigned long)ch->size, (void *)ch->descriptor);
                findings++;
            }
        }
    }

    /* 3. Missed fields: conservatively scan each MARKED object's payload for a
     * word that points at an unmarked live object. */
    int dumped_field = 0;
    for (size_t o = 0; o < n && findings < 40; o++) {
        if (!tab[o].marked)
            continue;
        pgc_header *ph = PGC_HEADER_OF((void *)tab[o].payload);
        size_t psize = pgc_object_size(ph);
        unsigned char *pl = (unsigned char *)tab[o].payload;
        for (size_t off = 0; off + sizeof(void *) <= psize && findings < 40;
             off += sizeof(void *)) {
            uintptr_t p = *(uintptr_t *)(pl + off);
            if (p < heap_lo || p >= heap_hi)
                continue;
            long oi = verify_find(tab, n, p);
            if (oi >= 0 && !tab[oi].marked) {
                pgc_header *ch = PGC_HEADER_OF((void *)p);
                fprintf(stderr,
                        "[pgc][verify gen=%lu] MISSED FIELD: parent payload=%p "
                        "desc=%p off=%lu -> unmarked child payload=%p size=%lu "
                        "desc=%p\n",
                        generation, (void *)tab[o].payload,
                        (void *)ph->descriptor, (unsigned long)off, (void *)p,
                        (unsigned long)ch->size, (void *)ch->descriptor);
                if (!dumped_field) {
                    dumped_field = 1;
                    fprintf(stderr, "  parent:\n");
                    verify_dump_desc(ph->descriptor);
                    fprintf(stderr, "  child:\n");
                    verify_dump_desc(ch->descriptor);
                }
                findings++;
            }
        }
    }

    free(tab);
    return findings;
}
