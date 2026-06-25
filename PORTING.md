# Happle Porting Guide — AI-Hands (Windows) → macOS

Happle starts as a clone of **AI-Hands v1.0.1**. This document maps every Windows-specific
surface to its macOS equivalent and tracks the work to make Happle run natively on
macOS / Apple Silicon.

**Status:** scaffold. Code is the verbatim Windows codebase. Nothing macOS-specific is wired yet.

**You need a Mac to do and verify this work.** A Windows machine cannot compile or test the
macOS paths. Options: develop on a Mac, delegate to a contributor with a Mac, or use GitHub
Actions `macos-14` runners (Apple Silicon, free for public repos) for CI-only verification.

---

## What already carries over (cross-platform — no work)

| Surface | Why it's portable |
|---------|-------------------|
| **Browser tier** (`browser-mcp`, chromiumoxide CDP) | chromiumoxide speaks the Chrome DevTools Protocol over a socket — OS-agnostic. Chrome/Chromium on macOS exposes the same `--remote-debugging-port`. |
| **Vision tier** (`vision-core`, Tesseract OCR, template match, image diff) | Tesseract + the `image` crate are cross-platform. OCR/diff/template logic is pure compute. |
| **MCP protocol layer** (tokio, serde, stdio transport) | Pure Rust, no OS deps. |
| **Meta-tool routers** (`hands_click` 7-rung ladder, etc.) | The *logic* is portable; only the UIA and Win32 *leaf calls* need swapping (see below). |
| **Most of `main.rs` dispatch** | Tool registration + routing is OS-agnostic. |

So roughly **two of the three tiers are free.** The native-UI tier is the real port.

---

## The Windows-specific surfaces to replace

### 1. Native UI automation — the big one

| Windows (AI-Hands) | macOS (Happle) | Crate |
|--------------------|----------------|-------|
| `uia-mcp` (UI Automation tree, find/click/type/read-value, window mgmt) | macOS **Accessibility API** — `AXUIElement` tree | [`accessibility`](https://crates.io/crates/accessibility) + [`accessibility-sys`](https://crates.io/crates/accessibility-sys) |
| `UIA get_state` (clickable element tree) | `AXUIElementCopyAttributeValue(kAXChildrenAttribute, …)` walk | `accessibility` |
| `UIA find_element` by name/automation-id | `AXUIElement` traversal matching `kAXTitleAttribute` / `kAXIdentifierAttribute` | `accessibility` |
| `UIA click` (invoke pattern / coords) | `AXUIElementPerformAction(kAXPressAction)` OR synthesized `CGEvent` mouse click | `accessibility` / `core-graphics` |
| `UIA type_text` | `AXUIElementSetAttributeValue(kAXValueAttribute, …)` OR synthesized `CGEvent` key events | `accessibility` / `core-graphics` |
| `UIA read_value` | `AXUIElementCopyAttributeValue(kAXValueAttribute, …)` | `accessibility` |
| `UIA window snap/move/resize` | `kAXPositionAttribute` / `kAXSizeAttribute` set, or `NSWindow` via `NSRunningApplication` | `accessibility` / `objc2-app-kit` |
| `UIA app_launch` | `NSWorkspace.launchApplication` / `open -a` | `objc2-app-kit` |
| `UIA watch / poll_events` | `AXObserverCreate` + `AXObserverAddNotification` (e.g. `kAXValueChangedNotification`) | `accessibility-sys` |

**Permission note:** macOS gates the Accessibility API behind **System Settings → Privacy & Security →
Accessibility**. The host app (Terminal, the MCP host, or Happle itself) must be granted access, or
every `AX*` call returns `kAXErrorAPIDisabled`. Document this prominently in the Happle README's
install section — it's the #1 "why doesn't it work" for macOS automation.

### 2. Keyboard / mouse synthesis

| Windows | macOS | Crate |
|---------|-------|-------|
| `Win32_UI_Input_KeyboardAndMouse` (`SendInput`, mouse_event) | `CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent` + `CGEventPost` | [`core-graphics`](https://crates.io/crates/core-graphics) |
| `drag` / `element_drag` combo tools | `CGEvent` mouse-down → mouse-moved → mouse-up sequence | `core-graphics` |

### 3. Window pixel capture (the `behind=true` / hidden-window path)

| Windows | macOS | Crate |
|---------|-------|-------|
| `PrintWindow` + `PW_RENDERFULLCONTENT` (`Win32_Storage_Xps` + `Win32_Graphics_Gdi`) — capture a window without foregrounding it | `CGWindowListCreateImage` with `kCGWindowListOptionIncludingWindow` + `kCGWindowImageBoundsIgnoreFraming` | [`core-graphics`](https://crates.io/crates/core-graphics) |
| `vision_screenshot_hidden_window` tool | Same tool, CGWindowList backend | `core-graphics` |

**Permission note:** macOS 10.15+ gates screen capture behind **Screen Recording** permission
(separate from Accessibility). Both must be granted.

### 4. App / window enumeration

| Windows | macOS | Crate |
|---------|-------|-------|
| `EnumWindows` / `GetWindowTextW` | `CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, …)` + `NSWorkspace.runningApplications` | `core-graphics` / `objc2-app-kit` |
| `uia_focus_window(title)` | `NSRunningApplication.activate` or AX `kAXMainAttribute` set | `objc2-app-kit` / `accessibility` |

### 5. COM — drop entirely

`Win32_System_Com` (COM init for UIA) has **no macOS equivalent and is not needed** — the
Accessibility API has no COM apartment model. Remove all `CoInitialize`/`CoUninitialize` calls
under `cfg(target_os = "macos")`.

### 6. Paths & install

| Windows | macOS |
|---------|-------|
| `cpc-paths` resolves `%LOCALAPPDATA%`, Volumes drive paths | macOS: `~/Library/Application Support/`, `~/Library/Caches/`. Either add macOS arms to `cpc-paths` or fork a `happle-paths`. |
| Claude Desktop config: `%APPDATA%/Claude/claude_desktop_config.json` | macOS: `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Binary: `happle` (no `.exe`) | same |
| Install: winget/Scoop | macOS: Homebrew tap (`brew install aiwander/tap/happle`) or notarized `.pkg` |

### 7. CI

| Windows | macOS |
|---------|-------|
| `windows-latest` runner, x64 + ARM64 cross-compile | `macos-14` runner (Apple Silicon, arm64 native) + `macos-13` (Intel x86_64) if dual-arch wanted |
| Update `.github/workflows/ci.yml` to target `aarch64-apple-darwin` (+ `x86_64-apple-darwin`) | universal2 binary via `lipo` if shipping one download |

---

## Recommended source structure for the port

Gate the platform leaves, keep the routers shared:

```
src/
  main.rs                 # OS-agnostic dispatch (unchanged)
  meta/                   # smart routers — logic unchanged, leaf calls cfg-gated
  native/
    mod.rs                # trait NativeUi { get_state, find, click, type, ... }
    windows.rs            # #[cfg(windows)]  — existing UIA impl (move uia-mcp calls here)
    macos.rs              # #[cfg(target_os = "macos")] — AXUIElement impl  ← THE WORK
  capture/
    windows.rs            # #[cfg(windows)] PrintWindow
    macos.rs              # #[cfg(target_os = "macos")] CGWindowListCreateImage
```

Define a `NativeUi` trait, implement it twice behind cfg gates, and the meta-tools call the trait —
not the platform API directly. That makes the eventual single cross-platform binary trivial and keeps
both builds green during the transition.

---

## Phased checklist

### Phase 0 — scaffold (DONE)
- [x] Repo `AIWander/Happle` created
- [x] AI-Hands v1.0.1 cloned in
- [x] Cargo.toml rebranded (`happle`, macOS keywords, macOS dep block placeholder)
- [x] README reframed as the Apple port
- [x] PORTING.md (this file)

### Phase 1 — make it build on macOS (no functionality yet)
- [ ] Move all `uia-mcp` / `windows`-crate calls behind `#[cfg(windows)]`
- [ ] Add `native::macos` + `capture::macos` modules that **compile to stubs** (return "not implemented on macOS yet")
- [ ] `cargo build --target aarch64-apple-darwin` succeeds (browser + vision tiers live, native tier stubbed)
- [ ] CI `macos-14` job green on the stub build

### Phase 2 — native UI tier
- [ ] `AXUIElement` tree walk → `get_state` equivalent
- [ ] find / click / type / read_value via AX
- [ ] window enumerate / focus / move / resize
- [ ] app launch via NSWorkspace
- [ ] AX permission-check + clear error when not granted

### Phase 3 — capture + input
- [ ] `CGWindowListCreateImage` for screenshot + hidden-window capture
- [ ] `CGEvent` keyboard + mouse synthesis (drag tools)
- [ ] Screen Recording permission-check + clear error

### Phase 4 — paths, install, polish
- [ ] macOS path resolution (cpc-paths macOS arm or happle-paths)
- [ ] Claude Desktop config registration at `~/Library/Application Support/Claude/`
- [ ] Homebrew tap or notarized `.pkg` installer
- [ ] README install section with the two permission grants spelled out
- [ ] Tag `v0.1.0` once Phase 2+3 pass on a real Mac

---

## Key macOS crates summary

```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2            = "0.5"   # Objective-C runtime
objc2-app-kit    = "0.2"   # NSWorkspace, NSRunningApplication, window mgmt
objc2-foundation = "0.2"   # NSString / NSArray / NSDictionary bridging
accessibility    = "0.1"   # AXUIElement high-level (UIA equivalent)
accessibility-sys= "0.1"   # raw AX* FFI for observers / edge cases
core-graphics    = "0.23"  # CGWindowListCreateImage, CGEvent input synthesis
core-foundation  = "0.9"   # CFType plumbing under the above
```

(Versions are starting points — check crates.io for current at port time.)
