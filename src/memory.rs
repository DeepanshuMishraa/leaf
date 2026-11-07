//! Giving memory back: mupdf's cache, and large blocks macOS malloc keeps after they're freed.

use mupdf_sys::fz_context;

/// mupdf-rs keeps its context pointer to itself, and its `Debug` output
/// (`Context { inner: 0x… }`) is the one place that shows it. `None` if that
/// output ever changes shape; the cache then just isn't trimmed.
fn mupdf_context() -> Option<*mut fz_context> {
    let shown = format!("{:?}", mupdf::Context::get());
    let hex: String = shown
        .split("0x")
        .nth(1)?
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    let address = usize::from_str_radix(&hex, 16).ok().filter(|&a| a != 0)?;
    Some(std::ptr::with_exposed_provenance_mut(address))
}

/// mupdf caches what it reads (fonts, objects, decoded images) up to a
/// quarter gigabyte before it lets anything go. This evicts the least
/// recently used items until the cache is at `percent` of its size; 0 empties it.
pub fn shrink_mupdf_cache(percent: u32) {
    let Some(ctx) = mupdf_context() else { return };
    // SAFETY: `ctx` is this thread's live mupdf context, and evicting from the
    // store neither throws nor touches anything still in use.
    unsafe {
        if percent == 0 {
            mupdf_sys::fz_empty_store(ctx);
        } else {
            mupdf_sys::fz_shrink_store(ctx, percent);
        }
    }
}

/// macOS malloc keeps up to 64 freed large blocks for reuse: for a reader
/// drawing figure after figure, a few hundred megabytes held for nothing. It
/// reads `MallocLargeCache` only at launch, so leaf sets it and starts itself
/// again, once, before touching the terminal. If that fails, leaf carries on
/// with the cache.
pub fn without_large_cache() {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        const VAR: &str = "MallocLargeCache";
        if std::env::var_os(VAR).is_some() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut args = std::env::args_os();
        let mut command = std::process::Command::new(exe);
        if let Some(name) = args.next() {
            command.arg0(name);
        }
        // `exec` returns only if it failed.
        let _ = command.args(args).env(VAR, "0").exec();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_the_mupdf_context() {
        assert!(super::mupdf_context().is_some());
    }
}
