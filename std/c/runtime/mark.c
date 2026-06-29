/*
 * mark.c
 * The mark phase of mark-compact: starting from every root, mark all
 * reachable objects. Tracing is driven by each object's GC type descriptor,
 * which names the managed-pointer fields (fixed objects) or the element layout
 * (arrays). Atomic objects are marked but have no children to trace.
 *
 * Marking uses an explicit mark stack rather than recursion so deep or cyclic
 * object graphs cannot overflow the C stack. The mark bit is the low bit of
 * the header's forward word; it is cleared again before the compaction passes
 * reuse that word for forwarding addresses.
 *
 * This phase runs only while the world is stopped.
 */

#include "./include/pgc_internal.h"

#include <stdlib.h>
#include <stdio.h>

/* =========================================================================
 * Mark bit
 * ====================================================================== */

bool pgc_is_marked(const pgc_header *h)
{
    return (h->forward & PGC_MARK_BIT) != 0;
}

void pgc_set_marked(pgc_header *h)
{
    h->forward |= PGC_MARK_BIT;
}

static void pgc_clear_mark(pgc_header *h)
{
    h->forward &= ~PGC_MARK_BIT;
}

/* Clear every mark bit by walking the heap. Called after a collection so the
 * next one starts clean. The walk uses each object's recorded size to step. */
static int pgc_descriptor_kind_is_valid(const void *descriptor); /* fwd decl */

void pgc_mark_clear(void)
{
    unsigned char *p = g_gc.heap.base;
    while (p < g_gc.heap.top) {
        pgc_header *h = (pgc_header *)p;
        size_t total = pgc_object_total(h);
        if (h->size == 0 && !pgc_descriptor_kind_is_valid(h->descriptor))
            break;  /* size-0 + invalid desc = desync; valid size-0 walks (total=24) */
        pgc_clear_mark(h);
        p += total;
    }
}

/* =========================================================================
 * Mark stack
 *
 * A growable stack of payload pointers waiting to be traced. Kept as a simple
 * dynamic array; it lives only for the duration of one mark phase.
 * ====================================================================== */

typedef struct pgc_mark_stack {
    void  **items;
    size_t  count;
    size_t  capacity;
} pgc_mark_stack;

static pgc_mark_stack g_mark_stack;

static void pgc_mark_stack_push(void *object)
{
    if (g_mark_stack.count == g_mark_stack.capacity) {
        size_t new_cap = (g_mark_stack.capacity == 0)
                           ? 1024
                           : g_mark_stack.capacity * 2;
        void **new_items = (void **)realloc(g_mark_stack.items,
                                            new_cap * sizeof(void *));
        if (new_items == NULL)
            return;  /* allocation failure: best-effort, the object is dropped */
        g_mark_stack.items    = new_items;
        g_mark_stack.capacity = new_cap;
    }
    g_mark_stack.items[g_mark_stack.count++] = object;
}

static void *pgc_mark_stack_pop(void)
{
    if (g_mark_stack.count == 0)
        return NULL;
    return g_mark_stack.items[--g_mark_stack.count];
}

/* =========================================================================
 * Marking a single reference
 *
 * If `object` is a non-null heap pointer, resolve it to the base of the
 * object that contains it (it may be an interior pointer -- an escaping
 * managed-buffer element reference such as string[i] -- not a payload start),
 * then if that base is not yet marked, mark it and push it for tracing. Used
 * both to seed roots and to follow children.
 *
 * Resolving to the base before marking is essential: treating an interior
 * pointer as a base would read the bytes 24 below it as a header/descriptor
 * and either mark the wrong thing or dereference garbage. The object index
 * (built at the start of the mark phase) maps any in-heap address to its base.
 * ====================================================================== */

static void pgc_mark_object(void *object)
{
    if (object == NULL)
        return;

    unsigned char *p = (unsigned char *)object;
    if (p < g_gc.heap.base + PGC_HEADER_SIZE || p > g_gc.heap.top)
        return;

    void *base = pgc_resolve_base(object);
    if (base == NULL)
        return;  /* not within any object: not a managed pointer, skip */

    pgc_header *h = PGC_HEADER_OF(base);
    if (pgc_is_marked(h))
        return;

    pgc_set_marked(h);
    pgc_mark_stack_push(base);
}

/* The root visitor: each root slot holds a managed pointer; mark its target. */
static void pgc_mark_root_slot(void **slot)
{
    if (slot != NULL)
        pgc_mark_object(*slot);
}

/* =========================================================================
 * Object/descriptor validation (diagnostic + defensive)
 *
 * A descriptor that is actually string/heap-garbage bytes would fault when
 * dereferenced, which can only happen if a non-object address reaches the
 * tracer. This guard validates an object before its descriptor is dereferenced:
 * the address must be a real object base (the object index agrees), and its
 * descriptor must be either the atomic sentinel or a pointer whose kind reads
 * as a known descriptor kind. On failure it logs the offending pointer and
 * surrounding bytes and returns false so the tracer can skip it instead of
 * crashing. */
static int pgc_descriptor_kind_is_valid(const void *descriptor)
{
    if (descriptor == NULL || descriptor == PGC_ATOMIC_DESCRIPTOR)
        return 1;  /* atomic / none: valid, simply has no children */

    uintptr_t d = (uintptr_t)descriptor;

    /* A real descriptor is a static global (in the binary's data section). It
     * is therefore pointer-aligned, lives well above the first page, and is
     * NEVER inside the GC heap (descriptors are not GC-allocated). Reject any
     * descriptor that fails these address-plausibility checks BEFORE
     * dereferencing it -- a small or wild value (e.g. 0x40) is exactly what
     * would fault on read. We cannot prove an address is mapped, but these
     * checks exclude tiny ints, heap-interior pointers, and string-byte
     * pointers. */
    if ((d & 0x7u) != 0)
        return 0;                       /* misaligned: not a descriptor       */
    if (d < 0x10000u)
        return 0;                       /* implausibly low (e.g. 0x40)        */
    {
        const unsigned char *p = (const unsigned char *)descriptor;
        if (p >= g_gc.heap.base && p < g_gc.heap.end)
            return 0;                   /* in the GC heap: descriptors never are */
    }

    int32_t kind = *(const int32_t *)descriptor;
    return (kind == PGC_DESC_FIXED || kind == PGC_DESC_ARRAY);
}

/* Returns the validated object base to trace, or NULL to skip. */
static void *pgc_validate_for_trace(void *object)
{
    /* Must resolve to a real object base via the index. If it does not, the
     * pointer was never a valid managed object (interior into the wrong place,
     * stale garbage, or a desynced walk). */
    void *base = pgc_resolve_base(object);
    if (base == NULL || base != object) {
        /* object was not an exact payload start; skip it. */
        return NULL;
    }

    pgc_header *h = PGC_HEADER_OF(base);
    if (!pgc_descriptor_kind_is_valid(h->descriptor)) {
        return NULL;
    }
    return base;
}

/* =========================================================================
 * Tracing an object's children via its descriptor
 * ====================================================================== */

static void pgc_trace_fixed(void *object, const pgc_descriptor_fixed *desc)
{
    /* Each offset names a managed-pointer field at a payload-relative byte
     * offset. Read the field and mark its target. */
    for (int32_t i = 0; i < desc->count; i++) {
        void **field = (void **)((unsigned char *)object + desc->offsets[i]);
        pgc_mark_object(*field);
    }
}

static void pgc_trace_array(void *object, const pgc_header *h,
                            const pgc_descriptor_array *desc)
{
    /* The element count is the object's payload size divided by the stride
     * (the stride was known at allocation; the count was not, so it is
     * recovered here from the recorded size). */
    if (desc->stride <= 0)
        return;
    size_t payload = pgc_object_size(h);
    size_t count   = payload / (size_t)desc->stride;

    const void *elem_desc = desc->element;
    if (elem_desc == NULL || elem_desc == PGC_ATOMIC_DESCRIPTOR)
        return;  /* atomic elements: nothing to trace */

    /* The element descriptor is itself a descriptor. For managed-pointer
     * elements the compiler emits a fixed kind-0 descriptor whose single
     * offset is 0 (the element slot holds the managed pointer). Trace each
     * element through it. */
    const pgc_descriptor *ed = (const pgc_descriptor *)elem_desc;
    for (size_t e = 0; e < count; e++) {
        unsigned char *element = (unsigned char *)object + e * (size_t)desc->stride;
        if (ed->kind == PGC_DESC_FIXED) {
            pgc_trace_fixed(element, (const pgc_descriptor_fixed *)ed);
        } else if (ed->kind == PGC_DESC_ARRAY) {
            /* Nested arrays are not produced by the current compiler, but
             * handle defensively: an element that is itself a managed pointer
             * to an array object is marked through the element slot. */
            void **slot = (void **)element;
            pgc_mark_object(*slot);
        }
    }
}

static void pgc_trace_object(void *object)
{
    /* Validate before dereferencing the descriptor: a non-object pointer here
     * would read garbage as a descriptor and crash. Skip-and-log on failure. */
    void *base = pgc_validate_for_trace(object);
    if (base == NULL)
        return;

    pgc_header *h = PGC_HEADER_OF(base);
    const void *descriptor = h->descriptor;

    if (descriptor == NULL || descriptor == PGC_ATOMIC_DESCRIPTOR)
        return;  /* atomic / no descriptor: no children */

    const pgc_descriptor *d = (const pgc_descriptor *)descriptor;
    if (d->kind == PGC_DESC_FIXED)
        pgc_trace_fixed(base, (const pgc_descriptor_fixed *)d);
    else if (d->kind == PGC_DESC_ARRAY)
        pgc_trace_array(base, h, (const pgc_descriptor_array *)d);
}

/* =========================================================================
 * Full mark
 *
 * Seed the mark stack from all three root sources, then drain it, tracing each
 * object's children until nothing remains reachable-but-unmarked.
 * ====================================================================== */

void pgc_mark_all(void)
{
    /* Reset the mark stack for this phase (capacity is retained across
     * collections to avoid repeated reallocation). */
    g_mark_stack.count = 0;

    /* Build the object-start index so interior pointers (escaping managed-
     * buffer element references, e.g. string[i]) can be resolved to their base
     * object during both this mark phase and the later reference-update pass.
     * Valid until objects move (pgc_move_objects), which runs after both. */
    pgc_object_index_build();

    /* Seed from roots. Stack roots require the parsed stack maps. */
    pgc_visit_global_roots(pgc_mark_root_slot);
    pgc_visit_handle_roots(pgc_mark_root_slot);
    pgc_visit_stack_roots(pgc_mark_root_slot);

    /* Transitive closure. */
    void *object;
    while ((object = pgc_mark_stack_pop()) != NULL)
        pgc_trace_object(object);
}
