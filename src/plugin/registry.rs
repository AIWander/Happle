//! In-memory registry of loaded plugins.
//!
//! Phase 2: the registry is a thread-safe `HashMap<name, LoadedPlugin>`
//! with insert/list/remove operations, plus a parallel `HashMap<name,
//! libloading::Library>` that holds the live dynamic-library handles. The
//! library map is kept separate from `LoadedPlugin` so `LoadedPlugin` can
//! stay `Clone` for snapshotting under `list()`.
//!
//! Lifetimes: a `LoadedPlugin` may outlive its `Library` (snapshots in
//! `list()` outputs are fine even after `unload`), but you must not call
//! `invoke_tool` for a plugin that's been unloaded — `unload` drops the
//! `Library`, which closes the underlying DLL/SO and invalidates every
//! pointer the plugin handed us during `init`.

// `insert`, `remove`, and `LoadedTool::input_schema_json` are reachable
// from the loader and tests; keep the allow so plain `cargo check` stays
// quiet even if a particular call site goes away in a refactor.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;

/// One loaded plugin's host-side metadata. Cloneable so callers can take a
/// snapshot of the registry without holding the lock across MCP responses.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub tools: Vec<LoadedTool>,
    pub abi_version_major: u32,
    pub abi_version_minor: u32,
    pub library_path: String,
    /// RFC3339 timestamp of when the plugin finished init.
    pub loaded_at: String,
}

/// One tool exposed by a loaded plugin. The JSON Schema string comes from
/// the plugin's `ToolDescriptor::input_schema_json` and is stored verbatim.
#[derive(Debug, Clone)]
pub struct LoadedTool {
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
}

/// Global registry, lazily initialized on first use. We use `Option<HashMap>`
/// inside the `Mutex` so the static can be `const`-initialized without
/// requiring `OnceCell` (no new external dep — phase 1 constraint).
static REGISTRY: Mutex<Option<HashMap<String, LoadedPlugin>>> = Mutex::new(None);

/// Run `f` with exclusive access to the registry map. Lazy-inits on first
/// call. Recovers from poisoning by taking the inner data so a panicked
/// thread can't permanently lock out the rest of the process.
pub fn with_registry<R>(f: impl FnOnce(&mut HashMap<String, LoadedPlugin>) -> R) -> R {
    let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

/// Snapshot of every loaded plugin, sorted by name for stable output.
pub fn list() -> Vec<LoadedPlugin> {
    with_registry(|m| {
        let mut v: Vec<LoadedPlugin> = m.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    })
}

/// Insert a plugin. Returns `Err` if a plugin with the same name is
/// already loaded — the host should reject the load attempt rather than
/// silently shadowing an earlier registration.
pub fn insert(plugin: LoadedPlugin) -> Result<(), String> {
    with_registry(|m| {
        if m.contains_key(&plugin.name) {
            return Err(format!("plugin '{}' already loaded", plugin.name));
        }
        m.insert(plugin.name.clone(), plugin);
        Ok(())
    })
}

/// Remove a plugin by name. Returns `true` if a plugin was present.
pub fn remove(name: &str) -> bool {
    with_registry(|m| m.remove(name).is_some())
}

// ---- Library storage (phase 2) ----------------------------------------
//
// We deliberately keep the live `libloading::Library` handles in a
// separate static map keyed by plugin name. Reasons:
//   1. `LoadedPlugin` derives `Clone` and gets snapshotted by `list()`;
//      `libloading::Library` is intentionally not `Clone`.
//   2. The library MUST outlive every cached function pointer we resolve
//      from it. Holding it as a process-lifetime static guarantees that
//      until `unload()` runs (which drops the entry and closes the DLL).
//
// Like `REGISTRY`, this is lazily initialized so the static can be
// `const`-constructed without bringing in OnceCell.

static LIBRARIES: Mutex<Option<HashMap<String, libloading::Library>>> = Mutex::new(None);

fn with_libraries<R>(f: impl FnOnce(&mut HashMap<String, libloading::Library>) -> R) -> R {
    let mut g = LIBRARIES.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

/// Insert a plugin alongside its live `Library` handle. Atomic with respect
/// to other callers — either both maps gain the entry or neither does.
///
/// Returns an error if a plugin with the same name is already loaded; the
/// caller should refuse the duplicate rather than silently shadow it.
pub fn insert_with_library(
    plugin: LoadedPlugin,
    library: libloading::Library,
) -> Result<(), String> {
    with_registry(|m| {
        if m.contains_key(&plugin.name) {
            return Err(format!("plugin '{}' already loaded", plugin.name));
        }
        let name = plugin.name.clone();
        m.insert(name.clone(), plugin);
        with_libraries(|libs| {
            libs.insert(name, library);
        });
        Ok(())
    })
}

/// Find which loaded plugin (if any) exposes the given tool name.
/// Returns the owning plugin's name on a match, or `None` if no plugin
/// has registered a tool with that name.
pub fn lookup_tool_owner(tool_name: &str) -> Option<String> {
    with_registry(|m| {
        for (plugin_name, plugin) in m.iter() {
            if plugin.tools.iter().any(|t| t.name == tool_name) {
                return Some(plugin_name.clone());
            }
        }
        None
    })
}

/// Invoke a tool on a loaded plugin via its `ai_hands_plugin_call` symbol.
///
/// `args_json` is forwarded verbatim to the plugin. The plugin's response
/// is freed via `ai_hands_plugin_free_string` regardless of status code, so
/// plugins never leak the response buffer back to the host.
///
/// On non-zero status the returned response (if any) is included in the
/// error message — many plugins use it to surface structured error info.
pub fn invoke_tool(
    plugin_name: &str,
    tool_name: &str,
    args_json: &str,
) -> Result<serde_json::Value, String> {
    with_libraries(|libs| {
        let lib = libs
            .get(plugin_name)
            .ok_or_else(|| format!("plugin '{plugin_name}' library not loaded"))?;

        // SAFETY: the symbols below are part of the ABI contract every
        // plugin must satisfy. `Library` outlives the borrowed `Symbol`s
        // because we hold the lock while we call them and never store the
        // pointers after the closure returns.
        let call_fn: libloading::Symbol<super::abi::PluginCallFn> = unsafe {
            lib.get(super::abi::SYM_CALL)
                .map_err(|e| format!("symbol ai_hands_plugin_call missing: {e}"))?
        };
        let free_fn: libloading::Symbol<super::abi::PluginFreeStringFn> = unsafe {
            lib.get(super::abi::SYM_FREE_STRING)
                .map_err(|e| format!("symbol ai_hands_plugin_free_string missing: {e}"))?
        };

        let tool_name_c =
            std::ffi::CString::new(tool_name).map_err(|e| format!("tool_name NUL: {e}"))?;
        let args_json_c =
            std::ffi::CString::new(args_json).map_err(|e| format!("args_json NUL: {e}"))?;
        let mut output_ptr: *mut std::ffi::c_char = std::ptr::null_mut();

        // SAFETY: plugin honors the ABI — non-null pointers in, optional
        // non-null pointer out via `output_json`. We always pair successful
        // output_ptr writes with a `free_fn` call below.
        let status = unsafe {
            call_fn(
                tool_name_c.as_ptr(),
                args_json_c.as_ptr(),
                &mut output_ptr as *mut _,
            )
        };

        let result = if !output_ptr.is_null() {
            // SAFETY: ABI says the plugin returned a NUL-terminated string
            // (or NULL). We copy out, then immediately hand the buffer
            // back via `free_fn` — the plugin owns its allocator.
            let s = unsafe {
                std::ffi::CStr::from_ptr(output_ptr)
                    .to_string_lossy()
                    .into_owned()
            };
            unsafe {
                free_fn(output_ptr);
            }
            serde_json::from_str::<serde_json::Value>(&s)
                .map_err(|e| format!("plugin returned non-JSON: {e}; raw: {s}"))?
        } else {
            serde_json::Value::Null
        };

        if status == 0 {
            Ok(result)
        } else {
            Err(format!(
                "plugin returned status code {status}; result: {result}"
            ))
        }
    })
}

/// Unload a plugin by name. Calls `ai_hands_plugin_shutdown` before
/// dropping the `Library` handle (which closes the DLL/SO and invalidates
/// all pointers the plugin previously handed us).
///
/// Idempotent: returns `true` if the plugin was registered, `false`
/// otherwise.
pub fn unload(plugin_name: &str) -> bool {
    let removed = remove(plugin_name);
    if removed {
        with_libraries(|libs| {
            if let Some(lib) = libs.remove(plugin_name) {
                // SAFETY: `shutdown` is part of the ABI; calling it once
                // before drop is what the contract specifies.
                if let Ok(shutdown_fn) =
                    unsafe { lib.get::<super::abi::PluginShutdownFn>(super::abi::SYM_SHUTDOWN) }
                {
                    unsafe {
                        shutdown_fn();
                    }
                }
                // `lib` drops here: closes the DLL/SO and reclaims its
                // address space (modulo OS-level deferred unload behavior).
                drop(lib);
            }
        });
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread;

    /// Tests share the global REGISTRY, so serialize them with a test-only
    /// mutex and clear state at the top of each test. Without this the
    /// concurrent_insert test would race the other tests.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn reset_registry() {
        with_registry(|m| m.clear());
    }

    fn sample_plugin(name: &str) -> LoadedPlugin {
        LoadedPlugin {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            author: "test".to_string(),
            description: "sample".to_string(),
            tools: vec![LoadedTool {
                name: format!("{}_tool", name),
                description: "echo".to_string(),
                input_schema_json: r#"{"type":"object"}"#.to_string(),
            }],
            abi_version_major: 1,
            abi_version_minor: 0,
            library_path: format!("/tmp/{}.so", name),
            loaded_at: "1970-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn list_returns_empty_initially() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        assert!(list().is_empty());
    }

    #[test]
    fn insert_then_list_returns_the_plugin() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        insert(sample_plugin("alpha")).expect("first insert");
        let plugins = list();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "alpha");
        assert_eq!(plugins[0].tools.len(), 1);
        assert_eq!(plugins[0].tools[0].name, "alpha_tool");
    }

    #[test]
    fn insert_duplicate_returns_error() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        insert(sample_plugin("beta")).expect("first insert");
        let err = insert(sample_plugin("beta")).expect_err("duplicate must error");
        assert!(
            err.contains("beta"),
            "error message should name the plugin: {err}"
        );
        assert_eq!(list().len(), 1, "registry must not double-register");
    }

    #[test]
    fn remove_returns_true_when_present_false_when_absent() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        insert(sample_plugin("gamma")).expect("insert");
        assert!(remove("gamma"));
        assert!(!remove("gamma"), "second remove must be false");
        assert!(!remove("never-existed"));
        assert!(list().is_empty());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        insert(sample_plugin("zeta")).unwrap();
        insert(sample_plugin("alpha")).unwrap();
        insert(sample_plugin("mu")).unwrap();
        let names: Vec<String> = list().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn concurrent_insert_does_not_crash() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();

        // Spawn 4 threads each inserting 5 plugins with disjoint names.
        // Disjoint names mean every insert succeeds — the test exercises
        // the Mutex coordination, not the dedup path.
        let collisions = Arc::new(StdMutex::new(0usize));
        let mut handles = Vec::new();
        for t in 0..4 {
            let coll = Arc::clone(&collisions);
            handles.push(thread::spawn(move || {
                for i in 0..5 {
                    let name = format!("plug_{}_{}", t, i);
                    if insert(sample_plugin(&name)).is_err() {
                        *coll.lock().unwrap() += 1;
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker panic");
        }

        assert_eq!(
            *collisions.lock().unwrap(),
            0,
            "no name collisions expected"
        );
        assert_eq!(list().len(), 20, "all 20 inserts should land");
    }

    fn reset_libraries() {
        with_libraries(|m| m.clear());
    }

    #[test]
    fn lookup_tool_owner_finds_tool_when_present() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        reset_libraries();
        // `sample_plugin("foo")` registers a tool named "foo_tool" — see
        // the helper above.
        insert(sample_plugin("foo")).expect("insert");
        insert(sample_plugin("bar")).expect("insert");

        assert_eq!(
            lookup_tool_owner("foo_tool").as_deref(),
            Some("foo"),
            "foo_tool belongs to plugin foo"
        );
        assert_eq!(
            lookup_tool_owner("bar_tool").as_deref(),
            Some("bar"),
            "bar_tool belongs to plugin bar"
        );
    }

    #[test]
    fn lookup_tool_owner_returns_none_when_absent() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        reset_libraries();
        insert(sample_plugin("only")).expect("insert");
        assert!(
            lookup_tool_owner("never_registered_tool").is_none(),
            "unknown tool name must return None"
        );
    }

    #[test]
    fn unload_removes_from_registry_and_library_map() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        reset_libraries();
        insert(sample_plugin("ephemeral")).expect("insert");
        // We can't easily synthesize a `libloading::Library` without an
        // actual DLL, so this test exercises the registry-side branch of
        // `unload`: it must return true (because the plugin is registered)
        // and `list()` must reflect the removal even though the library
        // map was empty for this entry.
        assert!(unload("ephemeral"), "first unload of a present plugin");
        assert!(
            !unload("ephemeral"),
            "second unload of the same name is a no-op"
        );
        assert!(
            list().iter().all(|p| p.name != "ephemeral"),
            "registry no longer references the plugin"
        );
        with_libraries(|libs| {
            assert!(
                !libs.contains_key("ephemeral"),
                "library map is also cleared"
            );
        });
    }

    #[test]
    fn unload_returns_false_for_unknown_plugin() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        reset_libraries();
        assert!(
            !unload("never_existed"),
            "unload of an absent plugin is false"
        );
    }

    /// `insert_with_library` rejects duplicate names. We can drive this
    /// path without a real DLL by seeding the registry via the plain
    /// `insert` helper and then attempting an `insert_with_library` for
    /// the same name — the duplicate check fires before we ever touch the
    /// `Library`, so the test stays portable.
    ///
    /// We can't construct a `libloading::Library` from thin air, so we
    /// rely on the fact that `insert_with_library` checks the registry
    /// map first. The test loads the example plugin (if it has been
    /// built) and runs the duplicate insert; if the example plugin DLL
    /// isn't present, we skip rather than fail so a fresh clone passes
    /// `cargo test` without manual build steps.
    #[test]
    fn insert_with_library_rejects_duplicate_name() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_registry();
        reset_libraries();

        let dll_path = example_plugin_dll_path();
        if !std::path::Path::new(&dll_path).exists() {
            eprintln!(
                "skipping insert_with_library_rejects_duplicate_name: example plugin not built at {dll_path}"
            );
            return;
        }

        // SAFETY: this test only runs when the example plugin DLL is on
        // disk; we trust it to be a sane cdylib we built ourselves.
        let lib1 = unsafe { libloading::Library::new(&dll_path) }.expect("first library load");
        let lib2 = unsafe { libloading::Library::new(&dll_path) }.expect("second library load");

        let p1 = LoadedPlugin {
            name: "dup_check".to_string(),
            ..sample_plugin("dup_check")
        };
        insert_with_library(p1, lib1).expect("first insert");

        let p2 = LoadedPlugin {
            name: "dup_check".to_string(),
            ..sample_plugin("dup_check")
        };
        let err = insert_with_library(p2, lib2).expect_err("duplicate must error");
        assert!(
            err.contains("dup_check"),
            "error must name the plugin: {err}"
        );

        // Cleanup so we don't pollute later tests.
        unload("dup_check");
    }

    fn example_plugin_dll_path() -> String {
        // The example plugin lives at
        // installers/plugin-abi/example/target/release/{example_plugin.dll, libexample_plugin.so, libexample_plugin.dylib}
        // We resolve relative to CARGO_MANIFEST_DIR so the test works no
        // matter where `cargo test` is invoked from.
        let manifest = env!("CARGO_MANIFEST_DIR");
        #[cfg(target_os = "windows")]
        let name = "example_plugin.dll";
        #[cfg(target_os = "macos")]
        let name = "libexample_plugin.dylib";
        #[cfg(all(unix, not(target_os = "macos")))]
        let name = "libexample_plugin.so";
        format!(
            "{manifest}/installers/plugin-abi/example/target/release/{name}",
            manifest = manifest,
            name = name
        )
    }
}
