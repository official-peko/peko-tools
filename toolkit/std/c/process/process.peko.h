#include <peko.h>

PEKO_BEGIN

/* Child process spawning and stdio piping for std::process, defined in
   peko_process.c. An argv and a process handle are unmanaged malloc
   allocations the caller owns. String parameters are GC-managed buffers read
   synchronously. Line reads and process waits block, so they are gcsafe: the
   collector may run while the thread waits. */

/* Argument vector, built before spawning. */
p_fn p_opaque peko_argv_new(void);
p_fn void peko_argv_push(p_opaque argv, p_gc(p_i8) arg);
p_fn void peko_argv_free(p_opaque argv);

/* Spawn program with the given argv (excluding program) and working directory
   (empty inherits the parent's). Returns a process handle, or null on failure. */
p_fn p_opaque peko_process_spawn(p_gc(p_i8) program, p_opaque argv, p_gc(p_i8) cwd);

/* Write bytes to the child's stdin; returns the count written, or -1. */
p_fn p_i32 peko_process_write(p_opaque handle, p_gc(p_i8) data, p_i32 len);

/* Close the child's stdin, signaling end of input. */
p_fn void peko_process_close_stdin(p_opaque handle);

/* Read the next line from stdout (which=1) or stderr (which=2), stripping the
   newline. Returns 1 when a line is available, 0 at end of output. Blocking. */
p_fn p_gcsafe p_i32 peko_process_read_line(p_opaque handle, p_i32 which);

/* Read up to n raw bytes from stdout (which=1) or stderr (which=2) into the
   stream buffer, returning the count read, or 0 at end of output. Unlike the
   line reader this does not stop at newlines, for framed protocols. Blocking. */
p_fn p_gcsafe p_i32 peko_process_read_bytes(p_opaque handle, p_i32 which, p_i32 n);

/* The last line or byte run read from stdout (which=1) or stderr (which=2). */
p_fn p_cstr peko_process_line(p_opaque handle, p_i32 which);

/* Wait for the child to exit and return its exit code. Blocking. The code is
   cached, so calling it after the child has exited returns at once. */
p_fn p_gcsafe p_i32 peko_process_wait(p_opaque handle);

/* Whether the child is still running: 1 if running, 0 if it has exited. Reaps
   the child without blocking when it has exited, so a following wait is cheap. */
p_fn p_i32 peko_process_is_running(p_opaque handle);

/* Force the child to terminate. */
p_fn void peko_process_kill(p_opaque handle);

/* The child's process id. */
p_fn p_i32 peko_process_pid(p_opaque handle);

/* Free the process handle and close its pipes. Does not terminate the child. */
p_fn void peko_process_free(p_opaque handle);

PEKO_END
