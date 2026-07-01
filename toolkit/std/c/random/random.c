/*
 * peko_random.c
 * Fast non-cryptographic PRNG implementation for the Peko random library.
 * Uses the CMWC (Complementary Multiply With Carry) algorithm.
 *
 * This RNG is NOT cryptographically secure.
 * For security-sensitive random data use the crypto package instead.
 *
 * Thread safety: the global RNG state is protected by C11 atomics.
 * Each Rng instance has its own independent state and is not shared,
 * so instance functions require no locking.
 */

#include "random.h"
#include <stddef.h>
#include <stdlib.h>
#include <time.h>
#include <stdatomic.h>
#include "../runtime/include/pgc.h"

#ifdef _WIN32
#  ifndef WIN32_LEAN_AND_MEAN
#    define WIN32_LEAN_AND_MEAN
#  endif
#  include <windows.h>
typedef CRITICAL_SECTION rng_mutex_t;
static void rng_mutex_init(rng_mutex_t *m)  { InitializeCriticalSection(m); }
static void rng_mutex_lock(rng_mutex_t *m)  { pgc_begin_blocking(); EnterCriticalSection(m); pgc_end_blocking(); }
static void rng_mutex_unlock(rng_mutex_t *m){ LeaveCriticalSection(m); }
#else
#  include <pthread.h>
typedef pthread_mutex_t rng_mutex_t;
static void rng_mutex_init(rng_mutex_t *m)  { pthread_mutex_init(m, NULL); }
static void rng_mutex_lock(rng_mutex_t *m)  { pgc_begin_blocking(); pthread_mutex_lock(m); pgc_end_blocking(); }
static void rng_mutex_unlock(rng_mutex_t *m){ pthread_mutex_unlock(m); }
#endif

/* =========================================================================
 * CMWC core
 * ====================================================================== */

#define CMWC_CYCLE   4096
#define CMWC_C_INIT  362436
#define CMWC_A       18782ULL
#define CMWC_PHI     0x9e3779b9U

typedef struct {
    uint32_t q[CMWC_CYCLE];
    uint32_t c;
    uint32_t i; /* current index, kept in the struct so state is not shared */
} cmwc_state_t;

static void cmwc_seed(cmwc_state_t *s, uint32_t seed)
{
    int i;
    s->q[0] = seed;
    s->q[1] = seed + CMWC_PHI;
    s->q[2] = seed + CMWC_PHI + CMWC_PHI;
    for (i = 3; i < CMWC_CYCLE; i++)
        s->q[i] = s->q[i-3] ^ s->q[i-2] ^ CMWC_PHI ^ (uint32_t)i;
    s->c = CMWC_C_INIT;
    s->i = CMWC_CYCLE - 1;
}

static uint32_t cmwc_next(cmwc_state_t *s)
{
    uint64_t t;
    uint32_t x;
    const uint32_t r = 0xfffffffeU;

    s->i = (s->i + 1) & (CMWC_CYCLE - 1);
    t = CMWC_A * s->q[s->i] + s->c;
    s->c = (uint32_t)(t >> 32);
    x = (uint32_t)(t + s->c);
    if (x < s->c) {
        x++;
        s->c++;
    }
    return (s->q[s->i] = r - x);
}

/* =========================================================================
 * Global RNG state
 * Protected by a platform mutex wrapped with pgc_begin/end_blocking so the
 * GC can collect while a thread waits for the lock.
 * ====================================================================== */

static cmwc_state_t g_state;
static rng_mutex_t  g_lock;
static atomic_int   g_initialized = 0;

/* One-time mutex initialization via a separate atomic flag. */
static atomic_flag  g_mutex_init_flag = ATOMIC_FLAG_INIT;
static atomic_int   g_mutex_ready     = 0;

static void ensure_mutex_init(void)
{
    if (atomic_load(&g_mutex_ready))
        return;
    /* First thread to win the flag initializes the mutex. */
    if (!atomic_flag_test_and_set(&g_mutex_init_flag)) {
        rng_mutex_init(&g_lock);
        atomic_store(&g_mutex_ready, 1);
    } else {
        /* Spin until the winner finishes init. This is a one-time startup
         * cost that completes in nanoseconds. */
        while (!atomic_load(&g_mutex_ready))
            ;
    }
}

static void global_lock(void)
{
    ensure_mutex_init();
    rng_mutex_lock(&g_lock);
}

static void global_unlock(void)
{
    rng_mutex_unlock(&g_lock);
}

void peko_random_init(void)
{
    global_lock();
    cmwc_seed(&g_state, (uint32_t)time(NULL));
    atomic_store(&g_initialized, 1);
    global_unlock();
}

void peko_random_seed(uint32_t seed)
{
    global_lock();
    cmwc_seed(&g_state, seed);
    atomic_store(&g_initialized, 1);
    global_unlock();
}

/* Initializes on first use if peko_random_init has not been called yet. */
static void ensure_initialized(void)
{
    if (!atomic_load(&g_initialized))
        peko_random_init();
}

int peko_random_int(int min, int max)
{
    uint32_t raw;
    int range;

    if (max <= min)
        return min;

    ensure_initialized();
    range = max - min;

    global_lock();
    raw = cmwc_next(&g_state);
    global_unlock();

    /* Rejection sampling to eliminate modulo bias. */
    uint32_t limit = (uint32_t)(-(uint32_t)range % (uint32_t)range);
    while (raw < limit) {
        global_lock();
        raw = cmwc_next(&g_state);
        global_unlock();
    }

    return (int)(raw % (uint32_t)range) + min;
}

float peko_random_float(void)
{
    uint32_t raw;

    ensure_initialized();

    global_lock();
    raw = cmwc_next(&g_state);
    global_unlock();

    /* Map to [0.0, 1.0) by dividing by 2^32. */
    return (float)(raw * (1.0 / 4294967296.0));
}

bool peko_random_bool(void)
{
    uint32_t raw;

    ensure_initialized();

    global_lock();
    raw = cmwc_next(&g_state);
    global_unlock();

    return (raw & 1) != 0;
}

/* =========================================================================
 * Rng instance
 * Each instance has its own cmwc_state_t and its own index counter,
 * so instances are fully independent and require no locking.
 * ====================================================================== */

typedef struct {
    cmwc_state_t state;
} rng_instance_t;

static uint32_t rng_next(rng_instance_t *r)
{
    return cmwc_next(&r->state);
}

void *peko_rng_new(void)
{
    rng_instance_t *r = (rng_instance_t *)malloc(sizeof(rng_instance_t));
    if (!r)
        return NULL;
    cmwc_seed(&r->state, (uint32_t)time(NULL));
    return r;
}

void *peko_rng_new_seeded(uint32_t seed)
{
    rng_instance_t *r = (rng_instance_t *)malloc(sizeof(rng_instance_t));
    if (!r)
        return NULL;
    cmwc_seed(&r->state, seed);
    return r;
}

void peko_rng_free(void *rng)
{
    free(rng);
}

void peko_rng_seed(void *rng, uint32_t seed)
{
    rng_instance_t *r = (rng_instance_t *)rng;
    cmwc_seed(&r->state, seed);
}

int peko_rng_int(void *rng, int min, int max)
{
    rng_instance_t *r = (rng_instance_t *)rng;
    uint32_t raw;
    int range;

    if (max <= min)
        return min;

    range = max - min;
    raw   = rng_next(r);

    uint32_t limit = (uint32_t)(-(uint32_t)range % (uint32_t)range);
    while (raw < limit)
        raw = rng_next(r);

    return (int)(raw % (uint32_t)range) + min;
}

float peko_rng_float(void *rng)
{
    return (float)(rng_next((rng_instance_t *)rng) * (1.0 / 4294967296.0));
}

bool peko_rng_bool(void *rng)
{
    return (rng_next((rng_instance_t *)rng) & 1) != 0;
}
