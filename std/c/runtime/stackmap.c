/*
 * stackmap.c
 * Parser for the LLVM Stack Map (version 3) section emitted by
 * RewriteStatepointsForGC, and the precise stack-root walk it drives.
 *
 * At link time the linker merges every object file's stack map section into
 * one. On ELF its start is exposed as the symbol __LLVM_StackMaps; on Mach-O
 * it is the contents of the __llvm_stackmaps section, located at run time via
 * getsectiondata (see pgc_stackmap_init). We parse that table
 * once into an in-memory index keyed by return address, so that at collection
 * time, for each stopped thread, we can walk its frames and, at each frame's
 * return address, find exactly which stack slots and registers hold live
 * managed pointers.
 *
 * Format (LLVM StackMaps, v3):
 *
 *   Header:    u8 version=3, u8 rsvd, u16 rsvd,
 *              u32 NumFunctions, u32 NumConstants, u32 NumRecords
 *   Functions: { u64 address, u64 stackSize, u64 recordCount } x NumFunctions
 *   Constants: { u64 value } x NumConstants
 *   Records:   { u64 patchpointID, u32 instructionOffset, u16 rsvd,
 *                u16 numLocations,
 *                Location x numLocations,
 *                <align 8>, u16 padding, u16 numLiveOuts,
 *                LiveOut x numLiveOuts, <align 8> } x NumRecords
 *   Location:  { u8 type, u8 rsvd, u16 size, u16 dwarfReg, u16 rsvd, i32 offset }
 *   LiveOut:   { u16 dwarfReg, u8 rsvd, u8 sizeBytes }
 *
 * Location types: 1 Register, 2 Direct, 3 Indirect, 4 Constant, 5 ConstIndex.
 * For GC roots the relevant cases are Indirect (a pointer spilled to a stack
 * slot, addressed as [reg + offset]) and Register (a pointer live in a
 * register at the safepoint). Direct encodes an address-of computed value.
 */

/* glibc declares dl_iterate_phdr and defines struct dl_phdr_info in <link.h>
 * only when _GNU_SOURCE is defined before any system header is included. This
 * must precede every include below. It is harmless on macOS and Windows, whose
 * headers do not key off it for the symbols this file uses. */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include "./include/pgc_internal.h"

#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

/* Image slide applied to the function addresses stored in the stack map. The
 * stack map stores link-time addresses. On a position-independent image the
 * code runs at a slid address, and the live return addresses on the stack
 * carry that slide. This value converts the stored addresses to the addresses
 * seen at run time. It is set in pgc_stackmap_init and is zero for a buffer
 * supplied directly by a test. */
static intptr_t g_sm_slide = 0;

/* Byte length of the stack map section when the platform can report it
 * (Mach-O getsectiondata, Windows PE section header). Zero means the length is
 * unknown (the ELF weak symbol path), in which case table iteration is bounded
 * by header validation and the cursor guard instead of a hard end. */
static size_t g_sm_section_len = 0;

/*
 * Locating the stack map at runtime is platform-specific.
 *
 * On ELF (Linux), the linker emits the merged stack map with a real, linkable
 * symbol __LLVM_StackMaps at its head, so we can reference it directly via an
 * extern declaration and take its address.
 *
 * On Mach-O (macOS), the stack map is emitted as the contents of a section
 * (__llvm_stackmaps in the __LLVM_STACKMAPS segment) but is NOT exposed as a
 * linkable symbol of that name -- an `extern __LLVM_StackMaps` therefore fails
 * to link ("Undefined symbols: ___LLVM_StackMaps"). Instead we look the section
 * up at run time from the main executable's Mach-O header with
 * getsectiondata(), which also yields the section size for free.
 */
#if defined(__APPLE__)
#include <mach-o/getsect.h>
#include <mach-o/ldsyms.h> /* _mh_execute_header */
#include <mach-o/dyld.h>   /* _dyld_get_image_vmaddr_slide */
#elif defined(_WIN32)
/* windows.h is included through pgc_internal.h. The stack map is a PE section
 * whose 15-character name .llvm_stackmaps does not fit the 8-byte section name
 * field, so the image holds the truncation .llvm_st. It is located at run time
 * by walking the main module's section table. */
#else
/* The symbol the ELF linker synthesizes at the head of the merged stack map
 * section. Declared as a byte so we can take its address; the bytes are the
 * table. Weak so a build with no statepoints (no section) links cleanly. */
extern const uint8_t __LLVM_StackMaps __attribute__((weak));
/* dl_iterate_phdr walks the loaded program headers so the segment containing
 * the stack map symbol can be found and its end used as a safe upper bound for
 * table iteration. ELF exposes no length for the section itself. */
#include <link.h>
#endif

/* =========================================================================
 * Location types (LLVM StackMaps)
 * ====================================================================== */

enum {
    PGC_LOC_REGISTER  = 1,
    PGC_LOC_DIRECT    = 2,
    PGC_LOC_INDIRECT  = 3,
    PGC_LOC_CONSTANT  = 4,
    PGC_LOC_CONSTIDX  = 5
};

typedef struct pgc_sm_location {
    uint8_t  type;
    uint16_t size;
    uint16_t dwarf_reg;
    int32_t  offset;
} pgc_sm_location;

/* One safepoint record, indexed by its absolute return address. */
typedef struct pgc_sm_record {
    uint64_t         return_address;  /* function address + instruction off  */
    uint64_t         patchpoint_id;   /* the statepoint ID                   */
    uint16_t         num_locations;
    pgc_sm_location *locations;       /* owned array                         */
} pgc_sm_record;

/* The parsed table: a flat array of records, sorted by return address so a
 * lookup at collection time is a binary search. */
typedef struct pgc_stackmap {
    pgc_sm_record *records;
    size_t         count;
    size_t         capacity;
    bool           parsed;
} pgc_stackmap;

static pgc_stackmap g_sm;

size_t pgc_stackmap_count(void)
{
    return g_sm.count;
}

/* =========================================================================
 * Little-endian cursor over the section bytes
 * ====================================================================== */

typedef struct pgc_cursor {
    const uint8_t *p;
    const uint8_t *end;
} pgc_cursor;

static uint8_t cur_u8(pgc_cursor *c)
{
    if (c->p + 1 > c->end) return 0;
    return *c->p++;
}

static uint16_t cur_u16(pgc_cursor *c)
{
    if (c->p + 2 > c->end) return 0;
    uint16_t v = (uint16_t)c->p[0] | ((uint16_t)c->p[1] << 8);
    c->p += 2;
    return v;
}

static uint32_t cur_u32(pgc_cursor *c)
{
    if (c->p + 4 > c->end) return 0;
    uint32_t v = (uint32_t)c->p[0] | ((uint32_t)c->p[1] << 8) |
                 ((uint32_t)c->p[2] << 16) | ((uint32_t)c->p[3] << 24);
    c->p += 4;
    return v;
}

static uint64_t cur_u64(pgc_cursor *c)
{
    if (c->p + 8 > c->end) return 0;
    uint64_t v = 0;
    for (int i = 0; i < 8; i++)
        v |= (uint64_t)c->p[i] << (8 * i);
    c->p += 8;
    return v;
}

static int32_t cur_i32(pgc_cursor *c)
{
    return (int32_t)cur_u32(c);
}

static void cur_align8(pgc_cursor *c, const uint8_t *base)
{
    size_t off = (size_t)(c->p - base);
    size_t rem = off & 7u;
    if (rem != 0)
        c->p += (8 - rem);
}

/* =========================================================================
 * Record sorting + lookup
 * ====================================================================== */

static int pgc_sm_cmp(const void *a, const void *b)
{
    uint64_t ra = ((const pgc_sm_record *)a)->return_address;
    uint64_t rb = ((const pgc_sm_record *)b)->return_address;
    if (ra < rb) return -1;
    if (ra > rb) return 1;
    return 0;
}

static const pgc_sm_record *pgc_sm_lookup(uint64_t return_address)
{
    size_t lo = 0, hi = g_sm.count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        uint64_t r = g_sm.records[mid].return_address;
        if (r == return_address)
            return &g_sm.records[mid];
        if (r < return_address)
            lo = mid + 1;
        else
            hi = mid;
    }
    return NULL;
}

/* =========================================================================
 * Parse the table once
 *
 * Two passes are not needed; we build records in one forward pass. Function
 * stack-size records are consumed to pair each function with its address (the
 * records that follow carry only an instruction offset relative to the
 * function start, so we add the function address to recover an absolute
 * return address). LLVM emits records grouped by function in the same order as
 * the function records, with each function's recordCount telling how many
 * records belong to it.
 * ====================================================================== */

#if !defined(__APPLE__) && !defined(_WIN32)
/* Find the end of the loaded segment that contains `target`. ELF gives no
 * length for the stack map section, so the parser would otherwise have to read
 * until it found a byte pattern that is not a valid table header, which can
 * step into an unmapped page just past the section. Bounding iteration to the
 * end of the mapped segment keeps every probe inside mapped memory: bytes past
 * the real tables are segment padding, which parse as an invalid header and
 * stop the loop safely. */
typedef struct {
    const uint8_t *target;
    const uint8_t *segment_end;
} pgc_phdr_query;

static int pgc_find_segment_end(struct dl_phdr_info *info, size_t size, void *data)
{
    (void)size;
    pgc_phdr_query *q = (pgc_phdr_query *)data;
    for (int i = 0; i < info->dlpi_phnum; i++) {
        const ElfW(Phdr) *ph = &info->dlpi_phdr[i];
        if (ph->p_type != PT_LOAD)
            continue;
        const uint8_t *seg_start =
            (const uint8_t *)(info->dlpi_addr + ph->p_vaddr);
        const uint8_t *seg_end = seg_start + ph->p_memsz;
        if (q->target >= seg_start && q->target < seg_end) {
            q->segment_end = seg_end;
            return 1;  /* Stop iteration: found the containing segment. */
        }
    }
    return 0;
}
#endif

void pgc_stackmap_init(void)
{
    if (g_sm.parsed)
        return;

    const uint8_t *base = NULL;

#if defined(__APPLE__)
    /* Look the stack map section up in the main executable's Mach-O image.
     * getsectiondata returns NULL if the section is absent (e.g. a build with
     * no statepoints), in which case the parser bails gracefully and only the
     * global and handle roots are scanned. */
    unsigned long sm_size = 0;
    base = getsectiondata(&_mh_execute_header, "__LLVM_STACKMAPS",
                          "__llvm_stackmaps", &sm_size);
    if (base == NULL || sm_size == 0) {
        g_sm.parsed = true;
        return;
    }
    g_sm_section_len = (size_t)sm_size;
    /* The Mach-O stack map section stores link-time function addresses that
     * the loader does not rebase. Find the main image and record its slide so
     * the parser can convert those addresses to run-time addresses. */
    {
        uint32_t image_count = _dyld_image_count();
        for (uint32_t i = 0; i < image_count; i++) {
            if (_dyld_get_image_header(i) ==
                (const struct mach_header *)&_mh_execute_header) {
                g_sm_slide = _dyld_get_image_vmaddr_slide(i);
                break;
            }
        }
    }
#elif defined(_WIN32)
    /* Locate the stack map section in the loaded main module by walking its PE
     * section table. The module handle is the image base. The section name in
     * the image is the 8-byte truncation .llvm_st of .llvm_stackmaps. A build
     * with no statepoints has no such section, so the parser bails gracefully
     * and only the global and handle roots are scanned. */
    {
        HMODULE module = GetModuleHandleW(NULL);
        if (module == NULL) {
            g_sm.parsed = true;
            return;
        }
        const unsigned char *image = (const unsigned char *)module;
        const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)image;
        const IMAGE_NT_HEADERS *nt =
            (const IMAGE_NT_HEADERS *)(image + dos->e_lfanew);
        const IMAGE_SECTION_HEADER *sec = IMAGE_FIRST_SECTION(nt);
        for (WORD i = 0; i < nt->FileHeader.NumberOfSections; i++) {
            if (memcmp(sec[i].Name, ".llvm_st", 8) == 0) {
                base = image + sec[i].VirtualAddress;
                /* VirtualSize is the section's byte length in memory. Use it as
                 * the hard upper bound for table iteration. SizeOfRawData can
                 * be larger due to file alignment, so VirtualSize is the
                 * correct in-memory extent. */
                g_sm_section_len = (size_t)sec[i].Misc.VirtualSize;
                break;
            }
        }
        if (base == NULL) {
            g_sm.parsed = true;
            return;
        }
    }
#else
    base = &__LLVM_StackMaps;
    /* A target whose linker does not define __LLVM_StackMaps leaves this weak
     * symbol null. Bail so the parser does not read from a null address; only
     * the global and handle roots are scanned. */
    if (base == NULL) {
        g_sm.parsed = true;
        return;
    }
    /* ELF exposes no section length. Bound iteration to the end of the mapped
     * segment that holds the section so a probe past the last table lands in
     * mapped segment padding (which parses as an invalid header and stops the
     * loop) rather than an unmapped page. If the segment cannot be found the
     * length stays zero and the driver falls back to its guard window. */
    {
        pgc_phdr_query q;
        q.target = base;
        q.segment_end = NULL;
        dl_iterate_phdr(pgc_find_segment_end, &q);
        if (q.segment_end != NULL && q.segment_end > base)
            g_sm_section_len = (size_t)(q.segment_end - base);
    }
#endif

    pgc_stackmap_parse(base);
}

/* Append space for `extra` records to g_sm.records, growing the backing array.
 * Returns the index at which the caller should begin writing, or SIZE_MAX on
 * allocation failure. The array grows by doubling so repeated table appends do
 * not reallocate on every record. */
static size_t sm_reserve_records(size_t extra)
{
    if (extra == 0)
        return g_sm.count;

    size_t needed = g_sm.count + extra;
    /* Guard against overflow in the addition above. */
    if (needed < g_sm.count)
        return SIZE_MAX;

    if (needed > g_sm.capacity) {
        size_t new_cap = (g_sm.capacity == 0) ? needed : g_sm.capacity;
        while (new_cap < needed) {
            size_t doubled = new_cap * 2;
            if (doubled < new_cap) {   /* overflow: clamp to exactly needed */
                new_cap = needed;
                break;
            }
            new_cap = doubled;
        }
        pgc_sm_record *grown = (pgc_sm_record *)realloc(
            g_sm.records, new_cap * sizeof(pgc_sm_record));
        if (grown == NULL)
            return SIZE_MAX;
        g_sm.records  = grown;
        g_sm.capacity = new_cap;
    }

    size_t start = g_sm.count;
    g_sm.count = needed;
    return start;
}

/* Parse one stack map (version 3) table beginning at `table`. `base` is the
 * start of the whole section, used as the alignment origin (LLVM aligns record
 * padding relative to the section start; since each table is a whole number of
 * eight byte units, aligning relative to the section start and relative to the
 * table start are equivalent). `limit` is one past the last readable byte of
 * the section. Records are appended to g_sm. Returns a pointer to the first
 * byte past this table on success, or NULL if no valid table is present at
 * `table` (bad version, truncated header, or counts that do not fit before
 * `limit`). On a parse that begins validly but runs out of bytes midway the
 * function returns NULL and leaves any partially appended records in place;
 * the driver stops in that case. */
static const uint8_t *parse_one_table(const uint8_t *table,
                                      const uint8_t *base,
                                      const uint8_t *limit)
{
    /* A complete header is eight bytes: version, reserved, reserved16, then
     * three uint32 counts. Reject anything that cannot hold a header. */
    if (table == NULL || limit == NULL || table + 8 + 12 > limit)
        return NULL;

    pgc_cursor c;
    c.p   = table;
    c.end = limit;

    uint8_t version = cur_u8(&c);
    (void)cur_u8(&c);
    (void)cur_u16(&c);
    if (version != 3)
        return NULL;   /* Not a table here: stop. */

    uint32_t num_functions = cur_u32(&c);
    uint32_t num_constants = cur_u32(&c);
    uint32_t num_records   = cur_u32(&c);

    /* Bound the fixed-size portion (function records and constants) against the
     * remaining bytes before reading them. Each StkSizeRecord is 24 bytes and
     * each constant is 8. This rejects a spurious header whose counts would run
     * off the end. The product is computed in 64 bits so it cannot overflow
     * when size_t is 32 bits. */
    {
        uint64_t remaining = (uint64_t)(limit - c.p);
        uint64_t fixed = (uint64_t)num_functions * 24u + (uint64_t)num_constants * 8u;
        if (fixed > remaining)
            return NULL;
    }

    /* Read function records: address, stackSize, recordCount. */
    uint64_t *fn_addr  = (uint64_t *)calloc(num_functions, sizeof(uint64_t));
    uint64_t *fn_count = (uint64_t *)calloc(num_functions, sizeof(uint64_t));
    if (num_functions != 0 && (fn_addr == NULL || fn_count == NULL)) {
        free(fn_addr); free(fn_count);
        return NULL;
    }
    for (uint32_t i = 0; i < num_functions; i++) {
        /* The loader rebases these function-address fields to run-time
         * addresses, so they are used directly. */
        fn_addr[i]  = cur_u64(&c);
        (void)cur_u64(&c);            /* stackSize: not needed for root finding */
        fn_count[i] = cur_u64(&c);
    }

    /* Skip the constants pool. */
    for (uint32_t i = 0; i < num_constants; i++)
        (void)cur_u64(&c);

    /* Reserve space for this table's records, appended after any already
     * parsed from earlier tables. */
    size_t base_index = sm_reserve_records((size_t)num_records);
    if (base_index == SIZE_MAX) {
        free(fn_addr); free(fn_count);
        return NULL;
    }

    /* Walk records, attributing each to its function (in order) so we can turn
     * the per-record instruction offset into an absolute return address. */
    uint32_t fn_index = 0;
    uint64_t fn_remaining = (num_functions > 0) ? fn_count[0] : 0;
    /* Advance past functions that have zero records. */
    while (fn_index < num_functions && fn_remaining == 0) {
        fn_index++;
        fn_remaining = (fn_index < num_functions) ? fn_count[fn_index] : 0;
    }

    for (uint32_t r = 0; r < num_records; r++) {
        /* A record header is sixteen bytes before its locations. If the cursor
         * cannot read it the table is truncated. Discard the records reserved
         * for this table (the entries from base_index onward were not all
         * filled, and realloc does not zero them) by rolling the count back,
         * then stop. */
        if (c.p + 16 > c.end) {
            for (uint32_t k = 0; k < r; k++)
                free(g_sm.records[base_index + k].locations);
            g_sm.count = base_index;
            free(fn_addr); free(fn_count);
            return NULL;
        }

        uint64_t patchpoint = cur_u64(&c);
        uint32_t insn_off   = cur_u32(&c);
        (void)cur_u16(&c);                 /* reserved (record flags) */
        uint16_t num_loc    = cur_u16(&c);

        pgc_sm_record *rec = &g_sm.records[base_index + r];
        rec->patchpoint_id = patchpoint;
        rec->num_locations = num_loc;
        rec->locations = (num_loc > 0)
            ? (pgc_sm_location *)calloc(num_loc, sizeof(pgc_sm_location))
            : NULL;

        uint64_t func_address = (fn_index < num_functions) ? fn_addr[fn_index] : 0;
        rec->return_address = func_address + (uint64_t)insn_off;

        for (uint16_t l = 0; l < num_loc; l++) {
            uint8_t  type   = cur_u8(&c);
            (void)cur_u8(&c);              /* reserved */
            uint16_t size   = cur_u16(&c);
            uint16_t reg    = cur_u16(&c);
            (void)cur_u16(&c);             /* reserved */
            int32_t  offset = cur_i32(&c);

            if (rec->locations != NULL) {
                rec->locations[l].type      = type;
                rec->locations[l].size      = size;
                rec->locations[l].dwarf_reg = reg;
                rec->locations[l].offset    = offset;
            }
        }

        /* The location array is padded to an eight byte boundary, then a fixed
         * two byte reserved field and the live-out count follow. The record is
         * padded again to an eight byte boundary after the live-out array. */
        cur_align8(&c, base);
        (void)cur_u16(&c);                 /* reserved field */
        uint16_t num_liveouts = cur_u16(&c);
        for (uint16_t lo = 0; lo < num_liveouts; lo++) {
            (void)cur_u16(&c);             /* dwarf reg */
            (void)cur_u8(&c);              /* reserved  */
            (void)cur_u8(&c);              /* size      */
        }
        cur_align8(&c, base);

        /* Advance the function attribution. */
        if (fn_remaining > 0)
            fn_remaining--;
        while (fn_index < num_functions && fn_remaining == 0 && (r + 1) < num_records) {
            fn_index++;
            fn_remaining = (fn_index < num_functions) ? fn_count[fn_index] : 0;
            if (fn_remaining != 0)
                break;
        }
    }

    free(fn_addr);
    free(fn_count);

    /* The cursor now sits at the first byte past this table. A read that ran
     * off the end leaves c.p == c.end (the cursor helpers stop advancing past
     * end); treat reaching exactly the limit as a clean finish. */
    return c.p;
}

/* Parse the stack map section starting at `base`. Separated from
 * pgc_stackmap_init so the parser can be driven with a supplied buffer (the
 * linked __LLVM_StackMaps in production, a synthetic blob under test). Marks
 * the section parsed even on early return so init is not retried.
 *
 * Incremental builds compile each module to its own object file, so each
 * object carries its own complete stack map table. The linker concatenates
 * these into one section, giving several version 3 tables laid end to end.
 * This driver parses every table in sequence, appending all records, rather
 * than assuming a single table. */
void pgc_stackmap_parse(const uint8_t *base)
{
    g_sm.parsed = true;

    if (base == NULL)
        return;  /* No statepoints in the program: nothing to parse. */

    /* Upper bound for reads. When the platform reported the section length use
     * it as a hard end so no read can run past the section. When it did not
     * (the ELF weak symbol path, g_sm_section_len == 0) fall back to a large
     * guard; table iteration then stops at the first position that does not
     * hold a valid version 3 header. */
    const uint8_t *limit = (g_sm_section_len != 0)
        ? base + g_sm_section_len
        : base + ((size_t)1 << 30);

    const uint8_t *table = base;
    while (table + 8 + 12 <= limit) {
        const uint8_t *next = parse_one_table(table, base, limit);
        if (next == NULL || next <= table)
            break;   /* No further valid table, or no forward progress. */

        /* Tables are emitted as whole eight byte units, so the next table
         * begins at the next eight byte boundary. Align defensively in case a
         * linker inserted padding between concatenated tables. */
        size_t off = (size_t)(next - base);
        size_t rem = off & 7u;
        if (rem != 0)
            next += (8 - rem);
        table = next;
    }

    /* Sort by return address for binary-search lookup, once, across the
     * records accumulated from every table. */
    if (g_sm.count > 1)
        qsort(g_sm.records, g_sm.count, sizeof(pgc_sm_record), pgc_sm_cmp);
}

/* =========================================================================
 * Stack root walk
 *
 * For each stopped thread, walk its frames from the parked frame pointer
 * toward the stack base along the saved frame pointer chain. Each frame holds
 * a frame record of two pointers: the saved frame pointer and the return
 * address. The return address names a call site in the caller of that frame.
 * The matching stack map record lists that caller's live managed pointers as
 * stack slots relative to the caller's stack pointer. The caller's stack
 * pointer is the address one frame record above the current frame pointer.
 *
 * The chain walk and the frame-record layout hold on the System V (Linux,
 * Android) and AAPCS / Apple (macOS, iOS) ABIs, where every managed function
 * keeps a frame pointer and the live slots are recorded relative to the stack
 * pointer. Windows x64 keeps no reliable frame pointer chain and records unwind
 * data in .pdata / .xdata, so its frames are walked with RtlVirtualUnwind from
 * a captured register context in pgc_walk_thread_win64.
 * ====================================================================== */

static void pgc_visit_record_roots(const pgc_sm_record *rec,
                                   uintptr_t caller_sp,
                                   pgc_root_visitor visit)
{
    for (uint16_t i = 0; i < rec->num_locations; i++) {
        const pgc_sm_location *loc = &rec->locations[i];
        if (loc->type != PGC_LOC_INDIRECT)
            continue;

        /* The slot is recorded relative to the stack pointer of the frame that
         * holds the safepoint. caller_sp is that stack pointer. The slot at
         * [caller_sp + offset] holds a managed pointer, and its address is what
         * the visitor marks and rewrites on a move. */
        void **slot = (void **)((unsigned char *)caller_sp + loc->offset);

        /* A statepoint lists a base pointer and a derived pointer for each live
         * value. When both name one slot, that slot appears twice in the
         * record. The update pass rewrites a slot in place, so a second visit
         * to the same slot would forward an already-forwarded pointer. Visit
         * each distinct slot once. */
        bool duplicate = false;
        for (uint16_t j = 0; j < i; j++) {
            if (rec->locations[j].type == PGC_LOC_INDIRECT &&
                rec->locations[j].dwarf_reg == loc->dwarf_reg &&
                rec->locations[j].offset == loc->offset) {
                duplicate = true;
                break;
            }
        }
        if (!duplicate) {
            visit(slot);
        }

        /* A PGC_LOC_REGISTER root would live in the parked register file. The
         * supported targets spill every live managed pointer to the stack, so
         * no record names a register and that case does not arise. */
    }
}

#ifdef _WIN32
/* Walk one stopped thread on Windows x64 with the unwinder. The seed context
 * holds the register state captured when the thread parked or began collecting.
 * Each step unwinds one frame through its .pdata entry. After an unwind the
 * context's program counter is a return address into the caller and the
 * context's stack pointer is that caller's stack pointer at the call, which is
 * the base the slot offsets are measured from. A program counter that matches
 * a record contributes that frame's roots. */
static void pgc_walk_thread_win64(pgc_thread *thread, pgc_root_visitor visit)
{
    CONTEXT ctx = thread->win_context;  /* a copy; RtlVirtualUnwind rewrites it */
    uintptr_t stack_base = (uintptr_t)thread->stack_base;

    for (int guard = 0; guard < 4096; guard++) {
        const pgc_sm_record *rec = pgc_sm_lookup((uint64_t)ctx.Rip);
        if (rec != NULL)
            pgc_visit_record_roots(rec, (uintptr_t)ctx.Rsp, visit);

        DWORD64 image_base = 0;
        PRUNTIME_FUNCTION fn =
            RtlLookupFunctionEntry(ctx.Rip, &image_base, NULL);
        if (fn == NULL)
            break;  /* leaf frame or top of stack: no unwind data */

        PVOID handler_data = NULL;
        DWORD64 establisher = 0;
        uintptr_t prev_sp = (uintptr_t)ctx.Rsp;
        RtlVirtualUnwind(UNW_FLAG_NHANDLER, image_base, ctx.Rip, fn,
                         &ctx, &handler_data, &establisher, NULL);

        if (ctx.Rip == 0)
            break;
        if ((uintptr_t)ctx.Rsp <= prev_sp)
            break;  /* the stack pointer must move toward the base */
        if (stack_base != 0 && (uintptr_t)ctx.Rsp >= stack_base)
            break;
    }
}
#endif

void pgc_visit_stack_roots(pgc_root_visitor visit)
{
    if (!g_sm.parsed)
        pgc_stackmap_init();
    if (g_sm.count == 0)
        return;

    static unsigned long g_walk_count = 0;
    g_walk_count++;

    for (int t = 0; t < g_gc.thread_count; t++) {
        pgc_thread *thread = &g_gc.threads[t];
        if (!thread->in_use || thread->stack_top == NULL)
            continue;

#ifdef _WIN32
        pgc_walk_thread_win64(thread, visit);
#else
        /* A thread parked in a native blocking call recorded its managed
         * caller's safepoint directly, because the begin-blocking frame is
         * gone by the time the call blocks. The record at blocking_ret_addr
         * names that caller's live managed pointers, relative to
         * blocking_caller_sp. The chain walk below continues from the caller's
         * frame pointer, which the thread stored in stack_top. */
        if (thread->blocking_ret_addr != NULL) {
            const pgc_sm_record *brec =
                pgc_sm_lookup((uint64_t)(uintptr_t)thread->blocking_ret_addr);
            if (brec != NULL)
                pgc_visit_record_roots(brec,
                                       (uintptr_t)thread->blocking_caller_sp,
                                       visit);
        }

        /* Walk the frame-pointer chain. Each frame on common ABIs is:
         *   [saved frame pointer][return address] at the frame base, with the
         * caller's frame pointer chained through the saved slot. We start from
         * the parked frame pointer if available; lacking a separate captured
         * frame pointer here, we conservatively scan the saved-fp chain
         * beginning at stack_top. */
        uintptr_t fp = (uintptr_t)thread->stack_top;
        uintptr_t base = (uintptr_t)thread->stack_base;

        while (fp != 0 && fp < base) {
            uintptr_t *frame = (uintptr_t *)fp;
            uintptr_t saved_fp = frame[0];
            uintptr_t ret_addr = frame[1];

            const pgc_sm_record *rec = pgc_sm_lookup((uint64_t)ret_addr);
            const char *how = "exact";
            if (rec == NULL) {
                /* The stack holds the return address. When records are keyed by
                 * the call-site address, the call instruction sits one
                 * instruction (four bytes on this target) before the return
                 * address. Retry at that address. */
                rec = pgc_sm_lookup((uint64_t)ret_addr - 4);
                how = "ret-4";
            }

            if (rec != NULL) {
                /* The matched record belongs to the call site in the caller of
                 * this frame. The caller's stack pointer at that call is one
                 * frame record above this frame pointer, and the record's slot
                 * offsets are relative to that stack pointer. */
                uintptr_t caller_sp = fp + PGC_FRAME_RECORD_BYTES;
                pgc_visit_record_roots(rec, caller_sp, visit);
            }

            if (saved_fp <= fp)  /* chain must move toward the base */
                break;
            fp = saved_fp;
        }
#endif
    }
}
