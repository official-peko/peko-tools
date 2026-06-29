/*
 * compiler_abi.c
 * Thin adapter exposing the exact symbols the Pekoscript compiler emits, each
 * mapping onto the clean internal pgc_* API. Keeping the compiler-facing names
 * quarantined in this one file means the rest of the runtime uses only the
 * pgc_* surface.
 *
 * The compiler emits calls to (verified in the generated IR):
 *   peko_gc_alloc_object(descriptor, size)  - traced allocation
 *   peko_gc_alloc(size)                      - raw managed (atomic) allocation
 *   peko_gc_add_global_root(slot)            - register a global root slot
 *   peko_gc_write_barrier(slot, value)       - record a managed-pointer store
 *
 * It also references two safepoint symbols directly, which the runtime defines
 * elsewhere and are therefore not shimmed here:
 *   pgc_collection_requested  (global flag, defined in threads.c)
 *   pgc_enter_safepoint()     (park primitive, defined in threads.c)
 *
 * Sizes from the compiler are payload sizes (the object's own bytes); the
 * runtime adds the header. The compiler passes them as 32-bit ints, matching
 * the i32 size argument in the emitted statepoints.
 */

#include "./include/pgc.h"

/* =========================================================================
 * Allocation
 * ====================================================================== */

void *peko_gc_alloc_object(const void *descriptor, int size)
{
    return pgc_alloc_managed(descriptor, (size_t)(unsigned int)size);
}

void *peko_gc_alloc(int size)
{
    return pgc_alloc_atomic((size_t)(unsigned int)size);
}

/* =========================================================================
 * Global roots
 *
 * The compiler passes the address of the global variable (the slot). The
 * collector reads and, on a move, rewrites that slot, so the slot address is
 * exactly what pgc_add_root wants.
 * ====================================================================== */

void peko_gc_add_global_root(void *slot)
{
    pgc_add_root((void **)slot);
}

/* =========================================================================
 * Write barrier
 *
 * Both arguments arrive as raw (address-space-0) pointers; the compiler
 * addrspacecasts the managed slot and value down before the call. The
 * stop-the-world mark-compact collector does not need per-store remembered-set
 * bookkeeping (it rescans all roots and live objects each cycle), so the
 * barrier is a no-op today. The symbol is part of the stable ABI and is kept
 * so generated code links unchanged; a future generational or concurrent
 * collector would record the store here.
 * ====================================================================== */

void peko_gc_write_barrier(void *slot, void *value)
{
    (void)slot;
    (void)value;
}
