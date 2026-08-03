# optipng-rs

A high-performance, multi-threaded Rust reimplementation of **OptiPNG**, optimized for modern multi-core systems.

`optipng-rs` achieves high compression ratios using combinatorial parameter searching, zero-allocation zlib streaming, adaptive filter optimization, and smart bit-depth/alpha channel reduction. It also features a fast path for fast IDAT chunk coalescing and deduplication-friendly metadata injection.

---

## ⚡ Key Features & Performance

* **True Multi-Threaded Work-Stealing:** Utilizes dynamic work-stealing queues (`AtomicUsize`) across trial permutations, ensuring **100% CPU core utilization** across threads even when individual trial durations vary wildly.
* **Zero-Recompression Fast Path (`-o0` / `-nz`):** Bypasses trial search and pixel re-encoding entirely. Reads raw IDAT streams and merges them into single coalesced chunks (up to 2GB boundaries) without decompressing pixel data or loading uncompressed frames into RAM.
* **Metadata Injection:** Strips non-essential metadata and injects a `tEXt` chunk with the key `optipng-rs` to track optimization state.
* **Direct IDAT Streaming:** Computes expected zlib compression sizes during trial passes and streams winning compressed data directly into the PNG container in a single pass without holding large compressed chunks in memory.
* **Lossless Color & Bit Depth Reductions:**
  * **Fake 16-Bit Detection:** Automatically identifies 16-bit-per-channel images where MSB == LSB (bit duplication) and losslessly converts them to 8-bit.
  * **Alpha Channel Stripping:** Inspects alpha channels and strips alpha components if an image is 100% opaque (e.g., RGBA -> RGB).
  * **Palette & Color Type Reductions:** Automatically detects and converts unnecessary color types or oversized palettes.
* **Adaptive Filtering (f=5):** Full support for standard PNG filters (None, Sub, Up, Average, Paeth) and an adaptive heuristic using signed Sum of Absolute Differences (SAD) scoring to mirror OptiPNG heuristic outcomes.
* **IDAT Payload Validation:** Evaluates raw IDAT compressed payloads against original source payload sizes to guarantee files are never overwritten unless a genuine size reduction is achieved (unless `-force` is specified).
* **Direct Image Format Encoding:** Encodes Targa (TGA), PPM, PGM, and PAM images directly into optimized PNGs via the `-e` flag.

---

## ⚠️ Known Differences & Missing Features (vs. Original OptiPNG)

While `optipng-rs` matches or exceeds OptiPNG's speed and trial optimization capabilities, note the following differences:

1. **Selective Metadata Handling:**
   * *OptiPNG:* Preserves optional metadata chunks (`gAMA`, `pHYs`, `tIME`, `tEXt`, `zTXt`, `cHRM`) by default unless requested otherwise.
   * *`optipng-rs`:* **Strips all non-essential metadata chunks** and injects a clean, uncompressed `tEXt` metadata chunk (`optipng-rs`) to log command settings and winning trial results.
2. **No Adam7 Interlacing Support:**
   * *OptiPNG:* Can read, write, or de-interlace Adam7 PNG files (`-interlace 0/1`).
   * *`optipng-rs`:* Rejects Adam7 interlaced images during decoding.
3. **Missing Ancillary Utility Flags:**
   * Advanced OptiPNG repair and snippet utilities like `-fix` (recovery of corrupt PNG files) or `-snip` are not present.
4. **No APNG Support:** Animated PNG files are not supported.

---

## 🛠️ Installation & Building

Ensure you have a modern Rust toolchain installed.

```bash
# Clone the repository
git clone https://github.com/KyodaiKen/optipng-rs.git
cd optipng-rs

# Build release binary
cargo build --release
```

The optimized binary will be available at `./target/release/optipng-rs`.

---

## 🚀 Usage

```bash
optipng-rs [options] <file1.png> [file2.png ...]
optipng-rs [options] -e <input_file> [output.png]
```

### Options (`optipng-rs --help`)

```text
optipng-rs v0.1.0 - High-performance parallel PNG optimizer and converter

USAGE:
  optipng-rs [options] <file1.png> [file2.png ...]
  optipng-rs [options] -e <input_file> [output.png]

GENERAL OPTIONS:
  -h, --help         Print this help message
  -quiet, -silent    Quiet mode (suppress non-error output)
  -mt <threads>      Number of worker threads (default: 75% of CPUs)
  -backup, -keep     Keep a backup copy of original files (.bak)
  -simulate          Simulation mode (run trials only, skip writing files)
  -force             Force file write even if compressed size increases
  -ft                Force trials even if file was previously optimized

OPTIMIZATION OPTIONS:
  -o <level>         Optimization level 0-7 (default: 2)
  -zc <levels>       zlib compression levels (e.g., -zc1-9 or -zc9)
  -zm <levels>       zlib memory levels (e.g., -zm1-9 or -zm8,9)
  -zs <strategies>   zlib compression strategies (e.g., -zs0-3)
  -f <filters>       PNG delta filter algorithms (e.g., -f0,5 or -f0-5)
  -nz                No IDAT recoding (fast path / zero re-compression)

IMAGE REDUCTION OPTIONS:
  -nb                Disable bit depth reduction
  -nc                Disable color type reduction
  -np                Disable palette reduction
  -nx                Disable all image reductions (-nb, -nc, -np)

INPUT / OUTPUT OPTIONS:
  -e <file>          External input file format (TGA, PPM, PGM, PAM) to encode
  -out <file>        Output file path
  -dir <directory>   Output directory

METADATA & DEDUPLICATION:
  * All non-essential PNG metadata is stripped during optimization.
  * A 'tEXt' chunk with key 'optipng-rs' is injected to track optimization state:
    - Line 1: User-specified command options (if present).
    - Line 2: Winning trial settings (-zc -zm -zs -f) or '-o0'.
```

### Examples

```bash
# Standard optimization level 2 using default worker threads
optipng-rs image.png

# High optimization (level 5) using 8 dedicated threads
optipng-rs -o5 -mt8 image.png

# Zero-recompression fast path (-o0 / -nz): Coalesce IDAT chunks & inject metadata instantly
optipng-rs -o0 image.png

# Force re-optimization on an image already tagged with optipng-rs metadata
optipng-rs -o2 -ft image.png

# Custom zlib trial tuning across multiple strategy and filter combinations
optipng-rs -zc9 -zm8,9 -zs0-3 -f0-5 image.png

# Encode an external format (TGA / PAM) directly into an optimized PNG
optipng-rs -o5 -e input.tga output.png
```

### Large-Scale Batch Processing with GNU `parallel`

To process large image libraries concurrently across nested directory trees while keeping the `--bar` progress display clean:

```bash
find . -type f -iname "*.png" -print0 | parallel -0 --will-cite --bar \
  'out=$(optipng-rs -o0 -mt1 -force -- {} 2>&1) || printf "Error on %s:\n%s\n" {} "$out"'
```

---

## 📄 License

Distributed under the [MIT License](LICENSE).
