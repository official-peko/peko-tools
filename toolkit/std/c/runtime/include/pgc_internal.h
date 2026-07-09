/*
 * pgc_internal.h
 * Private definitions shared across the collector's translation units.
 *
 * Not part of the public surface. Defines the on-heap object header, the GC
 * type-descriptor formats (which must byte-match what the compiler emits),
 * the heap, thread registry, handle table, pin set, and the global collector
 * state. Every pgc_*.c file includes this; foreign code never does.
 */

#ifndef PGC_INTERNAL_H
#define PGC_INTERNAL_H

#include "pgc.h"

#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* Cross-platform threading primitives we rely on directly. The collector
 * owns its own synchronization rather than depending on a higher-level
 * threading library, so the GC and that library cannot deadlock against each
 * other. */
#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <windows.h>
#else
#  include <pthread.h>
#endif

/* =========================================================================
 * Object header
 *
 * Every managed allocation is laid out as [header][payload]. The allocator
 * reserves PGC_HEADER_SIZE bytes for the header and returns a pointer to the
 * payload; the header sits immediately before that pointer. All managed
 * pointers in the system (and every address the compiler sees) point at the
 * payload, so descriptor offsets are payload-relative.
 *
 * The header is three machine words (24 bytes on a 64-bit target):
 *
 *   word 0  size     - the payload size in bytes, recorded at allocation. The
 *                      collector needs this to step object-to-object when it
 *                      walks the heap linearly during compaction, and no
 *                      descriptor variant encodes the object's own byte size,
 *                      so it is stored here.
 *   word 1  forward  - dual purpose. Outside a collection it carries the mark
 *                      bit in its low bit (the rest is zero for a normal
 *                      object). During the compute-forwarding-address pass it
 *                      is overwritten with the object's post-compaction
 *                      payload address. This is the classic Lisp2 use of a
 *                      header word.
 *   word 2  descriptor - pointer to the object's GC type descriptor, or the
 *                      atomic sentinel (PGC_ATOMIC_DESCRIPTOR) for no-scan
 *                      objects. Used to find the object's managed children.
 *
 * The compiler is unaware of the header: it passes only the payload size to
 * the allocator and assumes the returned pointer is the payload. The header
 * size and layout are therefore entirely the runtime's choice.
 * ====================================================================== */

typedef struct pgc_header {
    uintptr_t   size;        /* payload size in bytes                        */
    uintptr_t   forward;     /* mark bit (low bit) / forwarding payload addr */
    const void *descriptor;  /* PGC_ATOMIC_DESCRIPTOR or a pgc_descriptor*   */
} pgc_header;

#define PGC_HEADER_SIZE   ((size_t)sizeof(pgc_header))

/* Recover the header from a payload pointer and vice versa. */
#define PGC_HEADER_OF(payload)  ((pgc_header *)((unsigned char *)(payload) - PGC_HEADER_SIZE))
#define PGC_PAYLOAD_OF(header)  ((void *)((unsigned char *)(header) + PGC_HEADER_SIZE))

/* The mark bit lives in the low bit of the forward word. Object addresses are
 * always at least 8-byte aligned, so the low bits are free outside of the
 * compute-forwarding pass (which runs only while the world is stopped and the
 * mark bits have already served their purpose). */
#define PGC_MARK_BIT  ((uintptr_t)1)

/* Sentinel descriptor value marking an atomic (no managed children) object.
 * A non-null, non-pointer-aligned constant so it can never collide with a
 * real descriptor address. */
#define PGC_ATOMIC_DESCRIPTOR  ((const void *)(uintptr_t)1)

/* =========================================================================
 * GC type descriptors
 *
 * These layouts MUST byte-match the constants the compiler emits. From the
 * generated IR:
 *
 *   fixed (kind 0):  { i32 kind, i32 count, [count x i64] offsets }
 *   array (kind 1):  { i32 kind, i64 stride, ptr element_descriptor }
 *
 * "kind" selects the variant. For fixed descriptors, "offsets" lists the
 * payload-relative byte offset of each managed-pointer field. For array
 * descriptors, "stride" is the element size in bytes and element_descriptor
 * points at the descriptor used to trace one element (null/atomic when the
 * elements carry no managed pointers); the element count is derived at runtime
 * from the object size and the stride.
 * ====================================================================== */

enum {
    PGC_DESC_FIXED = 0,
    PGC_DESC_ARRAY = 1
};

/* Common prefix: every descriptor begins with a 32-bit kind. Reading the kind
 * through this lets us dispatch before committing to a concrete layout. */
typedef struct pgc_descriptor {
    int32_t kind;
} pgc_descriptor;

/* Fixed-layout descriptor. The flexible array member holds "count" 64-bit
 * payload-relative offsets. Note the i32 kind + i32 count pack into the first
 * 8 bytes, matching the emitted { i32, i32, [n x i64] } with no padding before
 * the i64 array. */
typedef struct pgc_descriptor_fixed {
    int32_t kind;            /* PGC_DESC_FIXED */
    int32_t count;           /* number of managed-pointer offsets */
    int64_t offsets[];       /* payload-relative byte offsets of children */
} pgc_descriptor_fixed;

/* Array-layout descriptor. Matches { i32 kind, i64 stride, ptr element }.
 * The i32 kind is followed by 4 bytes of padding so the i64 stride is
 * 8-byte aligned, which is exactly how the emitted struct is laid out. */
typedef struct pgc_descriptor_array {
    int32_t     kind;        /* PGC_DESC_ARRAY */
    int32_t     _pad;        /* explicit: matches the emitted padding */
    int64_t     stride;      /* element size in bytes */
    const void *element;     /* element descriptor, or NULL/atomic */
} pgc_descriptor_array;

/* =========================================================================
 * Heap
 *
 * Single contiguous managed region. Allocation is a bump of g_heap.top toward
 * g_heap.end; compaction slides live objects down toward g_heap.base and
 * resets top to the end of the compacted region. (Mark-compact uses one
 * region, unlike a copying collector's two semispaces.)
 * ====================================================================== */

typedef struct pgc_heap {
    unsigned char *base;     /* first byte of the managed region          */
    unsigned char *top;      /* next free byte (bump pointer)             */
    unsigned char *end;      /* one past the last byte of the region      */
    size_t         size;     /* end - base                                */
} pgc_heap;

/* The frame record on the supported ABIs is two pointers: the saved frame
 * pointer followed by the return address. The caller's stack pointer at a call
 * site is the address immediately above this record. */
#define PGC_FRAME_RECORD_BYTES (2 * sizeof(void *))

/* =========================================================================
 * Thread registry
 *
 * Every attached thread has an entry. The collector uses these to stop each
 * thread at a safepoint and walk its stack for roots. stack_base is captured
 * at attach (the high end of the thread's stack on common downward-growing
 * stacks); stack_top is the thread's stack pointer at the moment it parked at
 * a safepoint, captured by pgc_enter_safepoint. parked is set while the thread
 * is waiting out a collection. blocking_ret_addr and blocking_caller_sp hold
 * the managed caller's safepoint for a thread parked in a native blocking
 * call, whose begin-blocking frame is gone by collection time;
 * blocking_ret_addr is null for a thread parked at a poll.
 *
 * Each thread also owns a TLAB (thread-local allocation buffer): a chunk
 * carved from the heap that the thread bump-allocates from without locking.
 * When the TLAB is exhausted the thread locks the heap and grabs a fresh
 * chunk. After a collection every TLAB is invalidated (the heap moved under
 * it), so the collector resets all TLABs and threads re-acquire on next use.
 * ====================================================================== */

typedef struct pgc_tlab {
    unsigned char *top;      /* next free byte within the TLAB            */
    unsigned char *end;      /* one past the TLAB's last byte             */
} pgc_tlab;

typedef struct pgc_thread {
    bool             in_use;       /* slot occupied                        */
    void            *stack_base;   /* high end of the thread's stack       */
    void            *stack_top;    /* frame pointer to start the walk from */
    void            *blocking_ret_addr;  /* caller safepoint when blocking  */
    void            *blocking_caller_sp; /* caller sp at the blocking call  */
    atomic_int       parked;       /* 1 while waiting out a collection     */
    pgc_tlab         tlab;         /* thread-local allocation buffer       */
#ifdef _WIN32
    DWORD            os_id;        /* OS thread id                         */
    CONTEXT          win_context;  /* register state to seed the Win64 walk */
#else
    pthread_t        os_handle;    /* OS thread handle                     */
#endif
} pgc_thread;

#define PGC_MAX_THREADS  256

/* =========================================================================
 * Handle table
 *
 * Maps a pgc_handle (integer) to a managed object pointer. Live entries are
 * roots; the collector updates entry targets when objects move, so the
 * integer a foreign caller holds stays valid across collections. A free list
 * threads through unused slots. Index 0 is reserved as PGC_NULL_HANDLE.
 * ====================================================================== */

typedef struct pgc_handle_entry {
    void    *object;     /* managed object, or NULL when free            */
    uint32_t next_free;  /* next free slot index when this slot is free  */
} pgc_handle_entry;

typedef struct pgc_handle_table {
    pgc_handle_entry *entries;
    uint32_t          capacity;
    uint32_t          free_head;  /* head of the free list, 0 if empty    */
} pgc_handle_table;

/* =========================================================================
 * Pin set
 *
 * Tracks pinned objects and their nesting counts. Pinned objects are excluded
 * from movement during compaction. Kept small and simple: pins are expected to
 * be few and short-lived.
 * ====================================================================== */

typedef struct pgc_pin_entry {
    void    *object;
    uint32_t count;      /* nesting depth; 0 means the slot is free       */
} pgc_pin_entry;

typedef struct pgc_pin_set {
    pgc_pin_entry *entries;
    uint32_t       capacity;
} pgc_pin_set;

/* =========================================================================
 * Global roots
 *
 * A growable array of slot addresses (void**). Each slot holds a managed
 * pointer the collector reads and, on a move, writes back.
 * ====================================================================== */

typedef struct pgc_root_set {
    void   ***slots;     /* array of slot addresses                       */
    size_t    count;
    size_t    capacity;
} pgc_root_set;

/* =========================================================================
 * Global collector state
 *
 * One instance, defined in pgc.c. Guarded by g_gc.lock for any structural
 * mutation. The collection flag (pgc_collection_requested, defined in
 * threads.c) and per-thread parked flags are atomics read on the hot path.
 * ====================================================================== */

typedef struct pgc_state {
    pgc_heap         heap;
    pgc_root_set     roots;
    pgc_handle_table handles;
    pgc_pin_set      pins;

    pgc_thread       threads[PGC_MAX_THREADS];
    int              thread_count;

    bool             initialized;

#ifdef _WIN32
    CRITICAL_SECTION lock;       /* protects structural mutations         */
    CONDITION_VARIABLE resume;   /* collector -> parked threads           */
    CONDITION_VARIABLE parked_cv;/* parked threads -> collector           */
#else
    pthread_mutex_t  lock;
    pthread_cond_t   resume;
    pthread_cond_t   parked_cv;
#endif
} pgc_state;

/* The single global collector instance. */
extern pgc_state g_gc;

/* =========================================================================
 * Internal cross-file API
 *
 * Names are deliberately distinct from the public pgc_* surface where they
 * are not the same operation. These are the seams between the .c files.
 * ====================================================================== */

/* heap.c: raw heap placement and the size of an object from its header. */
int pgc_heap_create(size_t heap_bytes);              /* reserve the region    */
void pgc_heap_destroy(void);                         /* release the region    */
unsigned char *pgc_heap_bump(size_t total_bytes);   /* lock held by caller */
size_t pgc_object_size(const pgc_header *header);    /* payload size in bytes */
size_t pgc_object_total(const pgc_header *header);   /* header + payload      */
void   pgc_fill_gap(unsigned char *gap_start, unsigned char *gap_end);
                                                     /* fill a TLAB gap with an
                                                        atomic filler object   */
/* heap.c: object-start index for interior-pointer resolution.
 *
 * Roots and references are not guaranteed to point at an object's payload
 * start: a managed-buffer element reference (e.g. string[i], or an array
 * element pointer that escapes its access) is an INTERIOR pointer into the
 * object. The collector must resolve such a pointer back to the base object so
 * it can mark and relocate it, preserving the interior offset across a move.
 *
 * pgc_object_index_build walks the heap once (world stopped) and records every
 * object's payload start in address order. pgc_resolve_base maps an arbitrary
 * in-heap address (base, interior, or one-past-end) to the payload start of the
 * object that contains it, or NULL if it is not within any object. The index
 * is valid from the build until objects move (pgc_move_objects); marking and
 * update_references both run before the move, so one build per cycle suffices.
 * pgc_object_index_free releases it. */
void  pgc_object_index_build(void);
void  pgc_object_index_free(void);
void *pgc_resolve_base(void *addr);   /* interior addr -> base payload, or NULL */

/* heap.c: the three heap-walking passes of mark-compact (mark is in mark.c).
 * The world must be stopped for all three. */
void pgc_compute_forwarding(void);
void pgc_update_references(void);
void pgc_move_objects(void);

/* mark.c: mark every reachable object starting from all root sources. */
void pgc_mark_all(void);
void pgc_mark_clear(void);                 /* clear all mark bits           */
bool pgc_is_marked(const pgc_header *h);
void pgc_set_marked(pgc_header *h);

/* audit.c: read-only debugging passes, world stopped. pgc_audit validates heap
 * structure; pgc_verify_mark conservatively cross-checks the precise mark for
 * missed roots/fields. Both gated by env vars in pgc_collect. */
int pgc_audit(void);
int pgc_verify_mark(unsigned long generation);

/* roots.c: enumerate root sources by invoking a visitor on each slot.
 * The visitor may read and rewrite the slot (for pointer updating). */
typedef void (*pgc_root_visitor)(void **slot);
void pgc_visit_global_roots(pgc_root_visitor visit);
void pgc_visit_handle_roots(pgc_root_visitor visit);
bool pgc_is_pinned(const void *object);

/* stackmap.c: parse the merged __LLVM_StackMaps once, and enumerate the
 * precise stack roots of every stopped thread. */
void pgc_stackmap_init(void);
void pgc_stackmap_parse(const uint8_t *base);   /* parse a supplied table */
size_t pgc_stackmap_count(void);                /* parsed record count    */
void pgc_visit_stack_roots(pgc_root_visitor visit);

/* threads.c: stop-the-world coordination used by the collection driver. */
void pgc_stop_the_world(void);   /* request + wait until all others parked */
void pgc_start_the_world(void);  /* clear flag + wake parked threads       */
void pgc_reset_all_tlabs(void);  /* invalidate every TLAB after compaction */
void pgc_fill_all_tlab_gaps(void); /* fill current TLAB tails before walking  */
/* threads.c: the calling thread's registry entry, or NULL when the thread has
 * not attached. Used by the allocator to reach the thread's TLAB. */
pgc_thread *pgc_current_thread(void);

/* Lock helpers (thin wrappers, defined in pgc.c). */
void pgc_lock(void);
void pgc_unlock(void);

#endif /* PGC_INTERNAL_H */
