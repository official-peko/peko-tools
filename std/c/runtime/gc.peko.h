#include <peko.h>

PEKO_BEGIN

/* The source line and file of the current call point. The compiler writes
   these before a call so the runtime can report a meaningful location. */
p_var p_i32 current_line;
p_var p_cstr current_file;

/* Compiler-emitted allocation and write-barrier ABI. Allocators return a
   managed pointer (born in address space 1) and can trigger a collection, so
   they are gcsafe. descriptor and slot stay opaque at the C boundary. */
p_fn p_gcsafe p_gc_opaque peko_gc_alloc_object(p_opaque descriptor, p_i32 size);
p_fn p_gcsafe p_gc_opaque peko_gc_alloc(p_i32 size);
p_fn void peko_gc_add_global_root(p_opaque slot);
p_fn void peko_gc_write_barrier(p_opaque slot, p_opaque value);

/* GC lifecycle, driven from the program entry. */
p_fn p_gcsafe p_i32 pgc_init(p_i32 heap_bytes);
p_fn p_gcsafe void pgc_shutdown();
p_fn p_gcsafe void pgc_thread_attach();
p_fn p_gcsafe void pgc_thread_detach();

PEKO_END
