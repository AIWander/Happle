//! Plugin system — heavyweight C-FFI loader path.
//!
//! Phase 2 (this commit): real dynamic-library loading via `libloading`,
//! symbol resolution, plugin-init invocation, registry insertion, and
//! tool dispatch through `ai_hands_plugin_call`. The `hands_plugin_list`,
//! `hands_plugin_load`, `hands_plugin_call`, and `hands_plugin_unload`
//! MCP tools expose the full lifecycle.
//!
//! Plugin authors should target the C ABI in
//! `installers/plugin-abi/ai_hands_plugin.h`. See that header for the
//! contract (5 entry points, thread-safety, version-skew policy). A
//! minimal reference implementation lives at
//! `installers/plugin-abi/example/`.

pub mod abi;
pub mod loader;
pub mod registry;
