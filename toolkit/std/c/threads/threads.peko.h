#include <peko.h>

PEKO_BEGIN

/* Threading primitives backing std::threads, defined in threads.c over
   pthreads and the GC runtime. A thread, mutex, and cancel token are unmanaged
   malloc handles the caller owns. The thread worker is a closure's raw function
   pointer (passed as an opaque) and its data is the closure's managed context,
   which the C side keeps alive with a GC handle until the thread attaches. */

/* The GC parks the calling thread for a blocking call. pgc_thread_attach and
   pgc_thread_detach are declared in c/runtime/gc.peko.h. */
p_fn p_gcsafe void pgc_begin_blocking();
p_fn void pgc_end_blocking();

/* Thread lifecycle. worker is a function pointer passed as an opaque; data is
   the closure's managed context. */
p_fn p_gcsafe p_opaque peko_thread_create(p_opaque worker, p_gc_opaque data, p_i1 synchronous);
p_fn void peko_thread_kill(p_opaque thread);
p_fn p_gcsafe void peko_thread_free(p_opaque thread);

/* Mutex. The OS lock lives in malloc memory so the collector cannot move it. */
p_fn p_opaque peko_mutex_new();
p_fn void peko_mutex_free(p_opaque mutex);
p_fn void peko_mutex_lock(p_opaque mutex);
p_fn void peko_mutex_unlock(p_opaque mutex);

/* Sleep the calling thread. The caller brackets this with pgc_begin_blocking. */
p_fn void peko_sleep_ms(p_i32 ms);

/* Cooperative cancellation. on_cancel takes a closure's function pointer and
   its managed context. */
p_fn p_opaque peko_cancel_token_new();
p_fn void peko_cancel_token_free(p_opaque token);
p_fn p_i1 peko_cancel_token_is_cancelled(p_opaque token);
p_fn p_gcsafe void peko_cancel_token_cancel(p_opaque token);
p_fn p_gcsafe void peko_cancel_token_on_cancel(p_opaque token, p_opaque callback, p_gc_opaque callback_data);

/* Message channel. Items are managed pointers the C side keeps alive with GC
   handles. The recv calls return NULL for a closed or empty channel (a managed
   item is never NULL), so the success flag stays internal to C. */
p_fn p_opaque peko_channel_new(p_i32 buffer_size);
p_fn void peko_channel_free(p_opaque channel);
p_fn p_gcsafe p_i1 peko_channel_send(p_opaque channel, p_gc_opaque item);
p_fn p_gcsafe p_i1 peko_channel_try_send(p_opaque channel, p_gc_opaque item);
p_fn p_gcsafe p_gc_opaque peko_channel_recv_value(p_opaque channel);
p_fn p_gcsafe p_gc_opaque peko_channel_try_recv_value(p_opaque channel);
p_fn p_gcsafe void peko_channel_close(p_opaque channel);
p_fn p_i1 peko_channel_is_closed(p_opaque channel);

/* Future: the result of an asynchronous computation. */
p_fn p_opaque peko_future_new();
p_fn void peko_future_free(p_opaque future);
p_fn p_gcsafe void peko_future_set_result(p_opaque future, p_gc_opaque result);
p_fn p_gcsafe p_gc_opaque peko_future_await(p_opaque future);
p_fn p_gcsafe p_gc_opaque peko_future_await_timeout_value(p_opaque future, p_i32 timeout_ms);
p_fn p_i1 peko_future_is_complete(p_opaque future);
p_fn p_gcsafe void peko_future_cancel(p_opaque future);

PEKO_END
