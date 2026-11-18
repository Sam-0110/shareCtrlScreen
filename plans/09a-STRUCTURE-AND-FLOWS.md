# 09a — Detachable tabs: structure, contracts and flows

Companion to [09](09-DETACHABLE-TABS.md), rewritten after its design review (09 §7) and the
owner's answers (09 §5, §7). Builds on plan 08's structure ([08a](08a-STRUCTURE-AND-FLOWS.md));
only what changes is described. Tags in flows: [UI] web page, [TM] TabManager, [WM]
WindowManager, [SF] SurfaceFactory, [E] engine, [UI-T] the UI thread.

## 1. Ownership

| Owner | Owns | Does not own |
|---|---|---|
| **Engine** | Each session's **current surface** (one swappable cell); which sessions are on screen and which one is focused, applied atomically; pausing off-screen sessions; the clipboard gate and shortcut hook of the focused one; per-window web-dialog state; handing a session to another surface | Windows, tabs, placement |
| **TabManager** (`app/src/tabs.rs`) | The strip (docked tabs), the active tab, the **detached set** (seeded at launch / sign-in only), each tab's window, the on-screen / focused decision | OS window events |
| **WindowManager** (new `app/src/windows.rs`) | Detached windows: create / show / hide / destroy on the UI thread; their activation (from the OS, §3.3), minimise, move, resize, close; reading and saving their placement | Sessions, the strip |
| **SurfaceFactory** (`app/src/surfaces.rs`) | Surfaces in a given window with a given inset; destroyed only after `SurfaceReleased` / `Released` | — |
| **Platform** | Window activation and minimised state per OS (§3.3); presenters on a given device | — |

## 2. Code layout

```
crates/protocol/src/sync/tabs.rs        SyncTab + placement fields (§3.1), merge
crates/protocol/src/sync/placement.rs   pure: proportions ↔ rectangles, monitor ordering, clamping
server/src/sync.js                      the same merge + validation; feature "placement"
testdata/sync-vectors.json              placement merge cases (Rust and JS)
crates/platform-api/src/surface.rs      Presenter on a given device; window activation hook (§3.3)
crates/platform-{windows,macos,linux}   activation + minimised detection; create_presenter_on
crates/engine/src/session/current.rs    the session's current surface cell (new)
crates/engine/src/session/active.rs     view: swappable surface, window id, per-window overlay
crates/engine/src/session/registry.rs   on_screen: BTreeSet<SessionId>, focused: Option<SessionId>
crates/engine/src/media/viewer.rs       Handover: new presenter on the decoder's device, held frame re-presented
crates/engine/src/sessions.rs           set_screen(), move_session(), set_overlay(window, open)
app/src/windows.rs                      WindowManager
app/src/tabs.rs                         detached set, Detach / Dock / Close, the screen decision
app/capabilities/*.json                 detached windows' capability (labels detached-*)
app/ui/window.html, window.js           a detached window's page
```

## 3. Types and contracts

### 3.1 Sync schema

```rust
pub struct SyncTab {                       // as today, plus:
    pub order: String, pub order_at: u64, pub opened_at: u64, pub closed_at: u64,
    pub detached: bool,      pub detached_at: u64,
    pub state: WinState,     pub state_at: u64,      // Normal | Maximized | Minimized
    pub size: Option<Frac2>, pub size_at: u64,       // w, h
    pub pos: Option<Pos>,    pub pos_at: u64,        // never written by a computer that cannot know it (Wayland)
}
pub struct Frac2 { pub a: u16, pub b: u16 }          // ten-thousandths (0..=10000) of the monitor's work area
pub struct Pos { pub monitor: u8, pub x: u16, pub y: u16 }   // monitor: 0 = primary, then left→right, top→bottom
```

Each pair merges last-writer-wins on its own stamp; equal stamps break ties by the serialized
value's code-point order (Rust and JS alike). Every stamp is clamped like the others (08a §7.3),
counts in `last_touched` and in the server's prune. The server validates ranges and drops what is
invalid; Rust parses leniently (out of range → `None`, unknown state → `Normal`). Sent only to a
server advertising feature `placement`; old clients ignore the fields (serde default).

A reopened tab (closed → open) clears `detached` and forgets `size` / `pos` (09 §7.9).

### 3.2 Engine

```rust
pub struct WindowId(pub u32);                     // 0 = main; the app numbers detached windows
impl Engine {
    /// Replaces `set_active`. Atomic under `reg`: each session in `on_screen` is shown on its
    /// current surface once ready there (not under its window's dialog, not in link trouble);
    /// every other viewer session is hidden and paused (`vpause` when supported); `focused`
    /// (∈ on_screen, or the last focused if `None` was not asked for) alone gets the clipboard
    /// and the shortcut hook. Never blocks.
    pub fn set_screen(&self, on_screen: &[SessionId], focused: Option<SessionId>);
    /// A web dialog opened / the last one closed in `window`: hides / shows that window's video.
    pub async fn set_overlay(&self, window: WindowId, open: bool);
    /// Hand session `id` (any phase before Ended) to `surface` in `window`. One move at a time
    /// per session (`Busy` otherwise, also while it is starting). The media thread builds the
    /// new presenter on the decoder's device, re-presents the held frame and swaps; the view,
    /// sinks, cursor control, cursor sprite and shortcut-hook window follow (hide old before
    /// show new). `SessionEvent::SurfaceReleased { surface }` follows for the old one.
    pub fn move_session(&self, id: SessionId, surface: Arc<dyn VideoSurface>, window: WindowId)
        -> Result<(), MoveError>;     // NotFound, Ended, Busy, SurfaceInUse
}
pub enum SessionEvent { …, SurfaceReleased { surface: SurfaceTag } }  // SurfaceTag identifies it to the app
pub enum UiEvent { …, TabShortcut { step: TabStep, session: SessionId } }   // from which session's video
```

Every holder of a session's surface today (09 §7 blocker 3) reads the one **current-surface
cell**; `end()` and the reaper detach and release **every** surface the session held.

### 3.3 Platform: activation and minimised

```rust
/// Window activation and minimise, from the OS (Tauri's `Focused` is synthesized from WebView2
/// focus on Windows and misses the native video child, 09 §7 blocker 1).
pub trait WindowWatch: Send + Sync {
    /// Calls `f(active, minimised)` on every change for the top-level window behind `handle`.
    fn watch(&self, handle: RawWindowHandle, f: Box<dyn Fn(bool, bool) + Send + Sync>)
        -> Result<WatchToken, SurfaceError>;   // dropping the token stops it
}
```

Windows: subclass `WM_ACTIVATE` / `WM_SIZE (SIZE_MINIMIZED)` on the top-level. macOS:
`NSWindowDidBecomeKey` / `DidResignKey` / `DidMiniaturize` / `DidDeminiaturize` notifications.
Linux X11: GTK `focus-in` / `focus-out` and `window-state-event` (ICONIFIED). Wayland: focus via
GTK; minimised never reported (09 §7.6).

### 3.4 Commands and events

Commands (UI → app; all take the tab's `remoteId`, never "the active session"):
`tab_detach`, `tab_dock`, `window_close` (= Disconnect + close tab), and the session-bar
commands `end_session`, `request_refresh`, `select_display`, `popout_display`,
`set_video_visible` (+ the calling window). Events: `tabs-changed` (docked tabs only, plus a
`detached` list), `tab-state {remoteId, tab}` for a detached window's own tab, `tab-displays` /
`tab-toast` by remote.

## 4. State machines

### 4.1 Tab (app)
```
               tab_detach (any phase)                     minimised (not on Wayland)
  Docked ─────────────────────────────────▶ Detached ──────────────────────────▶ Detached·Min
    ▲                                         │   ▲ restored ◀──────────────────────────┘
    └──────────── tab_dock ───────────────────┘
  window_close / tab_close → Disconnect, tab closed (synced)
  app exit / tray / sign-out / switch → windows closed or hidden, detached kept (not persisted false)
```
On screen = (Docked ∧ active ∧ main window not minimised) ∨ (Detached ∧ not minimised).
Focused = the session of the window the OS reports active; while none of ours is active, the last
one (sticky, for the clipboard). A pop-out window's activation counts as its session's.

### 4.2 Session surface (engine)
```
 Bound(s1) ──move_session(s2)──▶ Moving(s1→s2) ──presenter on s2 (decoder's device), held frame shown──▶ Bound(s2)
     │                               │ failure: Bound(s1), MoveError (the app rolls back)
     │                               └─▶ SurfaceReleased(s1)
     └── end() at any time: every surface detached, SurfaceReleased / Released for all
```

## 5. Flows

### 5.1 Detach `r`
1. [TM] r open and docked. 2. [WM]→[UI-T] create window `detached-<n>` (saved placement if any,
else a default size on the main window's monitor), its capability, its activation watch; [SF]
its surface (no inset). 3. [TM] if r has a session: `move_session(s, surface, w)`; on error
destroy the window, keep r docked (nothing else changed yet). 4. [TM] r leaves the strip; if r was
the active tab, the tab **before** it (else after) becomes active. 5. [TM] `set_screen`. 6. [TM]
persist `detached = true` (stamped). 7. On `SurfaceReleased` [SF] destroys the old surface.

### 5.2 Dock `r`
1. [WM] unminimise and raise the main window. 2. [SF] a surface in main (strip inset).
3. [TM] if live: `move_session(s, surface, main)`; on error keep the window. 4. [TM] r gets an
order key at the **end** of the strip and becomes the active tab; `set_screen` (the previous
active tab pauses). 5. persist `detached = false`. 6. [WM] destroys the window after
`SurfaceReleased`.

### 5.3 Activation, minimise, restore
[Platform watch] → [WM] → [TM] recompute on-screen and focused → one `set_screen` with the full
answer, under the TabManager's lock. Main window minimised → its active tab off screen. A
focused window that becomes active again gets its video the keyboard (`focus_owed` follows
focus).

### 5.4 Placement saved
Move / resize / state change in a detached window → 500 ms debounce → in Normal state only:
outer rectangle + its monitor's work area (logical points on macOS) → `placement.rs` → stamped
`size` / `pos` / `state`; a computer that cannot know positions never writes `pos`.

### 5.5 Launch / sign-in
[TM] seed the detached set from the workspace (only here). For each detached open tab [WM]
creates its window: `pos.monitor` if it exists, else the primary (else the first); the
rectangle scaled to that work area and clamped onto it (minimum size); position set before size
(Windows DPI); then maximised / minimised as saved. Disconnected; nothing connects. Tabs closed
on another computer meanwhile are not open, so no window (09 §7.8).

### 5.6 Close (X) on a detached window
[WM] `CloseRequested` → prevent → [TM] `window_close(r)`: Disconnect + close the tab (synced);
the window and its surface go after `Released`.

### 5.7 Main window close, tray, exit, sign-out, account switch
Sessions end (D6, as today). Close / exit: every detached window closes, then the app exits.
Tray (Windows service host): detached windows hide with the main window and are shown again
(disconnected) with it. Sign-out / switch: windows close; the next workspace's detached tabs
open as in §5.5. None of these persists `detached = false`.

### 5.8 Settings and menus in a detached window
Its Settings opens over its own video only (`set_overlay(w, …)`). The macOS / Linux menu bar,
the Monitor menu and pop-out offers act on the focused session. Tab chords from a detached
window's session are ignored (09 Decision 2).

## 6. Invariants

| # | Invariant |
|---|---|
| J1 | A session has one current surface; an old surface is destroyed only after `SurfaceReleased` / `Released` |
| J2 | At most one focused session; only it has the clipboard gate and the shortcut hook |
| J3 | Every viewer session not on screen is paused (`vpause` where supported) |
| J4 | A web dialog hides only the video of its own window |
| J5 | Detach / Dock never change the `SessionId` or the host session |
| J6 | Placement and the detached set from a sync apply only at launch / sign-in |
| J7 | Nothing connects by itself |
| J8 | The main window's placement is never synced |
| J9 | A computer that cannot know window positions never writes `pos` |
| J10 | Detach, Dock and Close never leave a tab both docked and detached, or in neither |
