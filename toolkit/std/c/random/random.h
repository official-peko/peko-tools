/*
 * peko_random.h
 * Fast non-cryptographic PRNG for the Peko random library.
 * Uses the CMWC (Complementary Multiply With Carry) algorithm.
 *
 * This RNG is NOT cryptographically secure.
 * For security-sensitive random data use the crypto package instead.
 */

#ifndef PEKO_RANDOM_H
#define PEKO_RANDOM_H

#include <stdint.h>
#include <stdbool.h>

/* -------------------------------------------------------------------------
 * Library initialization
 * Called automatically on first use via the initialized flag.
 * ---------------------------------------------------------------------- */

/* Seeds the global RNG from the current time. Safe to call multiple times. */
void peko_random_init(void);

/* Seeds the global RNG with a specific value for a reproducible sequence. */
void peko_random_seed(uint32_t seed);

/* -------------------------------------------------------------------------
 * Global RNG functions
 * ---------------------------------------------------------------------- */

/* Returns a random int in [min, max). Initializes automatically if needed. */
int peko_random_int(int min, int max);

/* Returns a random float in [0.0, 1.0). */
float peko_random_float(void);

/* Returns a random bool. */
bool peko_random_bool(void);

/* -------------------------------------------------------------------------
 * Rng instance functions
 * Each Rng instance has its own independent state.
 * ---------------------------------------------------------------------- */

/*
 * Allocates and initializes a new Rng instance seeded from the current time.
 * Returns an opaque pointer to the instance. Caller owns the pointer and
 * must free it with peko_rng_free when done.
 */
void *peko_rng_new(void);

/*
 * Allocates and initializes a new Rng instance with the provided seed.
 * Returns an opaque pointer to the instance.
 */
void *peko_rng_new_seeded(uint32_t seed);

/* Frees an Rng instance. */
void peko_rng_free(void *rng);

/* Re-seeds an existing Rng instance. */
void peko_rng_seed(void *rng, uint32_t seed);

/* Returns a random int in [min, max) from the provided Rng instance. */
int peko_rng_int(void *rng, int min, int max);

/* Returns a random float in [0.0, 1.0) from the provided Rng instance. */
float peko_rng_float(void *rng);

/* Returns a random bool from the provided Rng instance. */
bool peko_rng_bool(void *rng);

#endif /* PEKO_RANDOM_H */
