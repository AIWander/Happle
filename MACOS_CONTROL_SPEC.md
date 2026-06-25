# Happle macOS Control Spec — AXUIElement · CGEvent · CGWindowList

**Status: SPEC ONLY — not implemented.** This is the design for the deferred macOS
native-control tier. Today Happle on macOS ships browser + vision (screenshot); the
control tier returns "deferred on macOS" at runtime via `uia_shim` (`src/main.rs`).
This doc is the blueprint for replacing those stubs with real macOS implementations
**when Mac hardware is available** (a Mac is required to compile *and* verify these —
they touch the live window server and need interactive permission grants).

---

## Where it slots in (already wired for it)

The Phase 1 port deliberately left clean seams:

| Stub today | Becomes |
|------------|---------|
| `uia_shim::handle_tool_call` `#[cfg(not(windows))]` arm (`src/main.rs`) | the macOS dispatcher → AXUIElement / CGEvent backends |
| `atomic.rs` `AtomicTool::call` `#[cfg(not(windows))]` arm | per-tool macOS implementations |
| `vision_screenshot_hidden_window` Windows-only (`windows::Storage::Xps::PrintWindow`) | `CGWindowListCreateImage` |
| `uia/mod.rs` cross-platform `Point`/`Rect` stubs | real AX geometry types |

No re-architecture is needed — swap the `#[cfg(not(windows))]` stub bodies for real code
and add the macOS dep block (already commented in `Cargo.toml`).

---

## Subsystem 1 — AXUIElement (the UIA equivalent)

Replaces the Windows UI Automation tree. macOS Accessibility exposes every app's UI as an
`AXUIElement` tree. Crates: `accessibility` + `accessibility-sys` (raw `AX*` FFI), `objc2-app-kit`
(app/window enumeration), `core-foundation` (CFType plumbing).

| Happle tool | macOS implementation |
|-------------|----------------------|
| `uia_get_state` (clickable element tree) | walk `kAXChildrenAttribute` from the focused app's `AXUIElementCreateApplication`, collect role/title/value/position/size |
| `uia_find` (by name / automation id) | recursive AX walk matching `kAXTitleAttribute` / `kAXIdentifierAttribute` / `kAXRoleAttribute` |
| `uia_click` | `AXUIElementPerformAction(el, kAXPressAction)`; fallback to a CGEvent click at the element's `kAXPositionAttribute`+`kAXSizeAttribute` center |
| `uia_type_text` | `AXUIElementSetAttributeValue(el, kAXValueAttribute, str)`; fallback to CGEvent key synthesis |
| `uia_read_value` | `AXUIElementCopyAttributeValue(el, kAXValueAttribute)` |
| `uia_list_windows` | `NSWorkspace.runningApplications` + per-app `kAXWindowsAttribute` |
| `uia_focus_window` | set `kAXMainAttribute`/`kAXFocusedAttribute`, or `NSRunningApplication.activate` |
| `uia_window_snap/move/resize/state` | set `kAXPositionAttribute` / `kAXSizeAttribute` |
| `uia_app_launch` | `NSWorkspace.openApplicationAtURL` (or `open -a`) |
| `uia_watch` / `uia_poll_events` | `AXObserverCreate` + `AXObserverAddNotification` (e.g. `kAXValueChangedNotification`, `kAXFocusedUIElementChangedNotification`) |

**Permission:** Accessibility — *System Settings → Privacy & Security → Accessibility → enable the host app.*
Every `AX*` call returns `kAXErrorAPIDisabled` until granted. The macOS dispatcher must detect
this and return a clear "grant Accessibility to <host>" message (don't fail silently).

---

## Subsystem 2 — CGEvent (keyboard + mouse synthesis)

Replaces `windows::UI::Input::KeyboardAndMouse` (`SendInput`). Crate: `core-graphics`.

| Happle op | macOS implementation |
|-----------|----------------------|
| mouse click at (x,y) | `CGEventCreateMouseEvent(kCGEventLeftMouseDown/Up)` + `CGEventPost(kCGHIDEventTap, …)` |
| `drag` / `element_drag` | mouse-down → `kCGEventLeftMouseDragged` steps → mouse-up |
| key press / type | `CGEventCreateKeyboardEvent(keycode, true/false)` + post; unicode via `CGEventKeyboardSetUnicodeString` |
| modifiers | set `CGEventFlags` on the event |

CGEvent posting also needs **Accessibility** permission (same grant as Subsystem 1).

---

## Subsystem 3 — CGWindowList (hidden-window + screen capture)

Replaces `PrintWindow` + `PW_RENDERFULLCONTENT` (the `vision_screenshot_hidden_window` tool).
Crate: `core-graphics`. Note: regular full-screen `vision_screenshot` already works today via
the cross-platform `screenshots` crate — this subsystem is specifically the *capture a named
window without foregrounding it* path.

| Happle op | macOS implementation |
|-----------|----------------------|
| enumerate on-screen windows | `CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID)` → window IDs + titles + owner PIDs |
| capture one window (not foregrounded) | `CGWindowListCreateImage(rect, kCGWindowListOptionIncludingWindow, windowID, kCGWindowImageBoundsIgnoreFraming)` → `CGImage` → PNG |

**Permission:** Screen Recording — *System Settings → Privacy & Security → Screen Recording.*
(macOS 14+ also gates window *titles* in `CGWindowListCopyWindowInfo` behind this grant.)

---

## Permissions summary

| Tier | macOS permission | Already needed today? |
|------|------------------|------------------------|
| browser | none | shipping |
| `vision_screenshot` (full screen) | Screen Recording | shipping |
| Subsystem 1 (AX control) | Accessibility | NEW |
| Subsystem 2 (CGEvent input) | Accessibility | NEW |
| Subsystem 3 (hidden-window capture) | Screen Recording | NEW (extends existing) |

Add a `happle doctor` / preflight that reports which grants are present and links the exact
Settings panes — the #1 macOS-automation support issue is ungranted permissions failing silently.

---

## Effort estimate (calibrated to the 2026-06-25 port pace)

Today, with no Mac, the whole Windows→macOS *compile* port (5-repo dependency chain + UIA tier
gating) took one focused session via the CI loop. The *control* tier is different work — it needs
a Mac to compile (AX/CG frameworks) and to verify (live windows + interactive permission grants),
and it's net-new implementation, not gating. Realistic estimate **on a Mac**:

| Phase | Scope | Estimate |
|-------|-------|----------|
| **2a** | macOS dep block + `cargo build` green on a Mac; AX permission preflight; `uia_get_state` + `uia_find` (the AX tree walk — the load-bearing primitive) | **2–3 days** |
| **2b** | click / type / read_value / window focus-move-resize / app launch | **2–3 days** |
| **2c** | CGEvent mouse + keyboard synthesis + drag tools | **1–2 days** |
| **2d** | CGWindowList hidden-window capture | **1 day** |
| **2e** | AXObserver watch/poll-events; `happle doctor` permission report; smoke-test every tool on a real Mac | **2–3 days** |
| **Total** | parity with the Windows native tier | **~1.5–2 weeks** of focused Mac work |

This compresses if a contributor already knows `objc2`/Core Graphics. The browser + vision tiers
need no further work — they're done.

---

## How Happle functions: now vs. with this tier

**Now (this alpha, no control tier):**
- Drives the **browser** fully (navigate, click, fill, extract, network logs) — Chrome via CDP.
- Takes **screenshots** of the Mac screen (the troubleshooting "eyes"), full-screen via the
  `screenshots` crate. OCR with `--features onnx`.
- Cannot touch **native macOS apps** (Finder, Mail, Safari chrome, System Settings, any non-browser
  app) — `uia_*` tools return "deferred on macOS". Cannot synthesize OS-level clicks/keystrokes
  outside the browser. Cannot capture a specific background window without foregrounding it.
- **Useful for:** anything web-based + visual verification. An agent can research, fill web forms,
  scrape, and *see* the screen — it just can't drive native apps.

**With this tier added:**
- Full parity with the Windows build on Mac: drive **any** macOS app via the Accessibility tree,
  synthesize real mouse/keyboard input, capture any window. The agent goes from "web + eyes" to
  "complete desktop control" — click buttons in native apps, type into any field, manage windows,
  launch apps, watch for UI events.
- Same capability surface that makes the Windows AI-Hands a category leader, now on macOS.
