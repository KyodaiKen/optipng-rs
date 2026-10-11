//! C / C++ FFI layer for `liboptipng_rs`.
//!
//! The public entry point is [`optipng_optimize`]: hand it a PNG byte buffer
//! plus a small parameter set and it returns an opaque handle. Accessors read
//! the optimized byte stream and metadata back; [`optipng_result_free`] drops
//! the handle. The only thing that crosses the language boundary is the
//! `Box<T>` pointer, so no Rust type layout leaks into C.
//!
//! Panic safety: the workspace builds with `panic = 'abort'` (release), so a
//! Rust panic aborts the process instead of unwinding across `extern "C"`
//! (which would be UB). In debug builds we additionally wrap every FFI body in
//! `std::panic::catch_unwind` and return null / a stable error string.
//!
//! # Error reporting
//!
//! Errors surface two ways, matching the decoder's convention
//! (`get_last_error_dec`):
//!
//! 1. `optipng_optimize` returns a **null** handle on failure, and
//! 2. [`optipng_get_last_error`] returns a caller-freed `CString` describing
//!    the most recent error (thread-local, so safe under `-mt`).
//!
//! The C header is `optipng-rs.h`.

use std::os::raw::c_void;
use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;
use std::panic;
use std::ptr;

use crate::models::{LoadedImage, OptimizationTask};
use crate::pipeline::{optimize_memory, MemoryOptimizationResult};

/// Opaque result handle. The C side must not dereference it; use the accessor
/// functions in this module and finish with [`optipng_result_free`].
pub struct OptimizeResult {
    pub inner: MemoryOptimizationResult,
}

// ─────────────────────────────────────────────────────────────────────────────
// Thread-local error channel
// ─────────────────────────────────────────────────────────────────────────────

thread_local! {
    static LAST_ERROR: RefCell<String> = RefCell::new(String::new());
}

fn set_last_error(msg: &str) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.to_string());
}

/// Returns the most recent error message as a newly-allocated NUL-terminated
/// C string, or null if there is no error. The caller owns the returned
/// pointer and must release it with [`optipng_free_string`].
#[unsafe(no_mangle)]
pub extern "C" fn optipng_get_last_error() -> *mut c_char {
    let msg = LAST_ERROR.with(|e| e.borrow().clone());
    if msg.is_empty() {
        ptr::null_mut()
    } else {
        CString::new(msg).map(|c| c.into_raw()).unwrap_or(ptr::null_mut())
    }
}

/// Releases a C string previously returned by [`optipng_get_last_error`].
/// Passing null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe { drop(CString::from_raw(ptr)); }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Parameter struct (#[repr(C)], so it can be passed by value from C)
// ─────────────────────────────────────────────────────────────────────────────

/// Optimization parameters, passed by value from C.
///
/// Fields that are `0` use the library default. To reproduce an optipng CLI
/// invocation, mirror the flag → field mapping in `optipng-rs/src/cli.rs`.
///
/// The `zi == 2` (Zopfli) path is selected purely by this field; the `zc` /
/// `zm` / `zs` / `f` override vectors are ignored on that path (the library
/// builds the Zopfli trial list itself), which matches the CLI behaviour.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OptipngParams {
    /// Optimization level 0..=5 (maps to CLI `-o`; the CLI clamps `-o 7` down
    /// to this range for the in-memory pipeline). Default 2.
    pub opt_level: u8,
    /// Max worker threads (`-mt`). Clamped to 1..=trial count internally. 0
    /// means "use all available cores".
    pub mt: u32,
    /// Zopfli implementation selector (`-zi`). 0/1 = flate2/zlib, 2 = Zopfli.
    pub zi: u8,
    /// Apply color/bit-depth reduction before compression (`-nc` disables
    /// reduction when this is 0). 1 = reduce (default), 0 = skip.
    pub reduce_format: u8,
    /// `-nz` (no-reencode): re-encode the IDAT with the best trial only,
    /// skipping format-reduction trials. 1 = on.
    pub nz: u8,
    /// `-nc` (no-change): compress at the given bit depth / color type only.
    pub nc: u8,
    /// `-nb` (no-baseline): skip baseline trials.
    pub nb: u8,
    /// `-np` (no-palette): skip palette-based trials.
    pub np: u8,
    /// `-nx` (no-extended): skip extended (16-bit) trials.
    pub nx: u8,
    /// Progress callback: (completed_trials, total_trials, user_data), or NULL.
    pub progress_cb: Option<extern "C" fn(usize, usize, *mut c_void)>,
    /// Opaque pointer passed back to progress_cb unchanged.
    pub progress_user_data: *mut c_void,
}

/// FFI user-data pointer that is safe to share across the worker threads
/// invoking the progress callback. The pointer is opaque: it is owned by the
/// C side and only ever passed back to the caller's `extern "C"` callback,
/// so treating it as `Send + Sync` is sound.
struct CUserPtr(*mut c_void);
unsafe impl Send for CUserPtr {}
unsafe impl Sync for CUserPtr {}
impl CUserPtr {
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Default for OptipngParams {
    fn default() -> Self {
        Self {
            opt_level: 2,
            mt: 0,
            zi: 0,
            reduce_format: 1,
            nz: 0,
            nc: 0,
            nb: 0,
            np: 0,
            nx: 0,
            progress_cb: None,
            progress_user_data: std::ptr::null_mut(),
        }
    }
}

impl OptipngParams {
    fn to_task(
        &self,
        input: Vec<u8>,
        width: u32,
        height: u32,
        bit_depth: u8,
        color_type: u8,
        stride: usize,
        orig_idat_size: usize,
    ) -> OptimizationTask {
        let loaded = LoadedImage {
            width,
            height,
            bit_depth,
            color_type,
            stride,
            raw_pixels: input,
            orig_idat_size,
        };
        let mut task = OptimizationTask::default();
        task.opt_level = self.opt_level.min(5);
        // 0 means "all cores".
        task.mt = if self.mt == 0 {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        } else {
            self.mt as usize
        };
        task.zi = self.zi;
        task.reduce_format = self.reduce_format != 0;
        task.nz = self.nz != 0;
        task.nc = self.nc != 0;
        task.nb = self.nb != 0;
        task.np = self.np != 0;
        task.nx = self.nx != 0;
        task.in_memory_source = Some(loaded);
        if let Some(cb) = self.progress_cb {
            let user = CUserPtr(self.progress_user_data);
            task.progress_callback = Some(Box::new(move |completed, total| {
                cb(completed, total, user.as_ptr())
            }));
        }
        task
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Optimize a PNG in memory.
///
/// # Safety
///
/// - `input` must point to a valid buffer of at least `input_len` bytes, and
///   those bytes must remain valid for the duration of the call.
/// - `params`, if non-null, must point to a valid, fully-initialized
///   `OptipngParams`.
///
/// # Errors
///
/// Returns null on failure; call [`optipng_get_last_error`] for details.
///
/// The returned handle is opaque — read fields with `optipng_result_*` and
/// release with [`optipng_result_free`].
#[unsafe(no_mangle)]
pub extern "C" fn optipng_optimize(
    input: *const u8,
    input_len: usize,
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    stride: usize,
    orig_idat_size: usize,
    params: *const OptipngParams,
) -> *mut OptimizeResult {
    // Validate up front so the catch_unwind body never sees a null deref.
    if input.is_null() && input_len > 0 {
        set_last_error("optipng_optimize: input is null but input_len > 0");
        return ptr::null_mut();
    }

    let params = if params.is_null() {
        OptipngParams::default()
    } else {
        unsafe { *params }
    };

    let input_slice = if input_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(input, input_len).to_vec() }
    };

    let task = params.to_task(
        input_slice,
        width,
        height,
        bit_depth,
        color_type,
        stride,
        orig_idat_size,
    );

    // Wrap the call so a panic in debug mode returns null instead of
    // unwinding across the FFI boundary (UB in release).
    let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| optimize_memory(task)));
    match outcome {
        Ok(Ok(result)) => {
            set_last_error("");
            Box::into_raw(Box::new(OptimizeResult { inner: result }))
        }
        Ok(Err(e)) => {
            set_last_error(&e);
            ptr::null_mut()
        }
        Err(_) => {
            set_last_error("optipng_optimize: Rust panic");
            ptr::null_mut()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Accessors
// ─────────────────────────────────────────────────────────────────────────────

/// Returns a pointer to the optimized PNG byte stream.
///
/// The pointer is valid until the handle is freed with
/// [`optipng_result_free`]. The buffer is owned by the Rust side — do **not**
/// free it from C.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_bytes(handle: *const OptimizeResult) -> *const u8 {
    if handle.is_null() {
        return ptr::null();
    }
    unsafe { (*handle).inner.optimized_bytes.as_ptr() }
}

/// Length in bytes of the optimized PNG stream (see [`optipng_result_bytes`]).
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_len(handle: *const OptimizeResult) -> usize {
    if handle.is_null() {
        return 0;
    }
    unsafe { (*handle).inner.optimized_bytes.len() }
}

/// The original IDAT payload size (before optimization).
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_orig_idat_size(handle: *const OptimizeResult) -> usize {
    if handle.is_null() {
        return 0;
    }
    unsafe { (*handle).inner.orig_idat_size }
}

/// The winning (smallest) IDAT payload size after optimization.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_best_idat_size(handle: *const OptimizeResult) -> usize {
    if handle.is_null() {
        return 0;
    }
    unsafe { (*handle).inner.best_idat_size }
}

/// 1 if the optimized IDAT is smaller than the original, 0 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_is_smaller(handle: *const OptimizeResult) -> i32 {
    if handle.is_null() {
        return 0;
    }
    unsafe { if (*handle).inner.is_smaller { 1 } else { 0 } }
}

/// Writes the winning trial's `zc` into `*out` (0 on null handle / no winner).
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_zc(handle: *const OptimizeResult, out: *mut i32) -> i32 {
    if handle.is_null() || out.is_null() {
        return 0;
    }
    match unsafe { &(*handle).inner.best_config } {
        Some(cfg) => { unsafe { *out = cfg.zc; } 1 }
        None => 0,
    }
}

/// Writes the winning trial's `zm` into `*out`.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_zm(handle: *const OptimizeResult, out: *mut i32) -> i32 {
    if handle.is_null() || out.is_null() {
        return 0;
    }
    match unsafe { &(*handle).inner.best_config } {
        Some(cfg) => { unsafe { *out = cfg.zm; } 1 }
        None => 0,
    }
}

/// Writes the winning trial's `zs` into `*out`.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_zs(handle: *const OptimizeResult, out: *mut i32) -> i32 {
    if handle.is_null() || out.is_null() {
        return 0;
    }
    match unsafe { &(*handle).inner.best_config } {
        Some(cfg) => { unsafe { *out = cfg.zs; } 1 }
        None => 0,
    }
}

/// Writes the winning trial's `f` into `*out`.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_f(handle: *const OptimizeResult, out: *mut u8) -> i32 {
    if handle.is_null() || out.is_null() {
        return 0;
    }
    match unsafe { &(*handle).inner.best_config } {
        Some(cfg) => { unsafe { *out = cfg.f; } 1 }
        None => 0,
    }
}

/// Releases a handle previously returned by [`optipng_optimize`].
/// Passing null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_result_free(handle: *mut OptimizeResult) {
    if handle.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw(handle)); }
}

// ─────────────────────────────────────────────────────────────────────────────
// Version
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the library version as a NUL-terminated C string.
///
/// The pointer is valid for the lifetime of the program (static string) — do
/// **not** free it.
#[unsafe(no_mangle)]
pub extern "C" fn optipng_version() -> *const c_char {
    static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    VERSION.as_ptr() as *const c_char
}
