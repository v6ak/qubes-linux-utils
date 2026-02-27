// FFI bindings to libxenstore (C library).
//
// This module wraps the minimal xenstore API needed to write memory info:
//   xs_domain_open() - open a xenstore handle from a domain
//   xs_write()       - write a value to a xenstore path
//   xs_close()       - close the xenstore handle
//
// The xenstore integration is compiled only when libxenstore is detected at
// build time (see build.rs).  On machines without Xen the binary still builds
// and can be used with --debug.

#[cfg(has_xenstore)]
use std::ffi::{c_char, c_uint, c_void, CString};
use std::io;

// ── Raw FFI (only when libxenstore is present) ────────────────────────────────

#[cfg(has_xenstore)]
extern "C" {
    // Opens a connection to xenstore as a domain (unprivileged).
    fn xs_domain_open() -> *mut c_void;

    // Writes `data` of `len` bytes to `path` in transaction `t`.
    // XBT_NULL (0) is the null transaction (auto-commit).
    fn xs_write(
        h: *mut c_void,
        t: u32,
        path: *const c_char,
        data: *const c_void,
        len: c_uint,
    ) -> bool;

    // Closes the xenstore handle.
    fn xs_close(h: *mut c_void);
}

// ── Safe wrapper ──────────────────────────────────────────────────────────────

/// Safe wrapper around an open xenstore handle.
///
/// When libxenstore is not available at build time (`#[cfg(not(has_xenstore))]`
/// the type still exists but `open()` always returns an error, so callers
/// compiled on non-Xen machines get a meaningful error at runtime rather than
/// a build failure.
pub struct XsHandle {
    #[cfg(has_xenstore)]
    handle: *mut c_void,
}

impl XsHandle {
    /// Open a xenstore handle.  Returns an error if xenstore is not accessible
    /// or if the binary was compiled without xenstore support.
    pub fn open() -> io::Result<Self> {
        #[cfg(has_xenstore)]
        {
            // SAFETY: xs_domain_open() is a plain C function that returns NULL
            // on failure.  No Rust invariants are involved.
            let h = unsafe { xs_domain_open() };
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            return Ok(XsHandle { handle: h });
        }
        #[cfg(not(has_xenstore))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "xenstore support was not compiled in \
             (libxenstore not found at build time); use --output print",
        ))
    }

    /// Write `value` to the xenstore key `path`.
    pub fn write(&self, path: &str, value: &str) -> io::Result<()> {
        #[cfg(has_xenstore)]
        {
            let path_c = CString::new(path).expect("path must not contain NUL");
            let data = value.as_bytes();
            // SAFETY: all pointers are valid for the duration of the call.
            // `self.handle` is non-null (guaranteed by `open`).
            let ok = unsafe {
                xs_write(
                    self.handle,
                    0, // XBT_NULL – no explicit transaction
                    path_c.as_ptr(),
                    data.as_ptr().cast::<c_void>(),
                    data.len() as c_uint,
                )
            };
            if ok {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        #[cfg(not(has_xenstore))]
        {
            let _ = (path, value);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "xenstore support not compiled in",
            ))
        }
    }
}

impl Drop for XsHandle {
    fn drop(&mut self) {
        #[cfg(has_xenstore)]
        // SAFETY: self.handle is non-null and was returned by xs_domain_open().
        unsafe {
            xs_close(self.handle)
        };
    }
}
