/**
 * optipng-rs.h — C API for the liboptipng_rs library.
 *
 * Link against:
 *   - staticlib:  libliboptipng_rs.a
 *   - cdylib:     libliboptipng_rs.so / .dylib
 *
 * All functions are thread-safe. Error messages are thread-local.
 *
 * Memory conventions:
 *   - Handles (*OptimizeResult) are allocated by the library;
 *     free with optipng_result_free.
 *   - Strings from optipng_get_last_error are allocated by the library;
 *     free with optipng_free_string.
 *   - The byte buffer from optipng_result_bytes is owned by the library;
 *     do NOT free it.
 *   - optipng_version returns a static string; do NOT free it.
 */

#ifndef OPTIPNG_RS_H
#define OPTIPNG_RS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ─── Opaque types ─────────────────────────────────────────────────────── */

typedef struct OptimizeResult OptimizeResult;

/**
 * Progress callback. Invoked as trials complete; `completed` ranges from 1
 * to `total`. May be called concurrently from worker threads, so it MUST be
 * thread-safe.
 * @param completed  trials finished so far
 * @param total      total trials in this run
 * @param user_data  opaque pointer from OptipngParams.progress_user_data
 */
typedef void (*optipng_progress_cb)(size_t completed, size_t total, void *user_data);

/* ─── Parameter struct (pass by value) ─────────────────────────────────── */

/**
 * Optimization parameters.
 *
 * All boolean fields are 0 or 1 (uint8_t).
 * `opt_level` is 0..=5.
 * `mt` is 0 (meaning "all cores") or an explicit thread count.
 * `zi` is 0 or 1 (flate2/zlib) or 2 (Zopfli).
 */
typedef struct {
    uint8_t  opt_level;      /* 0-5, default 2 */
    uint32_t mt;             /* 0 = all cores */
    uint8_t  zi;             /* 0/1 = flate2, 2 = Zopfli */
    uint8_t  reduce_format;  /* 1 = apply reduction (default), 0 = skip */
    uint8_t  nz;             /* 1 = no-reencode */
    uint8_t  nc;             /* 1 = no-change (fixed color type/bit depth) */
    uint8_t  nb;             /* 1 = no-baseline */
    uint8_t  np;             /* 1 = no-palette */
    uint8_t  nx;             /* 1 = no-extended (16-bit) */
    optipng_progress_cb progress_cb;   /* or NULL */
    void *progress_user_data;
} OptipngParams;

/* ─── Main entry point ─────────────────────────────────────────────────── */

/**
 * Optimize a PNG in memory.
 *
 * @param input         Pointer to raw decoded pixel data
 *                      (or NULL if input_len == 0).
 * @param input_len     Length of `input` in bytes.
 * @param width         Image width in pixels.
 * @param height        Image height in pixels.
 * @param bit_depth     Original bit depth (1, 2, 4, 8, or 16).
 * @param color_type    Original PNG color type (0-4).
 * @param stride        Row stride in bytes
 *                      (may be > width * channels * bpp/8).
 * @param orig_idat_size  Original IDAT payload size
 *                        (for comparison reporting).
 * @param params        Pointer to an OptipngParams, or NULL for defaults.
 *
 * @returns Opaque handle on success, NULL on failure.
 *          On failure, call optipng_get_last_error() for details.
 */
OptimizeResult *optipng_optimize(
    const uint8_t *input,
    size_t input_len,
    uint32_t width,
    uint32_t height,
    uint8_t bit_depth,
    uint8_t color_type,
    size_t stride,
    size_t orig_idat_size,
    const OptipngParams *params
);

/* ─── Result accessors ─────────────────────────────────────────────────── */

/**
 * Returns a pointer to the optimized PNG byte stream.
 * Valid until optipng_result_free is called. Do NOT free this pointer.
 */
const uint8_t *optipng_result_bytes(const OptimizeResult *handle);

/** Length of the optimized PNG stream in bytes. */
size_t optipng_result_len(const OptimizeResult *handle);

/** Original IDAT payload size (before optimization). */
size_t optipng_result_orig_idat_size(const OptimizeResult *handle);

/** Best (smallest) IDAT payload size after optimization. */
size_t optipng_result_best_idat_size(const OptimizeResult *handle);

/** Returns 1 if the optimized IDAT is smaller than the original, 0 otherwise. */
int32_t optipng_result_is_smaller(const OptimizeResult *handle);

/**
 * Writes the winning trial's zlib compression level into *out.
 * Returns 1 on success, 0 if no winner or null pointer.
 */
int32_t optipng_result_zc(const OptimizeResult *handle, int32_t *out);

/**
 * Writes the winning trial's zlib memory level into *out.
 * Returns 1 on success, 0 if no winner or null pointer.
 */
int32_t optipng_result_zm(const OptimizeResult *handle, int32_t *out);

/**
 * Writes the winning trial's zlib strategy into *out.
 * Returns 1 on success, 0 if no winner or null pointer.
 */
int32_t optipng_result_zs(const OptimizeResult *handle, int32_t *out);

/**
 * Writes the winning trial's PNG filter type into *out.
 * Returns 1 on success, 0 if no winner or null pointer.
 */
int32_t optipng_result_f(const OptimizeResult *handle, uint8_t *out);

/* ─── Memory management ────────────────────────────────────────────────── */

/**
 * Releases a handle previously returned by optipng_optimize.
 * Passing NULL is a safe no-op.
 */
void optipng_result_free(OptimizeResult *handle);

/* ─── Error handling ───────────────────────────────────────────────────── */

/**
 * Returns the most recent error message as a newly-allocated NUL-terminated
 * C string, or NULL if there is no error.
 *
 * The caller MUST free the returned pointer with optipng_free_string.
 * Thread-local: safe to call from any thread.
 */
char *optipng_get_last_error(void);

/**
 * Frees a C string previously returned by optipng_get_last_error.
 * Passing NULL is a safe no-op.
 */
void optipng_free_string(char *ptr);

/* ─── Version ──────────────────────────────────────────────────────────── */

/**
 * Returns the library version as a NUL-terminated C string.
 * This is a static string — do NOT free it.
 */
const char *optipng_version(void);

#ifdef __cplusplus
}
#endif

#endif /* OPTIPNG_RS_H */
