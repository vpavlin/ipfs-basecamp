//! ipfs_module: one IPFS node (a bundled Kubo) for every Logos app on the device.
//!
//! The supervisor (`kubo`) is plain Rust, tested with
//! `FV_KUBO_BIN=… cargo test --no-default-features -- --ignored`; the Logos glue is
//! behind the default `logos_module` feature.

pub mod kubo;

/// Directory of the shared object this code lives in: the bundled `ipfs` binary
/// is staged beside the plugin in the package variant.
pub fn plugin_dir() -> Option<std::path::PathBuf> {
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(plugin_dir as *const libc::c_void, &mut info) == 0 || info.dli_fname.is_null() {
            return None;
        }
        let path = std::ffi::CStr::from_ptr(info.dli_fname).to_string_lossy().into_owned();
        std::path::Path::new(&path).parent().map(|p| p.to_path_buf())
    }
}

#[cfg(feature = "logos_module")]
mod glue;
