/*
 * roots.c
 * Three related subsystems the collector reads as root sources, plus the
 * pin set that the compactor honors:
 *
 *   global roots - slot addresses (void**) outside the heap that hold managed
 *                  pointers. The collector reads each slot to find the root
 *                  and writes it back when the target moves.
 *   handles      - integer-named, stable references to managed objects for
 *                  foreign code. Live handle targets are roots; the table is
 *                  updated when objects move so the integer stays valid.
 *   pins         - objects the compactor must not move, with nesting counts.
 *
 * All structural mutation here takes the global lock. Enumeration (the
 * pgc_visit_* functions) runs only while the world is stopped, so it does not
 * lock; the visitor may both read and rewrite each slot, which is how the
 * pointer-update compaction pass relocates roots.
 */

#include "./include/pgc_internal.h"

#include <stdlib.h>
#include <string.h>

/* =========================================================================
 * Global root registry
 *
 * A growable array of slot addresses. Registering the slot address (not the
 * value) is what lets the collector rewrite the global after compaction.
 * ====================================================================== */

#define PGC_ROOTS_INITIAL_CAPACITY  64

static int pgc_roots_grow(void)
{
    size_t new_cap = (g_gc.roots.capacity == 0)
                       ? PGC_ROOTS_INITIAL_CAPACITY
                       : g_gc.roots.capacity * 2;
    void ***new_slots = (void ***)realloc(g_gc.roots.slots,
                                          new_cap * sizeof(void **));
    if (new_slots == NULL)
        return 0;
    g_gc.roots.slots    = new_slots;
    g_gc.roots.capacity = new_cap;
    return 1;
}

void pgc_add_root(void **slot)
{
    if (slot == NULL)
        return;

    pgc_lock();

    if (g_gc.roots.count == g_gc.roots.capacity) {
        if (!pgc_roots_grow()) {
            pgc_unlock();
            return;
        }
    }
    g_gc.roots.slots[g_gc.roots.count++] = slot;

    pgc_unlock();
}

void pgc_remove_root(void **slot)
{
    if (slot == NULL)
        return;

    pgc_lock();

    for (size_t i = 0; i < g_gc.roots.count; i++) {
        if (g_gc.roots.slots[i] == slot) {
            /* Swap-remove: move the last entry into this slot. Order does not
             * matter for a root set. */
            g_gc.roots.slots[i] = g_gc.roots.slots[g_gc.roots.count - 1];
            g_gc.roots.count--;
            break;
        }
    }

    pgc_unlock();
}

void pgc_visit_global_roots(pgc_root_visitor visit)
{
    /* Called only while the world is stopped; no lock needed. The visitor may
     * rewrite the slot's contents (used by the pointer-update pass). */
    for (size_t i = 0; i < g_gc.roots.count; i++)
        visit(g_gc.roots.slots[i]);
}

/* =========================================================================
 * Handle table
 *
 * Maps a pgc_handle (integer index) to a managed object. Index 0 is reserved
 * as PGC_NULL_HANDLE. Free slots are threaded into a free list via the
 * entry's next_free field; the head is g_gc.handles.free_head (0 if empty,
 * since index 0 can never be a real free slot).
 * ====================================================================== */

#define PGC_HANDLES_INITIAL_CAPACITY  64

static int pgc_handles_grow(void)
{
    uint32_t old_cap = g_gc.handles.capacity;
    uint32_t new_cap = (old_cap == 0)
                         ? PGC_HANDLES_INITIAL_CAPACITY
                         : old_cap * 2;

    pgc_handle_entry *new_entries =
        (pgc_handle_entry *)realloc(g_gc.handles.entries,
                                    new_cap * sizeof(pgc_handle_entry));
    if (new_entries == NULL)
        return 0;

    g_gc.handles.entries  = new_entries;
    g_gc.handles.capacity = new_cap;

    /* Index 0 is reserved as the null handle on first growth. */
    uint32_t first = old_cap;
    if (old_cap == 0) {
        g_gc.handles.entries[0].object    = NULL;
        g_gc.handles.entries[0].next_free = 0;
        first = 1;
    }

    /* Thread the new slots onto the free list, lowest index at the head so
     * handles are handed out in a stable order. */
    for (uint32_t i = first; i < new_cap; i++) {
        g_gc.handles.entries[i].object    = NULL;
        g_gc.handles.entries[i].next_free = g_gc.handles.free_head;
        g_gc.handles.free_head            = i;
    }
    return 1;
}

pgc_handle pgc_handle_create(void *object)
{
    if (object == NULL)
        return PGC_NULL_HANDLE;

    pgc_lock();

    if (g_gc.handles.free_head == 0) {
        if (!pgc_handles_grow() || g_gc.handles.free_head == 0) {
            pgc_unlock();
            return PGC_NULL_HANDLE;
        }
    }

    uint32_t index = g_gc.handles.free_head;
    g_gc.handles.free_head = g_gc.handles.entries[index].next_free;
    g_gc.handles.entries[index].object    = object;
    g_gc.handles.entries[index].next_free = 0;

    pgc_unlock();
    return (pgc_handle)index;
}

void *pgc_handle_get(pgc_handle handle)
{
    if (handle == PGC_NULL_HANDLE)
        return NULL;

    pgc_lock();

    void *object = NULL;
    if (handle < g_gc.handles.capacity)
        object = g_gc.handles.entries[handle].object;

    pgc_unlock();
    return object;
}

void pgc_handle_release(pgc_handle handle)
{
    if (handle == PGC_NULL_HANDLE)
        return;

    pgc_lock();

    if (handle < g_gc.handles.capacity &&
        g_gc.handles.entries[handle].object != NULL) {
        g_gc.handles.entries[handle].object    = NULL;
        g_gc.handles.entries[handle].next_free = g_gc.handles.free_head;
        g_gc.handles.free_head                 = handle;
    }

    pgc_unlock();
}

void pgc_visit_handle_roots(pgc_root_visitor visit)
{
    /* Called only while the world is stopped; no lock needed. Each live
     * entry's object field is treated as a root slot: the visitor reads it to
     * mark/relocate and may rewrite it, which is how a live handle's target
     * is updated when the object moves. */
    for (uint32_t i = 1; i < g_gc.handles.capacity; i++) {
        if (g_gc.handles.entries[i].object != NULL)
            visit(&g_gc.handles.entries[i].object);
    }
}

/* =========================================================================
 * Pin set
 *
 * Objects the compactor must leave in place, with nesting counts. Linear
 * scan: pins are expected to be few and short-lived, so a small array beats
 * the overhead of a hash structure here.
 * ====================================================================== */

#define PGC_PINS_INITIAL_CAPACITY  16

static pgc_pin_entry *pgc_pin_find(const void *object)
{
    for (uint32_t i = 0; i < g_gc.pins.capacity; i++) {
        if (g_gc.pins.entries[i].count > 0 &&
            g_gc.pins.entries[i].object == object)
            return &g_gc.pins.entries[i];
    }
    return NULL;
}

static pgc_pin_entry *pgc_pin_find_free(void)
{
    for (uint32_t i = 0; i < g_gc.pins.capacity; i++) {
        if (g_gc.pins.entries[i].count == 0)
            return &g_gc.pins.entries[i];
    }
    return NULL;
}

static int pgc_pins_grow(void)
{
    uint32_t old_cap = g_gc.pins.capacity;
    uint32_t new_cap = (old_cap == 0)
                         ? PGC_PINS_INITIAL_CAPACITY
                         : old_cap * 2;

    pgc_pin_entry *new_entries =
        (pgc_pin_entry *)realloc(g_gc.pins.entries,
                                 new_cap * sizeof(pgc_pin_entry));
    if (new_entries == NULL)
        return 0;

    for (uint32_t i = old_cap; i < new_cap; i++) {
        new_entries[i].object = NULL;
        new_entries[i].count  = 0;
    }
    g_gc.pins.entries  = new_entries;
    g_gc.pins.capacity = new_cap;
    return 1;
}

void *pgc_pin(void *object)
{
    if (object == NULL)
        return NULL;

    pgc_lock();

    pgc_pin_entry *entry = pgc_pin_find(object);
    if (entry != NULL) {
        entry->count++;
    } else {
        entry = pgc_pin_find_free();
        if (entry == NULL) {
            if (!pgc_pins_grow()) {
                pgc_unlock();
                return NULL;
            }
            entry = pgc_pin_find_free();
        }
        entry->object = object;
        entry->count  = 1;
    }

    pgc_unlock();

    /* While pinned the object will not move, so its current address is stable
     * and safe to hand to foreign code. */
    return object;
}

void pgc_unpin(void *object)
{
    if (object == NULL)
        return;

    pgc_lock();

    pgc_pin_entry *entry = pgc_pin_find(object);
    if (entry != NULL && entry->count > 0) {
        entry->count--;
        if (entry->count == 0)
            entry->object = NULL;  /* slot becomes free */
    }

    pgc_unlock();
}

bool pgc_is_pinned(const void *object)
{
    /* Queried by the compactor while the world is stopped; no lock needed. */
    return pgc_pin_find(object) != NULL;
}
