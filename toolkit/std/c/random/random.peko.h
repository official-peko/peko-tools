#include <peko.h>

PEKO_BEGIN

/* Fast non-cryptographic CMWC PRNG primitives backing std::random. Defined in
   random.c. Scalars only cross the boundary; the Rng instance handle is an
   unmanaged malloc pointer the caller owns and frees. NOT cryptographically
   secure. */

/* Global RNG. The state is shared and time-seeded on first use. */
p_fn void peko_random_init();
p_fn void peko_random_seed(p_i32 seed);
p_fn p_i32 peko_random_int(p_i32 min, p_i32 max);
p_fn p_f32 peko_random_float();
p_fn p_i1 peko_random_bool();

/* Rng instance. Each handle owns an independent state; free it when done. */
p_fn p_opaque peko_rng_new();
p_fn p_opaque peko_rng_new_seeded(p_i32 seed);
p_fn void peko_rng_free(p_opaque rng);
p_fn void peko_rng_seed(p_opaque rng, p_i32 seed);
p_fn p_i32 peko_rng_int(p_opaque rng, p_i32 min, p_i32 max);
p_fn p_f32 peko_rng_float(p_opaque rng);
p_fn p_i1 peko_rng_bool(p_opaque rng);

PEKO_END
