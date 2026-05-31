//! Plugin loader.
//!
//! Phase 2 (this commit): actually opens the dynamic library via
//! `libloading::Library::new`, probes the ABI version, resolves the five
//! required entry-point symbols, calls `ai_hands_plugin_init`, walks the
//! returned `PluginInfo` + tool descriptor array, and hands the resulting
//! `LoadedPlugin` plus the live `Library` handle to the registry via
//! `registry::insert_with_library`. The `Library` is held in the registry
//! for the plugin's lifetime so cached function pointers stay valid.
//!
//! The `PhaseNotImplemented` variant is retained in `LoadError` (now
//! `#[allow(dead_code)]`-tagged via the module-level allow) so existing
//! callers/tests that constructed it still compile. Phase 1 callers that
//! depended on `Err(PhaseNotImplemented)` now see real load errors
//! (`InitFailed`, `SymbolMissing`, `AbiVersionMismatch`) when the path
//! exists but isn't a valid plugin.

// Several `LoadError` variants are constructed only via FFI failure paths
// that are exercised by integration tests gated behind the example
// plugin DLL. Keep the allow so a plain `cargo check` stays quiet.
#![allow(dead_code)]

use super::registry::LoadedPlugin;

/// Errors returned by `load_from_path`. Variants are part of the host's
/// public error surface — do not reorder.
#[derive(Debug)]
pub enum LoadError {
    /// Retained from phase 1 for source compatibility; never returned by
    /// the phase-2 loader. Existing tests that match on this variant still
    /// compile.
    PhaseNotImplemented,
    /// The provided path does not exist on disk.
    PathNotFound(String),
    /// Plugin's reported major version does not match the host's.
    AbiVersionMismatch { expected: u32, actual: u32 },
    /// One of the required `ai_hands_plugin_*` symbols was not exported.
    SymbolMissing(String),
    /// `ai_hands_plugin_init` returned NULL, the library failed to open,
    /// or registry insertion failed. The message includes underlying
    /// detail for debugging.
    InitFailed(String),
}

/// Attempt to load a plugin from a dynamic-library path.
///
/// Steps:
///   1. Verify the path exists.
///   2. `libloading::Library::new(path)` — `LoadLibrary` on Windows,
///      `dlopen` on Unix. Failures map to `InitFailed`.
///   3. Resolve `ai_hands_plugin_abi_version` and check the major version
///      matches `abi::ABI_VERSION_MAJOR`.
///   4. Resolve the remaining four required symbols so we fail fast if
///      anything is missing.
///   5. Call `ai_hands_plugin_init`; reject a NULL return.
///   6. Walk the returned `PluginInfo` + tool descriptor array, copying
///      every C string to an owned `String`.
///   7. Hand the `LoadedPlugin` + `Library` to the registry.
///
/// SAFETY: dynamic library loading is inherently unsafe. We trust the
/// plugin author to have built against the ABI declared in
/// `installers/plugin-abi/ai_hands_plugin.h` and to honor the lifetime
/// rules for the strings exposed via `PluginInfo`.
pub fn load_from_path(path: &str) -> Result<LoadedPlugin, LoadError> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(LoadError::PathNotFound(path.to_string()));
    }

    // SAFETY: `Library::new` runs the platform loader on `path`; the file
    // is trusted to be a well-formed dynamic library exporting the AI-Hands
    // plugin ABI. We cannot validate that without actually loading it.
    let lib = unsafe { libloading::Library::new(path) }
        .map_err(|e| LoadError::InitFailed(format!("Library::new failed: {e}")))?;

    // ABI version probe — major must match exactly; minor is informational.
    // SAFETY: symbol returns a plain `u32`; calling it is sound as long as
    // the plugin honors the declared C signature, which is part of the ABI
    // contract.
    let abi_version_fn: libloading::Symbol<super::abi::AbiVersionFn> = unsafe {
        lib.get(super::abi::SYM_ABI_VERSION)
            .map_err(|e| LoadError::SymbolMissing(format!("ai_hands_plugin_abi_version: {e}")))?
    };
    let plugin_abi_packed = unsafe { abi_version_fn() };
    let plugin_major = plugin_abi_packed >> 16;
    let plugin_minor = plugin_abi_packed & 0xFFFF;
    if plugin_major != super::abi::ABI_VERSION_MAJOR {
        return Err(LoadError::AbiVersionMismatch {
            expected: super::abi::ABI_VERSION_MAJOR,
            actual: plugin_major,
        });
    }

    // Resolve the remaining required symbols before we call init so we
    // fail fast if anything's missing. We probe (and intentionally
    // discard) `call`, `free_string`, and `shutdown` — `invoke_tool` /
    // `unload` will re-resolve them later from the same `Library`.
    //
    // SAFETY: same contract as above — these are part of the ABI surface
    // every plugin must export.
    let init_fn: libloading::Symbol<super::abi::PluginInitFn> = unsafe {
        lib.get(super::abi::SYM_INIT)
            .map_err(|e| LoadError::SymbolMissing(format!("ai_hands_plugin_init: {e}")))?
    };
    let _call_fn_probe: libloading::Symbol<super::abi::PluginCallFn> = unsafe {
        lib.get(super::abi::SYM_CALL)
            .map_err(|e| LoadError::SymbolMissing(format!("ai_hands_plugin_call: {e}")))?
    };
    let _free_fn_probe: libloading::Symbol<super::abi::PluginFreeStringFn> = unsafe {
        lib.get(super::abi::SYM_FREE_STRING)
            .map_err(|e| LoadError::SymbolMissing(format!("ai_hands_plugin_free_string: {e}")))?
    };
    let _shutdown_fn_probe: libloading::Symbol<super::abi::PluginShutdownFn> = unsafe {
        lib.get(super::abi::SYM_SHUTDOWN)
            .map_err(|e| LoadError::SymbolMissing(format!("ai_hands_plugin_shutdown: {e}")))?
    };

    // SAFETY: calling the plugin's init function. The plugin contract
    // promises a non-NULL `*const PluginInfo` on success.
    let info_ptr = unsafe { init_fn() };
    if info_ptr.is_null() {
        return Err(LoadError::InitFailed(
            "ai_hands_plugin_init returned NULL".to_string(),
        ));
    }

    // SAFETY: the plugin guarantees `PluginInfo` (and the strings + tool
    // descriptor array it references) lives for the plugin's lifetime per
    // the ABI contract. We only borrow it briefly to copy fields out.
    let info = unsafe { &*info_ptr };
    let name = unsafe { c_str_to_string(info.name) };
    let version = unsafe { c_str_to_string(info.version) };
    let author = unsafe { c_str_to_string(info.author) };
    let description = unsafe { c_str_to_string(info.description) };

    let mut tools = Vec::with_capacity(info.tool_count as usize);
    for i in 0..info.tool_count as isize {
        // SAFETY: descriptor array length is `info.tool_count`, indices
        // `[0, tool_count)` are in-bounds by contract.
        let td = unsafe { &*info.tools.offset(i) };
        tools.push(super::registry::LoadedTool {
            name: unsafe { c_str_to_string(td.name) },
            description: unsafe { c_str_to_string(td.description) },
            input_schema_json: unsafe { c_str_to_string(td.input_schema_json) },
        });
    }

    let plugin = LoadedPlugin {
        name: name.clone(),
        version,
        author,
        description,
        tools,
        abi_version_major: plugin_major,
        abi_version_minor: plugin_minor,
        library_path: path.to_string(),
        loaded_at: chrono::Utc::now().to_rfc3339(),
    };

    // Hand the live library off to the registry so it stays alive while
    // anyone might invoke a tool on this plugin. If the registry rejects
    // (duplicate name), we drop both `plugin` and `lib` here, which calls
    // the OS unloader. The plugin already ran its `init` side effects —
    // we cannot undo them, but a duplicate-name collision indicates a
    // host bug, not a plugin failure.
    super::registry::insert_with_library(plugin.clone(), lib)
        .map_err(|e| LoadError::InitFailed(format!("registry insert: {e}")))?;

    Ok(plugin)
}

/// Copy a C-side `*const c_char` to an owned `String`, replacing
/// non-UTF-8 sequences with U+FFFD. Returns an empty `String` for NULL,
/// which keeps `info.name == NULL` from blowing up downstream — the
/// resulting `LoadedPlugin.name == ""` will fail the registry's
/// duplicate-rejection logic the moment a second misbehaving plugin
/// shows up, which is a deliberate fail-loud signal.
///
/// SAFETY: caller must guarantee `p` is either NULL or a valid
/// NUL-terminated C string that lives at least as long as this call.
unsafe fn c_str_to_string(p: *const std::ffi::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_from_path_returns_path_not_found_for_missing_path() {
        // Pick a path that's vanishingly unlikely to exist.
        let bogus =
            r"C:\__definitely_not_a_real_plugin_path__\plugin_phase1_stub_does_not_exist.dll";
        match load_from_path(bogus) {
            Err(LoadError::PathNotFound(p)) => assert_eq!(p, bogus),
            other => panic!("expected PathNotFound, got {:?}", other),
        }
    }

    #[test]
    fn rejects_path_that_isnt_a_library() {
        // Create a real file that isn't a dynamic library. Phase 2 tries
        // to `Library::new` it — the platform loader rejects, so we
        // expect `InitFailed` (which wraps the loader's message). We do
        // NOT pin the exact message text because it differs per OS.
        let mut f = tempfile::NamedTempFile::new().expect("tempfile create");
        writeln!(f, "this is not actually a plugin").unwrap();
        let path = f.path().to_string_lossy().into_owned();

        match load_from_path(&path) {
            Err(LoadError::InitFailed(msg)) => {
                assert!(
                    msg.contains("Library::new failed") || msg.contains("library"),
                    "expected loader error, got: {msg}"
                );
            }
            // On some platforms the loader will return a symbol-missing
            // error if it accidentally succeeds at opening a junk file
            // (very rare). Either is acceptable; just don't accept
            // PhaseNotImplemented or PathNotFound.
            Err(LoadError::SymbolMissing(_)) => {}
            other => panic!("expected InitFailed or SymbolMissing, got {:?}", other),
        }
    }

    #[test]
    fn load_error_variants_are_debug_formattable() {
        // Each variant must be Debug-printable so the MCP handler can
        // surface unknown failures via `format!("{:?}", e)`.
        let variants = [
            LoadError::PhaseNotImplemented,
            LoadError::PathNotFound("x".into()),
            LoadError::AbiVersionMismatch {
                expected: 1,
                actual: 2,
            },
            LoadError::SymbolMissing("ai_hands_plugin_init".into()),
            LoadError::InitFailed("returned null".into()),
        ];
        for v in &variants {
            let s = format!("{:?}", v);
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn c_str_to_string_handles_null() {
        // SAFETY: explicitly passing NULL — the helper's NULL branch is
        // the whole point of this test.
        let s = unsafe { c_str_to_string(std::ptr::null()) };
        assert!(s.is_empty(), "NULL must convert to an empty string");
    }

    #[test]
    fn c_str_to_string_extracts_utf8() {
        let owned = std::ffi::CString::new("hello, plugins").unwrap();
        // SAFETY: pointer is to a live CString owned by this scope.
        let s = unsafe { c_str_to_string(owned.as_ptr()) };
        assert_eq!(s, "hello, plugins");
    }

    /// Integration: build the example plugin first, then verify
    /// `load_from_path` walks the ABI end-to-end. Gated `#[ignore]` so
    /// the default `cargo test` run doesn't depend on a built DLL.
    ///
    /// Run locally with:
    ///   cargo build --release --manifest-path \
    ///       installers/plugin-abi/example/Cargo.toml
    ///   cargo test --bin hands -- --ignored loads_example_plugin
    #[test]
    #[ignore = "requires the example plugin to be built — see test docs"]
    fn loads_example_plugin_successfully() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        #[cfg(target_os = "windows")]
        let dll = "example_plugin.dll";
        #[cfg(target_os = "macos")]
        let dll = "libexample_plugin.dylib";
        #[cfg(all(unix, not(target_os = "macos")))]
        let dll = "libexample_plugin.so";
        let path = format!("{manifest}/installers/plugin-abi/example/target/release/{dll}");
        assert!(
            std::path::Path::new(&path).exists(),
            "example plugin not built at {path}; run `cargo build --release --manifest-path installers/plugin-abi/example/Cargo.toml` first"
        );

        let plugin = load_from_path(&path).expect("load_from_path");
        assert_eq!(plugin.name, "example_plugin");
        assert!(!plugin.tools.is_empty(), "expected at least one tool");
        assert!(
            plugin.tools.iter().any(|t| t.name == "example_echo"),
            "expected example_echo tool: {:?}",
            plugin.tools
        );
        assert_eq!(
            plugin.abi_version_major,
            super::super::abi::ABI_VERSION_MAJOR
        );

        // Cleanup so subsequent --ignored runs don't trip the
        // duplicate-name check.
        super::super::registry::unload(&plugin.name);
    }
}
