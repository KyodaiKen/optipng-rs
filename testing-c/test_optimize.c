/*
 * C integration test for liboptipng-rs (the C ABI surface).
 *
 * Usage: optipng_c_test <input.png> <output.png> [opt_level] [mt]
 *
 * Decodes a PNG into raw unfiltered pixels with libpng, then hands the
 * pixels + image metadata to optipng_optimize(), inspects the result through
 * the accessor functions, writes the optimized PNG bytes back out, and frees
 * every caller-owned resource. Exercises the full C lifecycle:
 *   decode (libpng) -> optipng_optimize -> accessors
 *                 -> write output -> optipng_result_free -> optipng_free_string
 *
 * Build (inside the devenv container, where libpng + gcc are present):
 *   gcc -std=c11 -O2 -I. testing-c/test_optimize.c \
 *       -Ltarget/release -lliboptipng_rs \
 *       -Wl,-rpath,<abs>/target/release -lpng -lm \
 *       -o target/optipng_c_test
 */
#include "optipng-rs.h"

#include <png.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Progress callback */
static void progress_cb(size_t completed, size_t total, void *user_data) {
    (void)user_data;
    int pct = (total > 0) ? (int)(completed * 100 / total) : 0;
    printf("\r    progress: %3d%%", pct);
    fflush(stdout);
    if (completed >= total) { printf("\n"); fflush(stdout); }
}

/* Sum the total IDAT payload size by walking the PNG chunk stream. */
static size_t sum_idat(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return 0;
    unsigned char hdr[8];
    if (fread(hdr, 1, 8, f) != 8) { fclose(f); return 0; }
    size_t total = 0;
    for (;;) {
        unsigned char b[8];
        if (fread(b, 1, 8, f) != 8) break;
        unsigned int len = (unsigned int)b[0] << 24
                         | (unsigned int)b[1] << 16
                         | (unsigned int)b[2] << 8
                         | (unsigned int)b[3];
        const char *type = (const char *)(b + 4);
        if (!memcmp(type, "IEND", 4)) break;
        if (!memcmp(type, "IDAT", 4)) total += len;
        if (fseek(f, (long)len + 4, SEEK_CUR) != 0) break; /* skip data + CRC */
    }
    fclose(f);
    return total;
}

/*
 * Decode a PNG into a flat, unfiltered pixel buffer using libpng's default
 * expansion (palette->RGB, 1/2/4-bit->8-bit, tRNS->alpha). Returns 0 on
 * success and fills the out-params, non-zero on failure.
 */
static int decode_png(const char *path,
                      unsigned char **out_pixels,
                      uint32_t *out_w, uint32_t *out_h,
                      uint8_t *out_bd, uint8_t *out_ct,
                      size_t *out_stride) {
    FILE *f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "decode_png: cannot open '%s'\n", path); return -1; }

    png_structp png = png_create_read_struct(PNG_LIBPNG_VER_STRING, NULL, NULL, NULL);
    if (!png) { fclose(f); return -1; }
    png_infop info = png_create_info_struct(png);
    if (!info) { png_destroy_read_struct(&png, NULL, NULL); fclose(f); return -1; }

    int rc = -1;
    if (setjmp(png_jmpbuf(png))) {
        goto cleanup; /* error path */
    }

    png_init_io(png, f);
    png_read_info(png, info);

    /* Deterministic expansion toward a format the encoder understands. */
    int bd = png_get_bit_depth(png, info);
    int ct = png_get_color_type(png, info);
    if (ct == PNG_COLOR_TYPE_PALETTE) {
        png_set_palette_to_rgb(png);
    }
    if (bd < 8) {
        png_set_expand(png);
    }
    if (png_get_valid(png, info, PNG_INFO_tRNS)) {
        png_set_tRNS_to_alpha(png);
    }

    png_read_update_info(png, info);

    *out_w  = png_get_image_width(png, info);
    *out_h  = png_get_image_height(png, info);
    *out_bd = (uint8_t)png_get_bit_depth(png, info);
    *out_ct = (uint8_t)png_get_color_type(png, info);

    size_t rowbytes = png_get_rowbytes(png, info);
    *out_stride = rowbytes;

    size_t total = (size_t)rowbytes * *out_h;
    unsigned char *pixels = (unsigned char *)malloc(total);
    if (!pixels) { fprintf(stderr, "decode_png: OOM (%zu bytes)\n", total); goto cleanup; }

    png_bytep *rows = (png_bytep *)malloc(sizeof(png_bytep) * *out_h);
    if (!rows) { free(pixels); goto cleanup; }
    for (uint32_t y = 0; y < *out_h; y++) {
        rows[y] = pixels + (size_t)y * rowbytes;
    }

    png_read_image(png, rows);
    png_read_end(png, info);
    free(rows);

    *out_pixels = pixels;
    rc = 0;

cleanup:
    png_destroy_read_struct(&png, &info, NULL);
    fclose(f);
    return rc;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <input.png> <output.png> [opt_level] [mt]\n", argv[0]);
        return 2;
    }
    const char *in_path = argv[1];
    const char *out_path = argv[2];
    unsigned int opt_level = (argc >= 4) ? (unsigned int)atoi(argv[3]) : 2;
    unsigned int mt = (argc >= 5) ? (unsigned int)atoi(argv[4]) : 1;

    printf("optipng-rs C test\n");
    printf("  library version : %s\n", optipng_version());
    printf("  input           : %s\n", in_path);
    printf("  output          : %s\n", out_path);
    printf("  opt_level       : %u\n", opt_level);
    printf("  mt              : %u\n\n", mt);

    size_t orig_idat = sum_idat(in_path);
    printf("[1] original IDAT size : %zu bytes\n", orig_idat);

    unsigned char *pixels = NULL;
    uint32_t w = 0, h = 0;
    uint8_t bd = 0, ct = 0;
    size_t stride = 0;
    if (decode_png(in_path, &pixels, &w, &h, &bd, &ct, &stride) != 0) {
        fprintf(stderr, "ERROR: failed to decode input PNG\n");
        return 1;
    }
    size_t input_len = stride * (size_t)h;
    printf("[2] decoded: %ux%u bit_depth=%u color_type=%u stride=%zu input_len=%zu bytes\n",
           w, h, bd, ct, stride, input_len);

    OptipngParams params;
    memset(&params, 0, sizeof(params));
    params.opt_level = (uint8_t)(opt_level > 5 ? 5 : opt_level);
    params.mt = mt;
    params.reduce_format = 1;
    params.progress_cb = progress_cb;
    params.progress_user_data = NULL;

    printf("[3] calling optipng_optimize() ...\n");
    OptimizeResult *res = optipng_optimize(
        pixels, input_len,
        w, h, bd, ct,
        stride, orig_idat,
        &params
    );
    free(pixels);
    if (!res) {
        char *err = optipng_get_last_error();
        fprintf(stderr, "ERROR: optimization failed: %s\n", err ? err : "(no message)");
        optipng_free_string(err);
        return 1;
    }
    printf("[4] got result handle (non-NULL)\n");

    const uint8_t *result_bytes = optipng_result_bytes(res);
    size_t result_len = optipng_result_len(res);
    if (!result_bytes) {
        fprintf(stderr, "ERROR: failed to read result bytes\n");
        optipng_result_free(res);
        return 1;
    }

    size_t best_idat = optipng_result_best_idat_size(res);
    int32_t is_smaller = optipng_result_is_smaller(res);

    int32_t zc = -1, zm = -1, zs = -1;
    uint8_t f = 0;
    optipng_result_zc(res, &zc);
    optipng_result_zm(res, &zm);
    optipng_result_zs(res, &zs);
    optipng_result_f(res, &f);

    printf("[5] result summary:\n");
    printf("     result size     : %zu bytes\n", result_len);
    printf("     orig IDAT size  : %zu bytes\n", optipng_result_orig_idat_size(res));
    printf("     best IDAT size  : %zu bytes\n", best_idat);
    printf("     zc (clevel)     : %d\n", zc);
    printf("     zm (method)     : %d\n", zm);
    printf("     zs (strategy)   : %d\n", zs);
    printf("     f (filters)     : %u\n", f);
    printf("     is_smaller      : %s\n", is_smaller ? "true" : "false");

    /* Sanity: result must be a valid PNG (signature check). */
    const uint8_t png_sig[8] = {0x89, 'P','N','G', 0x0D, 0x0A, 0x1A, 0x0A};
    if (result_len < 8 || memcmp(result_bytes, png_sig, 8) != 0) {
        fprintf(stderr, "ERROR: result is not a valid PNG (bad signature)\n");
        optipng_result_free(res);
        return 1;
    }
    printf("[6] result is a valid PNG (signature OK)\n");

    FILE *out = fopen(out_path, "wb");
    if (!out) {
        fprintf(stderr, "ERROR: cannot open output '%s'\n", out_path);
        optipng_result_free(res);
        return 1;
    }
    if (fwrite(result_bytes, 1, result_len, out) != result_len) {
        fprintf(stderr, "ERROR: short write to output\n");
        fclose(out);
        optipng_result_free(res);
        return 1;
    }
    fclose(out);
    printf("[7] wrote %zu bytes to '%s'\n", result_len, out_path);

    optipng_result_free(res);
    printf("[8] freed result handle\n");

    printf("\nOK: C FFI lifecycle completed successfully.\n");
    return 0;
}
