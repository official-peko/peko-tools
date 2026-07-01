/*
 * dispatch.c
 * Runtime trait dispatch for erased generics. An erased generic holds its
 * values as thin Object pointers, so a call to an `impl Trait` method on such a
 * value cannot know the concrete vtable slot statically. It finds the witness
 * table at runtime by scanning the object's TypeInfo itable for the trait.
 *
 * The struct layouts here mirror the records the codegen emits (see
 * llvm_constants.rs: type_info_struct_type and emit_type_info). A non-packed
 * LLVM struct uses the platform's natural C layout, so these match field for
 * field. The function reads static data only and never allocates, so it reaches
 * no safepoint and the object cannot move during the call.
 */

#include <stddef.h>
#include <stdint.h>

/* { i32 type_id, ptr parent, ptr descriptor, ptr vtable, ptr itable,
     i32 itable_len } */
typedef struct {
    int32_t type_id;
    void *parent;
    void *descriptor;
    void *vtable;
    void *itable;
    int32_t itable_len;
} peko_type_info;

/* { i32 trait_id, ptr witness } */
typedef struct {
    int32_t trait_id;
    void *witness;
} peko_itable_entry;

/*
 * The witness table for `trait_id` in the runtime type of `object`, or null
 * when the object's type does not implement the trait. Word 0 of the object
 * points at its static TypeInfo.
 */
void *peko_itable_lookup(void *object, int32_t trait_id)
{
    peko_type_info *info = *(peko_type_info **)object;
    peko_itable_entry *entries = (peko_itable_entry *)info->itable;

    for (int32_t index = 0; index < info->itable_len; index++) {
        if (entries[index].trait_id == trait_id)
            return entries[index].witness;
    }

    return NULL;
}
