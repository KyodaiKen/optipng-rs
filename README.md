# optipng-rs

A high-performance, multi-threaded Rust reimplementation of **OptiPNG**. 

Designed for modern multi-core systems, `optipng-rs` achieves high compression ratios using combinatorial parameter searching, zero-allocation zlib streaming, adaptive filter optimization, and smart bit-depth/alpha channel reduction.

---

## ⚡ Key Features & Performance

* **True Multi-Threaded Work-Stealing:** Utilizes dynamic work-stealing queues (`AtomicUsize`) across trial permutations, ensuring **100% CPU core utilization** across threads even when individual trial durations vary wildly.
* **Direct IDAT Streaming:** Computes expected zlib compression sizes during trial passes and streams the winning compressed data directly into the PNG container in a single pass without holding huge compressed chunks in RAM.
* **Zero-Wear File Swapping:** Performs atomic metadata pointer swaps (`fs::rename`) on file replace, preventing extra disk writes, reducing SSD wear, and providing instant replacement regardless of file size.
* **Lossless Color & Bit Depth Reductions:**
  * **Fake 16-Bit Detection:** Automatically identifies 16-bit-per-channel images where $MSB == LSB$ (bit duplication) and lossless converts them to 8-bit.
  * **Alpha Channel Stripping:** Inspects alpha channels and strips alpha components if an image is 100% opaque (e.g., RGBA $\rightarrow$ RGB).
* **Adaptive Filtering ($f=5$):** Full support for standard PNG filters (None, Sub, Up, Average, Paeth) and an adaptive heuristic using signed Sum of Absolute Differences (SAD) scoring to mirror OptiPNG heuristic outcomes.
* **IDAT Payload Validation:** Evaluates raw IDAT compressed payloads against original source IDAT payload sizes to guarantee that files are never overwritten unless a genuine size reduction is achieved.
* **Encode Tagra (TGA), PPM/PAM images directly to an optimized PNG** Simply add `-e myimage.tga` before the output PNG file name to encode a raw image to PNG.

---

## ⚠️ Known Differences & Missing Features (vs. Original OptiPNG)

While `optipng-rs` matches or exceeds OptiPNG's speed and trial optimization capabilities, the following features of original OptiPNG are **currently not implemented**:

1. **All Optional Metadata is Stripped:**
   * *OptiPNG:* Preserves optional metadata chunks (e.g., `gAMA`, `pHYs`, `tIME`, `tEXt`, `zTXt`, `cHRM`) by default unless requested otherwise.
   * *`optipng-rs`:* **Strips all optional metadata chunks**. Only structural headers (`IHDR`, `IDAT`, `IEND`) are written to the output file.
2. **No Adam7 Interlacing Support:**
   * *OptiPNG:* Can read, write, or de-interlace Adam7 PNG files (`-interlace 0/1`).
   * *`optipng-rs`:* Rejects Adam7 interlaced images during decoding.
3. **Missing Ancillary Utility Flags:**
   * Advanced OptiPNG repair and snippet utilities like `-fix` (recovery of corrupt PNG files), `-force`, `-snip`, or custom chunk preservation filters are not present.
4. **No APNG support**

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
optipng-rs [options] <file1.png> <file2.png> ...
```

### Options

| Flag | Argument | Description |
| :--- | :--- | :--- |
| `-o` | `<0-7>` | Optimization preset level (default: `2`). |
| `-mt` | `<num>` | Number of parallel worker threads (default: $CPU \div 4$, min 1). |
| `-zc` | `<range>` | Custom zlib compression levels (e.g. `1-9` or `9`). |
| `-zm` | `<range>` | Custom zlib memory levels (e.g. `8,9`). |
| `-zs` | `<range>` | Custom zlib strategy levels (e.g. `0-3`). |
| `-f` | `<range>` | Custom PNG delta filter list (e.g. `0,5`). |
| `-backup` / `-keep` | | Keep backup of original files (`.bak`). |
| `-nc` | | Disable color type / alpha stripping reductions. |
| `-simulate` | | Run trials without modifying any files on disk. |
| `-quiet` / `-silent` | | Suppress output messages. |
| `-e` | <filename> | **NEW in 0.1.5!** Encode TGA, PPM, PGM or PAM directly to an optimized PNG! |

### Examples

```bash
# Optimize an image with preset level 5 using 8 worker threads
optipng-rs -o5 -mt8 image.png

# Run custom trial ranges across all filters
optipng-rs -zc 9 -zm 8,9 -zs 0-3 -f 0-5 image.png

# Encode TGA directly to an optimized PNG
optipng-rs -o5 -e my.tga my.png

# Encode PAM directly to an optimized PNG
optipng-rs -o5 -e my.pam my.png
```

---

## 📦 Container / Toolbox Note

If running `optipng-rs` inside a **Toolbox** or rootless container where host mount boundaries break POSIX `rename()` across mounts, run the host binary directly via `flatpak-spawn` to maintain fast zero-SSD-wear file swaps:

```bash
flatpak-spawn --host ./target/release/optipng-rs image.png
```

---

## 📄 License

Distributed under the [MIT License](LICENSE).
