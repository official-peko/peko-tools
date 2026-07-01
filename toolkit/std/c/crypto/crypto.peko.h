#include <peko.h>

PEKO_BEGIN

/* libsodium wrappers backing std::crypto, defined in crypto.c. Inputs are
   managed byte buffers read synchronously. Hashing and encoding allocate a
   fresh managed buffer the collector owns, so they are gcsafe. A streaming
   context is an unmanaged malloc handle the caller threads through update and
   final; final frees it. */

/* Initialization and secure random. */
p_fn p_i32 peko_crypto_init();
p_fn p_i64 peko_random_u32();
p_fn void peko_random_bytes(p_gc(p_i8) buf, p_i32 len);

/* Hex encoding. Returns a fresh managed NUL-terminated string, or null. */
p_fn p_gcsafe p_gc(p_i8) peko_bin_to_hex(p_gc(p_i8) src, p_i32 src_len);

/* One-shot hashing. Returns a fresh managed digest buffer, or null. */
p_fn p_gcsafe p_gc(p_i8) peko_sha256(p_gc(p_i8) input, p_i32 in_len);
p_fn p_gcsafe p_gc(p_i8) peko_sha512(p_gc(p_i8) input, p_i32 in_len);
p_fn p_gcsafe p_gc(p_i8) peko_blake2b(p_gc(p_i8) input, p_i32 in_len, p_gc(p_i8) key, p_i32 key_len, p_i32 out_len);

/* Streaming SHA-256. */
p_fn p_opaque peko_sha256_ctx_new();
p_fn void peko_sha256_update(p_opaque ctx, p_gc(p_i8) input, p_i32 in_len);
p_fn p_gcsafe p_gc(p_i8) peko_sha256_final(p_opaque ctx);

/* Streaming SHA-512. */
p_fn p_opaque peko_sha512_ctx_new();
p_fn void peko_sha512_update(p_opaque ctx, p_gc(p_i8) input, p_i32 in_len);
p_fn p_gcsafe p_gc(p_i8) peko_sha512_final(p_opaque ctx);

/* Phase 2: every output is a caller-allocated managed buffer, so these do not
   allocate and are not gcsafe. The encrypt and decrypt calls return the bytes
   written, or -1 on failure or a failed authentication. */

/* Encoding. base64 encode allocates a fresh managed string; the decoders fill
   a caller buffer and return the byte count. */
p_fn p_gcsafe p_gc(p_i8) peko_to_base64(p_gc(p_i8) src, p_i32 src_len, p_i32 variant);
p_fn p_i32 peko_from_hex(p_gc(p_i8) out, p_i32 out_cap, p_gc(p_i8) hex, p_i32 hex_len);
p_fn p_i32 peko_from_base64(p_gc(p_i8) out, p_i32 out_cap, p_gc(p_i8) b64, p_i32 b64_len, p_i32 variant);

/* Secretbox (XSalsa20-Poly1305). */
p_fn void peko_secretbox_keygen(p_gc(p_i8) out);
p_fn void peko_secretbox_nonce(p_gc(p_i8) out);
p_fn p_i32 peko_secretbox_encrypt(p_gc(p_i8) ct, p_gc(p_i8) pt, p_i32 pt_len, p_gc(p_i8) nonce, p_gc(p_i8) key);
p_fn p_i32 peko_secretbox_decrypt(p_gc(p_i8) pt, p_gc(p_i8) ct, p_i32 ct_len, p_gc(p_i8) nonce, p_gc(p_i8) key);

/* ChaCha20-Poly1305 IETF (AEAD). */
p_fn void peko_chacha_keygen(p_gc(p_i8) out);
p_fn void peko_chacha_nonce(p_gc(p_i8) out);
p_fn p_i32 peko_chacha_encrypt(p_gc(p_i8) ct, p_gc(p_i8) pt, p_i32 pt_len, p_gc(p_i8) ad, p_i32 ad_len, p_gc(p_i8) nonce, p_gc(p_i8) key);
p_fn p_i32 peko_chacha_decrypt(p_gc(p_i8) pt, p_gc(p_i8) ct, p_i32 ct_len, p_gc(p_i8) ad, p_i32 ad_len, p_gc(p_i8) nonce, p_gc(p_i8) key);

/* AES-256-GCM (AEAD, needs CPU support). */
p_fn p_i32 peko_aesgcm_available();
p_fn void peko_aesgcm_keygen(p_gc(p_i8) out);
p_fn void peko_aesgcm_nonce(p_gc(p_i8) out);
p_fn p_i32 peko_aesgcm_encrypt(p_gc(p_i8) ct, p_gc(p_i8) pt, p_i32 pt_len, p_gc(p_i8) ad, p_i32 ad_len, p_gc(p_i8) nonce, p_gc(p_i8) key);
p_fn p_i32 peko_aesgcm_decrypt(p_gc(p_i8) pt, p_gc(p_i8) ct, p_i32 ct_len, p_gc(p_i8) ad, p_i32 ad_len, p_gc(p_i8) nonce, p_gc(p_i8) key);

/* Box (X25519 + XSalsa20-Poly1305). */
p_fn p_i32 peko_box_keypair(p_gc(p_i8) pk, p_gc(p_i8) sk);
p_fn void peko_box_nonce(p_gc(p_i8) out);
p_fn p_i32 peko_box_encrypt(p_gc(p_i8) ct, p_gc(p_i8) pt, p_i32 pt_len, p_gc(p_i8) nonce, p_gc(p_i8) recipient_pk, p_gc(p_i8) sender_sk);
p_fn p_i32 peko_box_decrypt(p_gc(p_i8) pt, p_gc(p_i8) ct, p_i32 ct_len, p_gc(p_i8) nonce, p_gc(p_i8) sender_pk, p_gc(p_i8) recipient_sk);

/* Ed25519 signatures. */
p_fn p_i32 peko_sign_keypair(p_gc(p_i8) pk, p_gc(p_i8) sk);
p_fn p_i32 peko_sign(p_gc(p_i8) sig, p_gc(p_i8) message, p_i32 message_len, p_gc(p_i8) secret_key);
p_fn p_i32 peko_sign_verify(p_gc(p_i8) signature, p_gc(p_i8) message, p_i32 message_len, p_gc(p_i8) public_key);

/* HMAC-SHA256 and HMAC-SHA512. */
p_fn void peko_hmac256_keygen(p_gc(p_i8) out);
p_fn p_i32 peko_hmac256(p_gc(p_i8) out, p_gc(p_i8) message, p_i32 message_len, p_gc(p_i8) key);
p_fn p_i32 peko_hmac256_verify(p_gc(p_i8) tag, p_gc(p_i8) message, p_i32 message_len, p_gc(p_i8) key);
p_fn void peko_hmac512_keygen(p_gc(p_i8) out);
p_fn p_i32 peko_hmac512(p_gc(p_i8) out, p_gc(p_i8) message, p_i32 message_len, p_gc(p_i8) key);
p_fn p_i32 peko_hmac512_verify(p_gc(p_i8) tag, p_gc(p_i8) message, p_i32 message_len, p_gc(p_i8) key);

/* Argon2id password hashing. The string hash and verify take NUL-terminated
   managed buffers. */
p_fn void peko_pwhash_salt(p_gc(p_i8) out);
p_fn p_i32 peko_pwhash(p_gc(p_i8) out, p_i32 key_len, p_gc(p_i8) password, p_i32 password_len, p_gc(p_i8) salt, p_i32 sensitive);
p_fn p_i32 peko_pwhash_str(p_gc(p_i8) out, p_gc(p_i8) password, p_i32 password_len, p_i32 sensitive);
p_fn p_i32 peko_pwhash_verify(p_gc(p_i8) hash_str, p_gc(p_i8) password, p_i32 password_len);

PEKO_END
