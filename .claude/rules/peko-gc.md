---
paths:
  - "**/*.peko"
  - "**/*.peko.h"
  - "**/*.c"
  - "**/*.m"
  - "**/*.h"
---

# Peko GC and FFI rules

PekoScript runs on a stop-the-world sliding mark-compact (Lisp2) collector.
Two facts drive every rule here:

- Objects move. Any raw pointer to a GC-managed object goes stale after a
  collection. Only GC-tracked references update automatically.
- Collections can fire at any allocation. Any attached thread that is not
  declared parked must be able to reach a safepoint at that moment.

Violations are not compile errors. They produce intermittent hangs,
stale-pointer crashes, and corruption that are hard to reproduce.

V2 spells the managed pointer `pointer<T>`. V1 source may use `Pointer<T>`; the
rules are identical.

## FFI types at the boundary

| Memory origin | Peko FFI type |
|---|---|
| `pgc_alloc_atomic`, `pgc_alloc_managed`, `__rt_peko_alloc<T>` | `pointer<void>` or `pointer<T>` |
| `malloc` or OS resource | `opaque` |
| C string literal or stack | `cstr` |
| GC string primitive | `string` or `cstr` parameter (autoboxed) |

- A C function that receives a GC buffer declares that parameter
  `pointer<void>`, not `opaque`.
- A C function that returns a `pgc_alloc_atomic` or `pgc_alloc_managed` buffer
  declares its return type `pointer<void>`.
- `malloc` OS handles use `opaque` and are never GC-allocated. The OS tracks
  them by address and cannot be told when the GC moves them.
- Null-check a `pointer<void>` with `== null`, not `== None`. `None` is for
  `Option<T>`.
- `&T` is a reference and at codegen is not distinct from `opaque`.

## Allocation

- Typed GC allocation in Peko uses `__rt_peko_alloc<T>(count)`.
- Do not call `peko_gc_alloc` or `peko_gc_alloc_object` from Peko source. Do
  not redeclare the GC ABI block (`peko_gc_alloc_object`, `peko_gc_alloc`,
  `peko_gc_add_global_root`, `peko_gc_write_barrier`) outside the runtime
  module. Redeclaration causes duplicate symbols and undefined behavior.
- In C, GC buffers use `pgc_alloc_atomic(n)` for bytes with no managed
  children or `pgc_alloc_managed(descriptor, n)` for traced objects. Non-GC
  memory uses `malloc`.
- Never mix. A `pgc_alloc_*` pointer never lives in a `malloc` struct the GC
  does not trace, and a `malloc` pointer never lives in a traced field the GC
  expects to update.
- `pgc_alloc_atomic` and `pgc_alloc_managed` can trigger a collection. Do not
  call them while holding an application mutex.
- `gc_alloc` and `gc_free` are removed. Use the `pgc_*` API.

## OS primitives in class fields

A class instance is GC-managed and may move. A field holding a raw OS resource
is `opaque`, allocated with `malloc` in C.

Mutexes are never GC-allocated. A `pthread_mutex_t` holds the OS wait queue and
is registered by address; a move corrupts it and causes hangs or crashes in
`pthread_mutex_lock`. The same holds for condition variables, semaphores, file
descriptors, sockets, crypto contexts, and thread handles.

## Pointer arrays and untraced containers

A `T**` array from `pgc_alloc_atomic` is atomic: the GC relocates the array but
does not scan its entries, so `pgc_alloc_atomic` string entries go stale after
a move. To return a list of strings from C, `malloc` each string and the array;
Peko's `cstr`-to-`string` cast copies each into GC memory on access. Never store
`pgc_alloc_atomic` pointers in a `malloc` struct, a C static, or a global and
expect them to survive a collection.

## Handles

- Call `pgc_handle_create` before `pgc_begin_blocking`. It takes `g_gc.lock`
  internally, and a parked thread must not perform GC operations.
- Call `pgc_handle_release` when done. Unreleased handles leak and hold objects
  live.
- Never store a `pgc_handle` in a traced field. Handles are integer indices and
  the GC misreads them as managed pointers.

## Pinning

- Pin a GC buffer before a syscall that needs a stable address; unpin right
  after. Pins nest one unpin per pin.
- Hold a pin across a blocking call only together with `pgc_begin_blocking`.
- Keep pins short. Long pins fragment the heap.

## Threads and safepoints

- Any thread that allocates managed memory or holds a managed reference calls
  `pgc_thread_attach` before its first managed access and `pgc_thread_detach`
  before exit. The Peko threads trampoline does this; raw `pthread_create` or
  `CreateThread` threads do it manually.
- Peko code reaches safepoints automatically at allocations and loop
  back-edges.
- Every blocking call on an attached thread is bracketed:

  ```c
  pgc_begin_blocking();
  blocking_call_here();
  pgc_end_blocking();
  ```

  or `PGC_BLOCKING(expr)`. This covers `accept`, `recv`, `read`, `send`,
  `write`, mutex lock, condition wait, thread join, sleep, slow file IO, and
  any syscall that can block.

## begin_blocking invariants

1. While parked, between `pgc_begin_blocking` and `pgc_end_blocking`, the
   thread does not allocate managed memory, create or release handles, pin or
   unpin, access any managed field, or call anything that does.
2. Do not hold `g_gc.lock` when calling `pgc_begin_blocking`,
   `pgc_end_blocking`, or any handle, pin, or root operation. They all take
   `g_gc.lock` and would deadlock.
3. `pgc_begin_blocking` and `pgc_end_blocking` pair exactly once each on the
   same thread.
4. Never spin-lock in place of `pgc_begin_blocking`. A spin never yields to the
   GC and the stop-the-world phase waits forever.

After `pgc_end_blocking` returns, any managed pointer held across the call may
point to a moved object unless it was a stack root or stored via a handle.
Re-derive it.

## Canonical patterns

Syscall on a GC buffer:

```c
void *stable = pgc_pin(buf);
pgc_begin_blocking();
recv(fd, stable, 1024, 0);
pgc_end_blocking();
pgc_unpin(buf);
```

Retain a managed object across a blocking call:

```c
pgc_handle h = pgc_handle_create(get_managed_object());
pgc_begin_blocking();
sleep(1);
pgc_end_blocking();
void *obj = pgc_handle_get(h);
use(obj);
pgc_handle_release(h);
```

Mutex field: `malloc` the mutex in C and return it to Peko as `opaque`.

## Common mistakes and symptoms

| Mistake | Symptom |
|---|---|
| Blocking syscall without `pgc_begin_blocking` | Random hang when a collection fires while the thread is blocked |
| `pgc_handle_create` inside `pgc_begin_blocking` | Handle-table race or deadlock |
| Mutex from `pgc_alloc_atomic` instead of `malloc` | Mutex corruption after the first collection |
| `pgc_alloc_atomic` pointers in an untraced `char**` | Stale pointers after a collection |
| `opaque` parameter that receives GC memory | Use-after-move |
| `None` used to null-check a `pointer<void>` | Missed null check |
| Redeclaring the GC ABI outside the runtime module | Duplicate symbols or undefined behavior |
| Spinlock without `pgc_begin_blocking` | Thread never yields; process hangs |
| Caching a managed pointer across begin and end blocking | Stale pointer or corruption |
