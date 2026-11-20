/* Small ownership/EVP adapter; all cryptography is provided by libcrypto. */
#include <openssl/crypto.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/opensslv.h>
#include <openssl/provider.h>

#if OPENSSL_VERSION_NUMBER < 0x30500000L
#error "OpenSSL 3.5 or newer is required"
#endif
#ifndef OPENSSL_THREADS
#error "A thread-safe OpenSSL build is required"
#endif

typedef struct {
    OSSL_LIB_CTX *libctx;
    OSSL_PROVIDER *provider;
    EVP_SIGNATURE *algorithms[2];
} bsc_pqc_context;

static const char *const names[2] = {"ML-DSA-44", "SLH-DSA-SHA2-128s"};

/* Called once by Rust's OnceLock. No global config or provider policy is used. */
void *bsc_pqc_openssl_init(void) {
    bsc_pqc_context *ctx = OPENSSL_zalloc(sizeof(*ctx));
    if (ctx == NULL)
        return NULL;
    ctx->libctx = OSSL_LIB_CTX_new();
    if (ctx->libctx == NULL)
        goto fail;
    ctx->provider = OSSL_PROVIDER_load(ctx->libctx, "default");
    if (ctx->provider == NULL)
        goto fail;
    ctx->algorithms[0] = EVP_SIGNATURE_fetch(ctx->libctx, names[0], "provider=default");
    ctx->algorithms[1] = EVP_SIGNATURE_fetch(ctx->libctx, names[1], "provider=default");
    if (ctx->algorithms[0] == NULL || ctx->algorithms[1] == NULL)
        goto fail;
    return ctx;
fail:
    EVP_SIGNATURE_free(ctx->algorithms[0]);
    EVP_SIGNATURE_free(ctx->algorithms[1]);
    OSSL_PROVIDER_unload(ctx->provider);
    OSSL_LIB_CTX_free(ctx->libctx);
    OPENSSL_free(ctx);
    ERR_clear_error();
    return NULL;
}

/* Immutable shared context; each call owns its key and verification context.
 * Returns 1 valid, 0 invalid, -1 backend failure. No input pointer is retained.
 * Raw public keys for these schemes have no invalid fixed-length bit patterns.
 */
int bsc_pqc_openssl_verify(const void *opaque, unsigned int algorithm,
                          const unsigned char *message, size_t message_len,
                          const unsigned char *signature, size_t signature_len,
                          const unsigned char *public_key, size_t public_key_len) {
    const bsc_pqc_context *ctx = opaque;
    EVP_PKEY *key = NULL;
    EVP_PKEY_CTX *verify = NULL;
    int result = -1;
    if (ctx == NULL || algorithm > 1)
        return -1;
    if (message_len != 32 || public_key_len != (algorithm == 0 ? 1312 : 32)
        || signature_len != (algorithm == 0 ? 2420 : 7856))
        return 0;

    key = EVP_PKEY_new_raw_public_key_ex(ctx->libctx, names[algorithm],
                                       "provider=default", public_key, public_key_len);
    if (key == NULL)
        goto done;
    verify = EVP_PKEY_CTX_new_from_pkey(ctx->libctx, key, "provider=default");
    if (verify == NULL)
        goto done;
    /* Message API defaults to the pure FIPS scheme and empty context, with
     * neither prehashing nor external-mu/raw-internal verification enabled. */
    if (EVP_PKEY_verify_message_init(verify, ctx->algorithms[algorithm], NULL) != 1)
        goto done;
    result = EVP_PKEY_verify(verify, signature, signature_len, message, message_len);
done:
    EVP_PKEY_CTX_free(verify);
    EVP_PKEY_free(key);
    /* Invalid signatures can populate the thread-local error queue. */
    ERR_clear_error();
    return result;
}

const char *bsc_pqc_openssl_version(void) {
    return OpenSSL_version(OPENSSL_VERSION);
}
