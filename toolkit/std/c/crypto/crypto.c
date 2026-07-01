/*
 * crypto.c
 * Wrapper over libsodium backing std::crypto: initialization, secure random,
 * encoding, hashing, authenticated encryption, public-key boxes, signatures,
 * message authentication codes, and password hashing.
 *
 * GC discipline: pgc_alloc_atomic can move every live managed buffer. Two
 * shapes appear here. The hashing and encoding allocators read their managed
 * input into stack or malloc memory first, then allocate the managed output,
 * so no managed pointer is held across the allocation. Every other function
 * takes a caller-allocated managed output buffer and never allocates, so it is
 * not gcsafe and its managed pointers stay put for the whole call.
 */

#define SODIUM_STATIC 1

#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "libsodium/sodium.h"

extern void *pgc_alloc_atomic(size_t size);

/* A volatile reference keeps the linker from dead-stripping the sodium symbols
 * before the wrapper's references resolve. */
__attribute__((used))
static int (*volatile sodium_anchor)(void) = sodium_init;

/* Copies a stable buffer into a fresh managed atomic buffer. */
static unsigned char *gc_dup(const unsigned char *src, int size)
{
    unsigned char *buf = (unsigned char *)pgc_alloc_atomic((size_t)size);
    if (!buf)
        return NULL;
    memcpy(buf, src, (size_t)size);
    return buf;
}

/* Copies a managed input into malloc memory so its address is stable across
 * the managed output allocation. Returns NULL for an empty input. */
static unsigned char *stage_in(const unsigned char *src, int len)
{
    if (len <= 0 || src == NULL)
        return NULL;
    unsigned char *buf = (unsigned char *)malloc((size_t)len);
    if (!buf)
        return NULL;
    memcpy(buf, src, (size_t)len);
    return buf;
}

/* =========================================================================
 * Initialization and secure random
 * ====================================================================== */

int peko_crypto_init(void)
{
    return sodium_init() < 0 ? -1 : 0;
}

/* A random 32-bit value widened to carry its full unsigned range. */
int64_t peko_random_u32(void)
{
    return (int64_t)(uint32_t)randombytes_random();
}

/* Fills the caller's managed buffer with len secure random bytes. The buffer
 * is filled in place without allocating, so it does not move. */
void peko_random_bytes(void *buf, int len)
{
    randombytes_buf(buf, (size_t)len);
}

/* =========================================================================
 * Encoding
 * ====================================================================== */

char *peko_bin_to_hex(const unsigned char *src, int src_len)
{
    int out_len = src_len * 2 + 1;

    unsigned char *src_copy = stage_in(src, src_len);
    if (src_len > 0 && !src_copy)
        return NULL;

    char *out = (char *)pgc_alloc_atomic((size_t)out_len);
    if (!out) {
        free(src_copy);
        return NULL;
    }

    sodium_bin2hex(out, (size_t)out_len,
                   src_len > 0 ? src_copy : (const unsigned char *)"",
                   (size_t)src_len);
    free(src_copy);
    return out;
}

/* =========================================================================
 * Hashing, one-shot
 * ====================================================================== */

unsigned char *peko_sha256(const unsigned char *in, int in_len)
{
    unsigned char tmp[crypto_hash_sha256_BYTES];
    if (crypto_hash_sha256(tmp, in, (unsigned long long)in_len) != 0)
        return NULL;
    return gc_dup(tmp, crypto_hash_sha256_BYTES);
}

unsigned char *peko_sha512(const unsigned char *in, int in_len)
{
    unsigned char tmp[crypto_hash_sha512_BYTES];
    if (crypto_hash_sha512(tmp, in, (unsigned long long)in_len) != 0)
        return NULL;
    return gc_dup(tmp, crypto_hash_sha512_BYTES);
}

unsigned char *peko_blake2b(const unsigned char *in, int in_len,
                            const unsigned char *key, int key_len,
                            int out_len)
{
    if (out_len < (int)crypto_generichash_BYTES_MIN ||
        out_len > (int)crypto_generichash_BYTES_MAX)
        return NULL;

    unsigned char *in_copy = stage_in(in, in_len);
    unsigned char *key_copy = stage_in(key, key_len);
    if ((in_len > 0 && !in_copy) || (key_len > 0 && !key_copy)) {
        free(in_copy);
        free(key_copy);
        return NULL;
    }

    unsigned char *out = (unsigned char *)pgc_alloc_atomic((size_t)out_len);
    if (!out) {
        free(in_copy);
        free(key_copy);
        return NULL;
    }

    int rc = crypto_generichash(out, (size_t)out_len,
                                in_len > 0 ? in_copy : (const unsigned char *)"",
                                (unsigned long long)in_len,
                                key_len > 0 ? key_copy : NULL,
                                (size_t)(key_len > 0 ? key_len : 0));
    free(in_copy);
    free(key_copy);
    return rc != 0 ? NULL : out;
}

/* =========================================================================
 * Hashing, streaming. The context is an unmanaged malloc allocation the
 * caller threads through update and final. final frees it.
 * ====================================================================== */

void *peko_sha256_ctx_new(void)
{
    crypto_hash_sha256_state *ctx =
        (crypto_hash_sha256_state *)malloc(sizeof(crypto_hash_sha256_state));
    if (!ctx)
        return NULL;
    if (crypto_hash_sha256_init(ctx) != 0) {
        free(ctx);
        return NULL;
    }
    return ctx;
}

void peko_sha256_update(void *ctx, const unsigned char *in, int in_len)
{
    if (!ctx || !in || in_len <= 0)
        return;
    crypto_hash_sha256_update((crypto_hash_sha256_state *)ctx,
                              in, (unsigned long long)in_len);
}

unsigned char *peko_sha256_final(void *ctx)
{
    if (!ctx)
        return NULL;
    unsigned char tmp[crypto_hash_sha256_BYTES];
    int rc = crypto_hash_sha256_final((crypto_hash_sha256_state *)ctx, tmp);
    free(ctx);
    return rc != 0 ? NULL : gc_dup(tmp, crypto_hash_sha256_BYTES);
}

void *peko_sha512_ctx_new(void)
{
    crypto_hash_sha512_state *ctx =
        (crypto_hash_sha512_state *)malloc(sizeof(crypto_hash_sha512_state));
    if (!ctx)
        return NULL;
    if (crypto_hash_sha512_init(ctx) != 0) {
        free(ctx);
        return NULL;
    }
    return ctx;
}

void peko_sha512_update(void *ctx, const unsigned char *in, int in_len)
{
    if (!ctx || !in || in_len <= 0)
        return;
    crypto_hash_sha512_update((crypto_hash_sha512_state *)ctx,
                              in, (unsigned long long)in_len);
}

unsigned char *peko_sha512_final(void *ctx)
{
    if (!ctx)
        return NULL;
    unsigned char tmp[crypto_hash_sha512_BYTES];
    int rc = crypto_hash_sha512_final((crypto_hash_sha512_state *)ctx, tmp);
    free(ctx);
    return rc != 0 ? NULL : gc_dup(tmp, crypto_hash_sha512_BYTES);
}

/* =========================================================================
 * Phase 2: encoding, symmetric and public-key encryption, signatures,
 * message authentication codes, and password hashing.
 *
 * These functions never allocate managed memory: the caller passes a managed
 * output buffer sized from the known constants, and the function fills it
 * synchronously. Nothing here is gcsafe, so every managed pointer stays put
 * for the duration of the call and no staging is needed.
 * ====================================================================== */

/* The libsodium base64 variant code for a Peko variant id. */
static int base64_variant(int variant)
{
    switch (variant) {
        case 1:  return sodium_base64_VARIANT_URLSAFE;
        case 2:  return sodium_base64_VARIANT_URLSAFE_NO_PADDING;
        default: return sodium_base64_VARIANT_ORIGINAL;
    }
}

/* The interactive or sensitive Argon2id cost parameters. */
static unsigned long long pwhash_ops(int sensitive)
{
    return sensitive ? crypto_pwhash_OPSLIMIT_SENSITIVE
                     : crypto_pwhash_OPSLIMIT_INTERACTIVE;
}

static size_t pwhash_mem(int sensitive)
{
    return sensitive ? crypto_pwhash_MEMLIMIT_SENSITIVE
                     : crypto_pwhash_MEMLIMIT_INTERACTIVE;
}

/* --- Encoding --- */

/* Encodes src as base64 into a fresh managed NUL-terminated string. This one
 * allocates, so it stages its input first. */
char *peko_to_base64(const unsigned char *src, int src_len, int variant)
{
    int v = base64_variant(variant);

    unsigned char *src_copy = stage_in(src, src_len);
    if (src_len > 0 && !src_copy)
        return NULL;

    size_t out_len = sodium_base64_encoded_len((size_t)src_len, v);
    char *out = (char *)pgc_alloc_atomic(out_len);
    if (!out) {
        free(src_copy);
        return NULL;
    }

    sodium_bin2base64(out, out_len,
                      src_len > 0 ? src_copy : (const unsigned char *)"",
                      (size_t)src_len, v);
    free(src_copy);
    return out;
}

/* Decodes hex into the caller's buffer. Returns the byte count, or -1. */
int peko_from_hex(unsigned char *out, int out_cap, const char *hex, int hex_len)
{
    size_t decoded = 0;
    if (sodium_hex2bin(out, (size_t)out_cap, hex, (size_t)hex_len,
                       NULL, &decoded, NULL) != 0)
        return -1;
    return (int)decoded;
}

/* Decodes base64 into the caller's buffer. Returns the byte count, or -1. */
int peko_from_base64(unsigned char *out, int out_cap, const char *b64,
                     int b64_len, int variant)
{
    size_t decoded = 0;
    if (sodium_base642bin(out, (size_t)out_cap, b64, (size_t)b64_len,
                          NULL, &decoded, NULL, base64_variant(variant)) != 0)
        return -1;
    return (int)decoded;
}

/* --- Secretbox (XSalsa20-Poly1305) --- */

void peko_secretbox_keygen(unsigned char *out)
{
    crypto_secretbox_keygen(out);
}

void peko_secretbox_nonce(unsigned char *out)
{
    randombytes_buf(out, crypto_secretbox_NONCEBYTES);
}

int peko_secretbox_encrypt(unsigned char *ct, const unsigned char *pt,
                           int pt_len, const unsigned char *nonce,
                           const unsigned char *key)
{
    if (crypto_secretbox_easy(ct, pt, (unsigned long long)pt_len, nonce, key) != 0)
        return -1;
    return pt_len + (int)crypto_secretbox_MACBYTES;
}

int peko_secretbox_decrypt(unsigned char *pt, const unsigned char *ct,
                           int ct_len, const unsigned char *nonce,
                           const unsigned char *key)
{
    int pt_len = ct_len - (int)crypto_secretbox_MACBYTES;
    if (pt_len < 0)
        return -1;
    if (crypto_secretbox_open_easy(pt, ct, (unsigned long long)ct_len, nonce, key) != 0)
        return -1;
    return pt_len;
}

/* --- ChaCha20-Poly1305 IETF (AEAD) --- */

void peko_chacha_keygen(unsigned char *out)
{
    crypto_aead_chacha20poly1305_ietf_keygen(out);
}

void peko_chacha_nonce(unsigned char *out)
{
    randombytes_buf(out, crypto_aead_chacha20poly1305_ietf_NPUBBYTES);
}

int peko_chacha_encrypt(unsigned char *ct, const unsigned char *pt, int pt_len,
                        const unsigned char *ad, int ad_len,
                        const unsigned char *nonce, const unsigned char *key)
{
    unsigned long long actual = 0;
    if (crypto_aead_chacha20poly1305_ietf_encrypt(
            ct, &actual, pt, (unsigned long long)pt_len,
            ad_len > 0 ? ad : NULL, (unsigned long long)(ad_len > 0 ? ad_len : 0),
            NULL, nonce, key) != 0)
        return -1;
    return (int)actual;
}

int peko_chacha_decrypt(unsigned char *pt, const unsigned char *ct, int ct_len,
                        const unsigned char *ad, int ad_len,
                        const unsigned char *nonce, const unsigned char *key)
{
    unsigned long long actual = 0;
    if (crypto_aead_chacha20poly1305_ietf_decrypt(
            pt, &actual, NULL, ct, (unsigned long long)ct_len,
            ad_len > 0 ? ad : NULL, (unsigned long long)(ad_len > 0 ? ad_len : 0),
            nonce, key) != 0)
        return -1;
    return (int)actual;
}

/* --- AES-256-GCM (AEAD, needs CPU support) --- */

int peko_aesgcm_available(void)
{
    return crypto_aead_aes256gcm_is_available() ? 1 : 0;
}

void peko_aesgcm_keygen(unsigned char *out)
{
    crypto_aead_aes256gcm_keygen(out);
}

void peko_aesgcm_nonce(unsigned char *out)
{
    randombytes_buf(out, crypto_aead_aes256gcm_NPUBBYTES);
}

int peko_aesgcm_encrypt(unsigned char *ct, const unsigned char *pt, int pt_len,
                        const unsigned char *ad, int ad_len,
                        const unsigned char *nonce, const unsigned char *key)
{
    unsigned long long actual = 0;
    if (crypto_aead_aes256gcm_encrypt(
            ct, &actual, pt, (unsigned long long)pt_len,
            ad_len > 0 ? ad : NULL, (unsigned long long)(ad_len > 0 ? ad_len : 0),
            NULL, nonce, key) != 0)
        return -1;
    return (int)actual;
}

int peko_aesgcm_decrypt(unsigned char *pt, const unsigned char *ct, int ct_len,
                        const unsigned char *ad, int ad_len,
                        const unsigned char *nonce, const unsigned char *key)
{
    unsigned long long actual = 0;
    if (crypto_aead_aes256gcm_decrypt(
            pt, &actual, NULL, ct, (unsigned long long)ct_len,
            ad_len > 0 ? ad : NULL, (unsigned long long)(ad_len > 0 ? ad_len : 0),
            nonce, key) != 0)
        return -1;
    return (int)actual;
}

/* --- Box (X25519 + XSalsa20-Poly1305) --- */

int peko_box_keypair(unsigned char *pk, unsigned char *sk)
{
    return crypto_box_keypair(pk, sk) == 0 ? 0 : -1;
}

void peko_box_nonce(unsigned char *out)
{
    randombytes_buf(out, crypto_box_NONCEBYTES);
}

int peko_box_encrypt(unsigned char *ct, const unsigned char *pt, int pt_len,
                     const unsigned char *nonce,
                     const unsigned char *recipient_pk,
                     const unsigned char *sender_sk)
{
    if (crypto_box_easy(ct, pt, (unsigned long long)pt_len, nonce,
                        recipient_pk, sender_sk) != 0)
        return -1;
    return pt_len + (int)crypto_box_MACBYTES;
}

int peko_box_decrypt(unsigned char *pt, const unsigned char *ct, int ct_len,
                     const unsigned char *nonce,
                     const unsigned char *sender_pk,
                     const unsigned char *recipient_sk)
{
    int pt_len = ct_len - (int)crypto_box_MACBYTES;
    if (pt_len < 0)
        return -1;
    if (crypto_box_open_easy(pt, ct, (unsigned long long)ct_len, nonce,
                             sender_pk, recipient_sk) != 0)
        return -1;
    return pt_len;
}

/* --- Ed25519 signatures --- */

int peko_sign_keypair(unsigned char *pk, unsigned char *sk)
{
    return crypto_sign_keypair(pk, sk) == 0 ? 0 : -1;
}

int peko_sign(unsigned char *sig, const unsigned char *message,
              int message_len, const unsigned char *secret_key)
{
    unsigned long long sig_len = 0;
    if (crypto_sign_detached(sig, &sig_len, message,
                             (unsigned long long)message_len, secret_key) != 0)
        return -1;
    return (int)sig_len;
}

int peko_sign_verify(const unsigned char *signature, const unsigned char *message,
                     int message_len, const unsigned char *public_key)
{
    return crypto_sign_verify_detached(signature, message,
                                       (unsigned long long)message_len,
                                       public_key) == 0 ? 1 : 0;
}

/* --- HMAC-SHA256 and HMAC-SHA512 --- */

void peko_hmac256_keygen(unsigned char *out)
{
    crypto_auth_hmacsha256_keygen(out);
}

int peko_hmac256(unsigned char *out, const unsigned char *message,
                 int message_len, const unsigned char *key)
{
    return crypto_auth_hmacsha256(out, message,
                                  (unsigned long long)message_len, key) == 0 ? 0 : -1;
}

int peko_hmac256_verify(const unsigned char *tag, const unsigned char *message,
                        int message_len, const unsigned char *key)
{
    return crypto_auth_hmacsha256_verify(tag, message,
                                         (unsigned long long)message_len,
                                         key) == 0 ? 1 : 0;
}

void peko_hmac512_keygen(unsigned char *out)
{
    crypto_auth_hmacsha512_keygen(out);
}

int peko_hmac512(unsigned char *out, const unsigned char *message,
                 int message_len, const unsigned char *key)
{
    return crypto_auth_hmacsha512(out, message,
                                  (unsigned long long)message_len, key) == 0 ? 0 : -1;
}

int peko_hmac512_verify(const unsigned char *tag, const unsigned char *message,
                        int message_len, const unsigned char *key)
{
    return crypto_auth_hmacsha512_verify(tag, message,
                                         (unsigned long long)message_len,
                                         key) == 0 ? 1 : 0;
}

/* --- Argon2id password hashing --- */

void peko_pwhash_salt(unsigned char *out)
{
    randombytes_buf(out, crypto_pwhash_SALTBYTES);
}

/* Derives a key of key_len bytes from a password and salt. Returns 0, or -1. */
int peko_pwhash(unsigned char *out, int key_len, const char *password,
                int password_len, const unsigned char *salt, int sensitive)
{
    return crypto_pwhash(out, (unsigned long long)key_len, password,
                         (unsigned long long)password_len, salt,
                         pwhash_ops(sensitive), pwhash_mem(sensitive),
                         crypto_pwhash_ALG_ARGON2ID13) == 0 ? 0 : -1;
}

/* Hashes a password into the caller's buffer as a self-contained string.
 * Returns 0, or -1. */
int peko_pwhash_str(char *out, const char *password, int password_len,
                    int sensitive)
{
    return crypto_pwhash_str(out, password, (unsigned long long)password_len,
                             pwhash_ops(sensitive), pwhash_mem(sensitive)) == 0
           ? 0 : -1;
}

int peko_pwhash_verify(const char *hash_str, const char *password,
                       int password_len)
{
    return crypto_pwhash_str_verify(hash_str, password,
                                    (unsigned long long)password_len) == 0 ? 1 : 0;
}
