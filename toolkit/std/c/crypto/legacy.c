/*
 * legacy.c
 * MD5 and SHA-1 backing the legacy corner of std::crypto. These algorithms
 * are cryptographically broken. They serve compatibility cases such as
 * checksums and non-security protocols.
 *
 * The implementations are Brad Conte's public domain reference code, wrapped
 * with a streaming context the caller threads through init, update, and final.
 * The context is an unmanaged malloc allocation; final frees it.
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* The streaming entry points are declared in crypto/legacy.peko.h. */
void *md5_context_allocate(void);
void md5_init_binded(void *ctx);
void md5_update_binded(void *ctx, const void *data, int len);
void md5_final_binded(void *ctx, void *hash);
void *sha1_context_allocate(void);
void sha1_init_binded(void *ctx);
void sha1_update_binded(void *ctx, const void *data, int len);
void sha1_final_binded(void *ctx, void *hash);

/* =========================================================================
 * MD5  (Brad Conte, public domain)
 * ====================================================================== */

typedef struct {
    uint8_t  data[64];
    uint32_t datalen;
    uint64_t bitlen;
    uint32_t state[4];
} MD5_CTX_IMPL;

#define ROTLEFT(a,b) (((a) << (b)) | ((a) >> (32-(b))))

#define F(x,y,z) ((x & y) | (~x & z))
#define G(x,y,z) ((x & z) | (y & ~z))
#define H(x,y,z) (x ^ y ^ z)
#define I(x,y,z) (y ^ (x | ~z))

#define FF(a,b,c,d,m,s,t) { a += F(b,c,d) + m + t; a = b + ROTLEFT(a,s); }
#define GG(a,b,c,d,m,s,t) { a += G(b,c,d) + m + t; a = b + ROTLEFT(a,s); }
#define HH(a,b,c,d,m,s,t) { a += H(b,c,d) + m + t; a = b + ROTLEFT(a,s); }
#define II(a,b,c,d,m,s,t) { a += I(b,c,d) + m + t; a = b + ROTLEFT(a,s); }

static void md5_transform(MD5_CTX_IMPL *ctx, const uint8_t data[])
{
    uint32_t a, b, c, d, m[16], i, j;

    for (i = 0, j = 0; i < 16; ++i, j += 4)
        m[i] = (uint32_t)(data[j]) | ((uint32_t)(data[j+1]) << 8) |
               ((uint32_t)(data[j+2]) << 16) | ((uint32_t)(data[j+3]) << 24);

    a = ctx->state[0]; b = ctx->state[1];
    c = ctx->state[2]; d = ctx->state[3];

    FF(a,b,c,d,m[0],   7,0xd76aa478); FF(d,a,b,c,m[1],  12,0xe8c7b756);
    FF(c,d,a,b,m[2],  17,0x242070db); FF(b,c,d,a,m[3],  22,0xc1bdceee);
    FF(a,b,c,d,m[4],   7,0xf57c0faf); FF(d,a,b,c,m[5],  12,0x4787c62a);
    FF(c,d,a,b,m[6],  17,0xa8304613); FF(b,c,d,a,m[7],  22,0xfd469501);
    FF(a,b,c,d,m[8],   7,0x698098d8); FF(d,a,b,c,m[9],  12,0x8b44f7af);
    FF(c,d,a,b,m[10], 17,0xffff5bb1); FF(b,c,d,a,m[11], 22,0x895cd7be);
    FF(a,b,c,d,m[12],  7,0x6b901122); FF(d,a,b,c,m[13], 12,0xfd987193);
    FF(c,d,a,b,m[14], 17,0xa679438e); FF(b,c,d,a,m[15], 22,0x49b40821);

    GG(a,b,c,d,m[1],   5,0xf61e2562); GG(d,a,b,c,m[6],   9,0xc040b340);
    GG(c,d,a,b,m[11], 14,0x265e5a51); GG(b,c,d,a,m[0],  20,0xe9b6c7aa);
    GG(a,b,c,d,m[5],   5,0xd62f105d); GG(d,a,b,c,m[10],  9,0x02441453);
    GG(c,d,a,b,m[15], 14,0xd8a1e681); GG(b,c,d,a,m[4],  20,0xe7d3fbc8);
    GG(a,b,c,d,m[9],   5,0x21e1cde6); GG(d,a,b,c,m[14],  9,0xc33707d6);
    GG(c,d,a,b,m[3],  14,0xf4d50d87); GG(b,c,d,a,m[8],  20,0x455a14ed);
    GG(a,b,c,d,m[13],  5,0xa9e3e905); GG(d,a,b,c,m[2],   9,0xfcefa3f8);
    GG(c,d,a,b,m[7],  14,0x676f02d9); GG(b,c,d,a,m[12], 20,0x8d2a4c8a);

    HH(a,b,c,d,m[5],   4,0xfffa3942); HH(d,a,b,c,m[8],  11,0x8771f681);
    HH(c,d,a,b,m[11], 16,0x6d9d6122); HH(b,c,d,a,m[14], 23,0xfde5380c);
    HH(a,b,c,d,m[1],   4,0xa4beea44); HH(d,a,b,c,m[4],  11,0x4bdecfa9);
    HH(c,d,a,b,m[7],  16,0xf6bb4b60); HH(b,c,d,a,m[10], 23,0xbebfbc70);
    HH(a,b,c,d,m[13],  4,0x289b7ec6); HH(d,a,b,c,m[0],  11,0xeaa127fa);
    HH(c,d,a,b,m[3],  16,0xd4ef3085); HH(b,c,d,a,m[6],  23,0x04881d05);
    HH(a,b,c,d,m[9],   4,0xd9d4d039); HH(d,a,b,c,m[12], 11,0xe6db99e5);
    HH(c,d,a,b,m[15], 16,0x1fa27cf8); HH(b,c,d,a,m[2],  23,0xc4ac5665);

    II(a,b,c,d,m[0],   6,0xf4292244); II(d,a,b,c,m[7],  10,0x432aff97);
    II(c,d,a,b,m[14], 15,0xab9423a7); II(b,c,d,a,m[5],  21,0xfc93a039);
    II(a,b,c,d,m[12],  6,0x655b59c3); II(d,a,b,c,m[3],  10,0x8f0ccc92);
    II(c,d,a,b,m[10], 15,0xffeff47d); II(b,c,d,a,m[1],  21,0x85845dd1);
    II(a,b,c,d,m[8],   6,0x6fa87e4f); II(d,a,b,c,m[15], 10,0xfe2ce6e0);
    II(c,d,a,b,m[6],  15,0xa3014314); II(b,c,d,a,m[13], 21,0x4e0811a1);
    II(a,b,c,d,m[4],   6,0xf7537e82); II(d,a,b,c,m[11], 10,0xbd3af235);
    II(c,d,a,b,m[2],  15,0x2ad7d2bb); II(b,c,d,a,m[9],  21,0xeb86d391);

    ctx->state[0] += a; ctx->state[1] += b;
    ctx->state[2] += c; ctx->state[3] += d;
}

static void md5_init(MD5_CTX_IMPL *ctx)
{
    ctx->datalen = 0; ctx->bitlen = 0;
    ctx->state[0] = 0x67452301; ctx->state[1] = 0xefcdab89;
    ctx->state[2] = 0x98badcfe; ctx->state[3] = 0x10325476;
}

static void md5_update(MD5_CTX_IMPL *ctx, const uint8_t *data, size_t len)
{
    for (size_t i = 0; i < len; ++i) {
        ctx->data[ctx->datalen] = data[i];
        ctx->datalen++;
        if (ctx->datalen == 64) {
            md5_transform(ctx, ctx->data);
            ctx->bitlen += 512;
            ctx->datalen = 0;
        }
    }
}

static void md5_final(MD5_CTX_IMPL *ctx, uint8_t hash[16])
{
    uint32_t i = ctx->datalen;
    if (ctx->datalen < 56) {
        ctx->data[i++] = 0x80;
        while (i < 56) ctx->data[i++] = 0x00;
    } else {
        ctx->data[i++] = 0x80;
        while (i < 64) ctx->data[i++] = 0x00;
        md5_transform(ctx, ctx->data);
        memset(ctx->data, 0, 56);
    }
    ctx->bitlen += ctx->datalen * 8;
    ctx->data[56] = (uint8_t)(ctx->bitlen);
    ctx->data[57] = (uint8_t)(ctx->bitlen >> 8);
    ctx->data[58] = (uint8_t)(ctx->bitlen >> 16);
    ctx->data[59] = (uint8_t)(ctx->bitlen >> 24);
    ctx->data[60] = (uint8_t)(ctx->bitlen >> 32);
    ctx->data[61] = (uint8_t)(ctx->bitlen >> 40);
    ctx->data[62] = (uint8_t)(ctx->bitlen >> 48);
    ctx->data[63] = (uint8_t)(ctx->bitlen >> 56);
    md5_transform(ctx, ctx->data);
    for (i = 0; i < 4; ++i) {
        hash[i]      = (uint8_t)(ctx->state[0] >> (i * 8));
        hash[i + 4]  = (uint8_t)(ctx->state[1] >> (i * 8));
        hash[i + 8]  = (uint8_t)(ctx->state[2] >> (i * 8));
        hash[i + 12] = (uint8_t)(ctx->state[3] >> (i * 8));
    }
}

/* Peko API */

void *md5_context_allocate(void)
{
    MD5_CTX_IMPL *ctx = (MD5_CTX_IMPL *)malloc(sizeof(MD5_CTX_IMPL));
    return ctx;
}

void md5_init_binded(void *ctx)
{
    md5_init((MD5_CTX_IMPL *)ctx);
}

void md5_update_binded(void *ctx, const void *data, int len)
{
    md5_update((MD5_CTX_IMPL *)ctx, (const uint8_t *)data, (size_t)len);
}

void md5_final_binded(void *ctx, void *hash)
{
    md5_final((MD5_CTX_IMPL *)ctx, (uint8_t *)hash);
    free(ctx);
}

/* =========================================================================
 * SHA-1  (Brad Conte, public domain)
 * ====================================================================== */

typedef struct {
    uint8_t  data[64];
    uint32_t datalen;
    uint64_t bitlen;
    uint32_t state[5];
    uint32_t k[4];
} SHA1_CTX_IMPL;

#define SHA1_ROTLEFT(a,b) (((a) << (b)) | ((a) >> (32-(b))))

static void sha1_transform(SHA1_CTX_IMPL *ctx, const uint8_t data[])
{
    uint32_t a, b, c, d, e, i, j, t, m[80];

    for (i = 0, j = 0; i < 16; ++i, j += 4)
        m[i] = ((uint32_t)data[j] << 24) | ((uint32_t)data[j+1] << 16) |
               ((uint32_t)data[j+2] << 8) | (uint32_t)data[j+3];
    for (; i < 80; ++i)
        m[i] = SHA1_ROTLEFT(m[i-3] ^ m[i-8] ^ m[i-14] ^ m[i-16], 1);

    a = ctx->state[0]; b = ctx->state[1]; c = ctx->state[2];
    d = ctx->state[3]; e = ctx->state[4];

    for (i = 0; i < 20; ++i) {
        t = SHA1_ROTLEFT(a,5) + ((b & c) | (~b & d)) + e + ctx->k[0] + m[i];
        e=d; d=c; c=SHA1_ROTLEFT(b,30); b=a; a=t;
    }
    for (; i < 40; ++i) {
        t = SHA1_ROTLEFT(a,5) + (b ^ c ^ d) + e + ctx->k[1] + m[i];
        e=d; d=c; c=SHA1_ROTLEFT(b,30); b=a; a=t;
    }
    for (; i < 60; ++i) {
        t = SHA1_ROTLEFT(a,5) + ((b&c)|(b&d)|(c&d)) + e + ctx->k[2] + m[i];
        e=d; d=c; c=SHA1_ROTLEFT(b,30); b=a; a=t;
    }
    for (; i < 80; ++i) {
        t = SHA1_ROTLEFT(a,5) + (b ^ c ^ d) + e + ctx->k[3] + m[i];
        e=d; d=c; c=SHA1_ROTLEFT(b,30); b=a; a=t;
    }

    ctx->state[0] += a; ctx->state[1] += b; ctx->state[2] += c;
    ctx->state[3] += d; ctx->state[4] += e;
}

static void sha1_impl_init(SHA1_CTX_IMPL *ctx)
{
    ctx->datalen  = 0; ctx->bitlen = 0;
    ctx->state[0] = 0x67452301; ctx->state[1] = 0xEFCDAB89;
    ctx->state[2] = 0x98BADCFE; ctx->state[3] = 0x10325476;
    ctx->state[4] = 0xc3d2e1f0;
    ctx->k[0] = 0x5a827999; ctx->k[1] = 0x6ed9eba1;
    ctx->k[2] = 0x8f1bbcdc; ctx->k[3] = 0xca62c1d6;
}

static void sha1_impl_update(SHA1_CTX_IMPL *ctx, const uint8_t *data, size_t len)
{
    for (size_t i = 0; i < len; ++i) {
        ctx->data[ctx->datalen] = data[i];
        ctx->datalen++;
        if (ctx->datalen == 64) {
            sha1_transform(ctx, ctx->data);
            ctx->bitlen += 512;
            ctx->datalen = 0;
        }
    }
}

static void sha1_impl_final(SHA1_CTX_IMPL *ctx, uint8_t hash[20])
{
    uint32_t i = ctx->datalen;
    if (ctx->datalen < 56) {
        ctx->data[i++] = 0x80;
        while (i < 56) ctx->data[i++] = 0x00;
    } else {
        ctx->data[i++] = 0x80;
        while (i < 64) ctx->data[i++] = 0x00;
        sha1_transform(ctx, ctx->data);
        memset(ctx->data, 0, 56);
    }
    ctx->bitlen += ctx->datalen * 8;
    ctx->data[63] = (uint8_t)(ctx->bitlen);
    ctx->data[62] = (uint8_t)(ctx->bitlen >> 8);
    ctx->data[61] = (uint8_t)(ctx->bitlen >> 16);
    ctx->data[60] = (uint8_t)(ctx->bitlen >> 24);
    ctx->data[59] = (uint8_t)(ctx->bitlen >> 32);
    ctx->data[58] = (uint8_t)(ctx->bitlen >> 40);
    ctx->data[57] = (uint8_t)(ctx->bitlen >> 48);
    ctx->data[56] = (uint8_t)(ctx->bitlen >> 56);
    sha1_transform(ctx, ctx->data);
    for (i = 0; i < 5; ++i) {
        hash[i*4]   = (uint8_t)(ctx->state[i] >> 24);
        hash[i*4+1] = (uint8_t)(ctx->state[i] >> 16);
        hash[i*4+2] = (uint8_t)(ctx->state[i] >> 8);
        hash[i*4+3] = (uint8_t)(ctx->state[i]);
    }
}

/* Peko API */

void *sha1_context_allocate(void)
{
    SHA1_CTX_IMPL *ctx = (SHA1_CTX_IMPL *)malloc(sizeof(SHA1_CTX_IMPL));
    return ctx;
}

void sha1_init_binded(void *ctx)
{
    sha1_impl_init((SHA1_CTX_IMPL *)ctx);
}

void sha1_update_binded(void *ctx, const void *data, int len)
{
    sha1_impl_update((SHA1_CTX_IMPL *)ctx, (const uint8_t *)data, (size_t)len);
}

void sha1_final_binded(void *ctx, void *hash)
{
    sha1_impl_final((SHA1_CTX_IMPL *)ctx, (uint8_t *)hash);
    free(ctx);
}
