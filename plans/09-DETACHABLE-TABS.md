# 09 — Detachable tabs: a tab becomes its own window, and back

Written 2026-11-18 against `main` @ `0c21619` (v2.2.0, multi-session tabs, plan 08). Read-only
research of the engine's ActiveGate, the TabManager, the surfaces and pop-out windows, the sync
schema and the Tauri 2.12.5 / tao 0.37 window APIs; every claim cites `file:line` (paths relative
to `native/` unless they start with `server/` or `plans/`). Structure, contracts and flows are in
[09a](09a-STRUCTURE-AND-FLOWS.md). Nothing here is built yet.

## Progress

| Step | What | Status |
|---|---|---|
| S | This plan + structure ([09a](09a-STRUCTURE-AND-FLOWS.md)), adversarial review folded in | **complete**: review folded in (§7), owner's answers recorded (§5, §7), 09a rewritten from them |
| W1 | Engine: several sessions on screen, one with the keyboard | **complete** (this commit): `set_screen(on_screen, focused)` replaces `set_active`; overlays per window (`set_overlay(window, open)`); the shortcut claim needs the window uncovered; loopback `several_sessions_on_screen_one_focused`; review fixes (the hook needs focused + on screen + uncovered, decided under one lock; overlays keyed by `WindowId`). The app still puts one tab on screen (W3 adds windows) |
| W2 | Engine: hand a live session to another surface | **complete** (this commit): `move_session(id, surface, window)` + `SessionEvent::SurfaceReleased`; one current-surface cell per session (`session/current.rs`) read by the slot, driver, shortcut hook and reaper; the media thread keeps its decoder and builds the presenter on its device (`create_presenter_on`, shared D3D11 device on Windows); `connect_in(…, window)`; `TabShortcut` carries the session. Loopback: 20 live moves, a failed move, a move while connecting. Review fixes (3761a13): a media thread that failed before the bind no longer strands moves (sticky media state, view gate bound before media starts), the old D3D11 swap chain is released before `SurfaceReleased` (and its waitable handle closed), a failed move during the end reports nothing twice; more tests. Device-pending: the move on real GPUs (W5 checklist) |
| W3 | App: detached windows (Detach / Dock / close, focus, minimise), saved on this computer | **complete** (b39201b, 7101b02; ba72da4, 4005ec3; 1e41032; review fixes in the next commit): synced schema + placement maths + workspace ops; the OS window watch on every OS; the app — WindowManager (`app/src/windows.rs`), TabManager detach / dock / close / screen, the window page (`index.html?window=`), placement saved in the normal state. Review fixes: no window closes while an ended-but-unreleased session draws in it (J1), a closed window's activation is forgotten, D6 counts detached tabs, a failed Detach goes back unreordered, local Detach / Dock do not depend on the account's flag, main-only dialogs stay in the main window. Device-pending: `checklists/DETACH-W3.md`; macOS code compiled by CI only |
| W4 | Sync: which tabs are detached and where, through the account | **complete** (d9534d5; review fixes in the next commit): placement is sent only to a server listing `placement`; a detached tab synced from another computer opens its window at sign-in once the first sync settles (W4 review); a newly advertised `placement` triggers an immediate second round. Server ships with the merge to `main` before any client tag. Device-pending: step 10 of `checklists/DETACH-W3.md` |
| W5 | Per-OS finish (Wayland placement, macOS minimise/focus, Windows DPI) + device checklists | **complete in code** (next commit), device-pending: Wayland saves size and state only, never position (documented in the README); macOS key / miniaturize notifications (W3 2/3); Windows position before size across DPI, `ScaleFactorChanged` re-saves; windows restored at login open minimised and the placing's own moves are not re-saved. Verification: `checklists/DETACH-W3.md` on every OS |
| Final | Whole-feature review, merge, release | not started |

## 1. What the owner asked for (2026-11-18)

- Sign-in starts with one tab or none; **+** adds a tab; tabs can be renamed (as today).
- In the strip only the **focused** tab is active: it streams and takes control; every other tab
  is inactive and paused (as today, plan 08 P3/P6).
- Every tab has a button that turns it into its **own normal window** (move, resize, maximise,
  minimise) without losing the connection to the machine. The window has the usual session
  buttons and a button that puts it back.
- All split-out windows **stream at the same time**, never paused. The window you focus gets the
  keyboard (and with it the clipboard and shortcut capture): your actions control that machine.
- A **minimised** window is inactive, like a background tab: paused until it is restored.
- Splitting out the **active** tab makes the tab before it in the strip the active tab, which
  streams. Splitting out an **inactive** connected tab makes it a focused window that resumes
  streaming; the strip's active tab stays.
- Putting a window back always docks it at the **end** of the strip as the **focused** tab,
  without losing the connection; the previously focused tab becomes inactive and pauses.
- The window's **close (X)** ends that session, and the window and its tab are gone.
- **Saved and synced through the account, whenever it changes:** which tabs are split out, and
  each window's position, size and maximised / minimised state, as **proportions of the screen,
  not pixels**. At the next sign-in — on this or another computer — it all comes back. Another
  computer picks changes up **at its next sign-in**, not live. **Nothing reconnects by itself.**
- The **main window's** own position and size are **not** synced.

Names used below: **Detach** (tab → window) and **Dock** (window → tab). They never end the
session; **Disconnect** keeps meaning "end the session".

## 2. Where we are today (facts the design must respect)

1. **One session on screen, engine-wide.** `Registry::active: Option<SessionId>`
   (`crates/engine/src/session/registry.rs:178`); `Engine::set_active`
   (`crates/engine/src/sessions.rs:785`) switches it under `reg`. Everything follows that one
   session: its view is shown (`session/active.rs`), its clipboard gate is open and it owns the
   shortcut hook (`session/mod.rs:259` `Activity`, `:896` `shortcut_loop`), and every other
   session's host is paused (`TabGate::set_active`, `session/mod.rs:194`). Detached windows need
   "several shown and streaming" separated from "one has the keyboard".
2. **The web-dialog flag is engine-wide.** `Engine::set_overlay` (`sessions.rs:818`) hides every
   video. With several windows, a dialog in one window must not hide the video in another.
3. **A session is bound to one surface for life.** The viewer media thread creates its presenter
   from the surface once (`media/viewer.rs:51`) and its decoder on the presenter's device
   (`:54`); the surface also carries the input and drop sinks, cursor control and the view gate
   (`session/active.rs`). On Windows the shortcut hook's focus gate is the surface's top-level
   window (`platform-windows/src/adapters.rs:773`). Moving a session to another window means
   handing all of this to a new surface — there is no such operation today.
4. **What already exists for windows.** Pop-out windows (D14) are `WebviewWindow`s whose native
   surface is made on the UI thread by `surfaces::create_in` (`app/src/surfaces.rs:29`) and
   attached to a running session (`app/src/popout.rs:270`); their lifecycle rules (close after
   the engine let go, labels unique per window) are a proven template.
5. **Tabs.** The TabManager (`app/src/tabs.rs`) owns the strip: `visible` / `shown_tab`
   (`:767`, `:782`), `activate` (`:365`), `step` (`:397`), `apply_active` (`:645`). A tab's
   record is `SyncTab {order, order_at, opened_at, closed_at}` (`crates/protocol/src/sync/tabs.rs:26`),
   merged identically in Rust and in `server/src/sync.js:82` (shared vectors,
   `testdata/sync-vectors.json`).
6. **Window APIs (Tauri 2.12.5).** `Window::{outer_position, inner_size, is_minimized,
   is_maximized, current_monitor, available_monitors, primary_monitor, set_position, set_size}`
   and `Monitor::{name, position, size, work_area, scale_factor}`
   (`tauri-2.12.5/src/window/mod.rs:81-101`, `:1580-1718`, `:1963`); events `Moved`, `Resized`,
   `Focused`, `ScaleFactorChanged`, `CloseRequested` (`tauri-runtime/src/window.rs:32-53`).
7. **Wayland has no window positions.** xdg-shell gives a client neither its global position nor
   a way to set one; tao's Linux backend reports what GTK stores (`tao-0.37.2/src/platform_impl/
   linux/window.rs:464`), which on Wayland is not a screen position. Size and maximised /
   minimised state work everywhere; **position restore is impossible on Wayland** (the compositor
   places the window).

## 3. Target design

### 3.1 On screen, focused, paused

- A session is **on screen** when it is the strip's active tab (and the main window is not
  minimised), or it has a detached window that is not minimised.
- **One** on-screen session is **focused**: the one whose window has the OS keyboard focus (the
  main window → its active tab). It alone gets the keyboard, the clipboard and the shortcut hook.
  Mouse input already goes to whichever surface it is over (per-surface sinks).
- Every session **not** on screen is paused (`vpause`, plan 08 P6); every on-screen one streams.
- A web dialog hides the video **of its own window** only.

The engine's single `active` becomes `on_screen: set` + `focused: Option` (09a §3). The decision
rules stay in the app (which window is focused, which tab is active, what is minimised); the
engine applies them atomically in one call, like `set_active` today.

### 3.2 Detach and Dock keep the session: surface handover

Detach makes a window with a new surface and hands the running session to it; Dock does the
reverse with a new surface in the main window. The connection, the host and the session's
threads stay; what moves is the presenter, the decoder (it lives on the presenter's GPU device,
§2.3), the input and drop sinks, cursor control, the view gate and the shortcut hook's window.
The engine gains **`Engine::move_session(id, surface)`**: the media thread builds a presenter and
decoder on the new surface, asks the host for a keyframe (the new decoder has no references),
swaps, and the old surface is released for destruction — the same "destroy after the engine let
go" rule pop-outs follow. Expected cost: a blank of about one keyframe round trip (≈ 100–300 ms).

Rejected: re-parenting the native view into the new window (`SetParent`, `addSubview`,
`XReparentWindow`). It would skip the presenter rebuild on three backends, but Wayland
sub-surfaces cannot change parent toplevel, DXGI swap chains and Metal layers across top-levels
with different DPI need their own handling, and it would be a second code path. One handover
path for every OS.

### 3.3 Windows, not pop-outs

A detached window is a `WebviewWindow` loading a **session page** (`ui/window.html`): the session
bar (with **Dock** added), the tab card (connecting / password / disconnected with Connect), the
link-trouble panel — the tab body without the strip, the connect page or the host screen. Its
native surface has **no top inset** (no strip) and keeps the hover-reveal session bar. Pop-outs
(D14) stay as they are and belong to the session, wherever it is shown.

### 3.4 Saved and synced placement

A detached tab carries a **placement**: detached or not, the window state (normal / maximised /
minimised), and its normal-state rectangle **as proportions of a monitor's work area**:
`{monitor, x, y, w, h}` with `x, y, w, h` in **ten-thousandths** of the work area (integers, so
the Rust and JS merges cannot disagree, plan 08a §7.13) and `monitor` = `0` for the primary,
else the monitor's place left-to-right, top-to-bottom among the others. Restoring on another
computer: the same monitor if it exists, else the primary; the rectangle is scaled to that
monitor's work area and kept on it. Wayland restores size and state only (§2.7).

Synced like the rest of the tab (`placement` + `placement_at`, last writer wins), only when the
server advertises it (a new feature flag, as `tabs` / `prefs` were). Saved whenever it changes,
debounced (a drag fires `Moved` many times a second). Applied **only at sign-in / launch**; a
sync merging another computer's placement while this one runs changes nothing on screen until
the next sign-in.

### 3.5 Policies

- **Nothing reconnects by itself** (as today): at sign-in detached tabs come back as their
  windows, placed and in their state, **disconnected**, with Connect.
- **Close (X)** on a detached window = Disconnect + close the tab (synced, as `tab_close`).
- **Dock** always appends at the end of the strip (a new order key) and focuses it.
- **Detach the active tab** → the tab before it (the left-hand one, else the right-hand one)
  becomes active, as a close picks its neighbour (`Workspace::close_tab`).
- **The main window minimised** → its active tab is off screen (paused), by the same rule as a
  minimised detached window (Decision 1).
- **Tab chords** (Ctrl+Alt+PageUp/Down) switch the strip's tabs; pressed in a detached window
  they do nothing there (Decision 2).
- **Sign-out, account switch, app exit, tray close**: every session ends (D6, as today); detached
  windows close; the next workspace's layout opens (disconnected).
- **> 8 connected** confirm counts connected sessions, detached or not (as today).

## 4. Phases (each ships on its own)

### S — Structure and review
09a: ownership, types, contracts, state machines, flows, invariants; adversarial review, every
finding verified against the code.

### W1 — Engine: several on screen, one focused (M) — no visible change
`Registry::{on_screen, focused}`; `Engine::set_screen(on_screen, focused)` replaces
`set_active`; per-session pause follows on-screen; clipboard and the shortcut hook follow
focused; `set_overlay(window, open)` per window (views carry their window id). The TabManager
calls it with today's single-tab answer, so nothing changes yet. Loopback: three sessions, two
on screen streaming and presenting, one paused; keys and clipboard only to the focused one.

### W2 — Engine: surface handover (M–L)
`Engine::move_session(id, surface)` + `SessionEvent::SurfaceReleased(old)`; the media thread
rebuilds presenter and decoder on the new surface and requests a keyframe; sinks, cursor
control, the view gate and the shortcut hook move with it. Loopback: move a live session between
two mock surfaces 20 times — no reconnect (same `SessionId`, same host session), pictures on the
new surface within a bound, nothing presented on the old one after its release, held keys
released on the old one.

### W3 — App: detached windows (L)
`app/src/windows.rs` (WindowManager): create / destroy detached windows on the UI thread, track
`Focused` / `Moved` / `Resized` / minimised, feed the TabManager's on-screen / focused answer,
Detach / Dock / Close; the window's page (built as `index.html?window=<remote>`, 09a §2); placement saved on this computer
(workspace, not synced yet) and restored at launch. Tray, exit, account switch close the windows.

### W4 — Sync (M)
`SyncTab.placement` + `placement_at`; Rust merge + `server/src/sync.js` merge and validation +
shared vectors; server feature flag; **server deployed before the clients that send it**; clients
send placement only to a server that advertises it.

### W5 — Per-OS finish (M)
Wayland: size and state only, documented. macOS: minimise / restore and focus events reach
`Focused` / `Resized` as on Windows (verify; tao may need `is_minimized` polling on
`Resized`), key window vs first responder for the focused session. Windows: placement across
mixed-DPI monitors (`ScaleFactorChanged`). Device checklists `checklists/DETACH-W*.md`.

## 5. Decisions (owner, 2026-11-18)

Everything in §1, and:

1. **Main window minimised** pauses its active tab, like a minimised detached window.
2. **Tab chords in a detached window** do nothing (a detached window has no strip).
3. **Monitor matching across computers**: placement is a share of a monitor's work area;
   primary = 0, then the others left-to-right (§3.4); a monitor that does not exist there
   → the primary, same proportions.

## 6. Risks

| Risk | Mitigation |
|---|---|
| Handover races (a frame presented on a surface being destroyed; input sunk into the old surface) | One handover path, owned by the media thread; old surface destroyed only on `SurfaceReleased` (W2 loopback ×20) |
| Several decoders and full streams at once (CPU / GPU / bandwidth) | Only on-screen sessions stream; minimised = paused; the > 8 confirm |
| Focus differs per OS (key window vs first responder on macOS, Wayland focus) | W5 device checks; focus is driven by `Focused` events, never inferred from clicks |
| Placement restored off-screen or tiny on another computer | Proportions of a real monitor's work area, clamped onto it; minimum size |
| Old servers / old clients | `placement` optional, capability-gated both ways; old clients ignore it (serde default) |

## 7. Design review (2026-11-18) → changes

An independent review checked every claim against the code and the Tauri / tao sources; the
findings below were verified before folding them in. **Where this section and §2–§6 or 09a
disagree, this section wins** (09a is rewritten from it before W1 starts).

### Blockers, fixed in the design

1. **Focus must come from the OS, not Tauri's `Focused`.** On Windows, Tauri synthesizes
   `Focused` from WebView2's `GotFocus` / `LostFocus` (`tauri-runtime-wry-2.12.3/src/lib.rs:522-563`,
   `:5350-5400`); our video is a separate child window, so clicking into it reports the window
   as unfocused. Window activation comes from a platform hook instead: Windows `WM_ACTIVATE` /
   foreground == `GA_ROOT` (what the keyboard hook already uses, `input/src/keyhook.rs:65-94`);
   macOS `windowDidBecomeKey` (reaches Tauri unchanged off Windows, `lib.rs:523`); GTK
   `focus-in` on Linux. Lands in **W3**, not W5.
2. **The handover keeps the decoder.** A rebuilt decoder needs a keyframe, and an idle host sends
   none (it encodes only on a captured change, `media/host.rs:655-770`; the same finding as plan
   08's P6 review). The handover rebuilds **only the presenter**, on the decoder's device: Windows
   gains `create_presenter_on(device)` (each presenter makes its own D3D11 device today,
   `platform-windows/src/adapters.rs:586`; sharing one device also saves memory with several
   windows); VideoToolbox and OpenH264 frames are taken by the new presenter as they are. After
   the swap the new surface is shown and repainted from the held frame on the next idle tick
   (W2 review: a hidden Windows swap chain skips presents, so not before the show). No latency promise until it
   is measured on devices.
3. **One current surface per session.** Today at least seven places hold the session's surface
   (`ViewerSlot.surface` `sessions.rs:406`; the driver `session/mod.rs:785-786` with
   `notify_dead`, link trouble and D10 cursor control `driver.rs:210-229, 405-417, 900-907`;
   `SessionView.surface` `active.rs:98`; the shortcut thread `mod.rs:824-846`; `Reaping.surface`
   `sessions.rs:1037-1043`; the TabManager `tabs.rs:104`). W2 replaces them with **one swappable
   cell** read by every consumer; `SessionView` swaps under its lock with "hide old" queued
   before "show new"; `end()` and `Released` cover every surface the session ever held; on the
   new surface cursor control (from `shared.control`), the cursor sprite, the shortcut hook's
   window and `ready` are re-seeded.
4. **A session can move in any phase before it ends**, not only when live: a connecting or
   password tab that is detached must not go live in the main window. `move_session` swaps the
   slot's surface under `reg` (answers `Busy` while `starting`); `SurfaceFactory::create(window,
   inset)`; the TabManager records each tab's window, and a reconnect from a detached window's
   card makes its surface in that window.
5. **Focus is sticky for the clipboard.** "Focused" = the last of our windows that had the
   keyboard, kept while another app is in front, so copying in another app still reaches the
   session you were using (as the active tab does today); a pop-out window counts as its session.

### Major, folded in

- **Ordering:** the engine binds session → window only at the swap; `set_screen` takes session
  ids only; a session shows only on its current surface once ready there (keeps hide-before-show,
  `active.rs:13-16`).
- **Minimise detection:** Windows reports it (`Resized(0,0)`); **macOS has no event** (tao has no
  `windowDidMiniaturize` handler; `isMiniaturized` must be polled or a delegate added);
  **Wayland cannot know at all** (xdg-shell has no minimised state). §2.7's "works everywhere"
  was wrong. → owner question 3.
- **Placement is recorded only in the normal state** (a minimised Windows window reports
  -32000,-32000; a maximised one its maximised rectangle); the last normal rectangle is kept
  (Windows can read `GetWindowPlacement`). macOS computes in logical points (tao scales monitor
  and window positions by different factors); Windows sets position before size (DPI change).
- **Position has its own stamp and is optional:** a computer that cannot know positions
  (Wayland) never overwrites another's; size, state and detached are stamped separately.
- **`detached` from a live sync does not change the screen:** the TabManager keeps its own
  detached set, seeded only at launch / sign-in (J6 covers it).
- **Session-bar commands take the remote** (`end_session`, `request_refresh`, `select_display`,
  `popout_display`, today on `engine.active_session()`, `commands.rs:207-287`); native menus and
  the Monitor menu follow the focused session; pop-outs are offered for any on-screen session.
- **Tab chords carry the session** (`UiEvent::TabShortcut` has none, `events.rs:37`), so a chord
  in a detached window is ignored (Decision 2).
- **Detach's neighbour:** a new helper (left, else right); `close_tab` picks right, else left.
- **Strip membership:** detached tabs **leave the strip** (owner question 1); `visible`,
  `shown_tab` and `step` work on docked tabs; `connected_count` counts all.
- **Failure paths, specified in 09a before W1:** Detach failing after the neighbour was activated
  rolls back; a session ending mid-handover detaches every surface it holds; one move at a time
  per session; `SurfaceReleased(surface)` names the surface; the macOS presenter is dropped on
  the main thread.
- **Detached windows need a capability entry** (`capabilities/default.json` lists only `main`),
  their own `CloseRequested` handling (the handlers filter on `main`, `main.rs:227-240`,
  `tray.rs:84`), and `set_video_visible` takes the caller's window.
- **Phasing:** focus and minimise detection move into W3; the synced schema (`placement` in
  `SyncTab`, merge rules, server validation, shared vectors) lands with W3 because
  `TabRecord = SyncTab` (`workspace.rs:41`); W4 only turns on sending it when the server
  advertises the feature.

### Minor, folded in

Identical Rust/JS tie-break for equal stamps; `placement_at` in `last_touched` and the server's
prune; the server validates every placement field; lenient parsing (out of range → no
placement, unknown state → normal); `primary_monitor` may be `None` on GDK Wayland (fall back to
the first monitor); partly off-screen rectangles are clamped onto the monitor; a maximised window
is first placed by its normal rectangle, then maximised; `focus_owed` follows becoming focused,
not coming on screen; Dock unminimises and raises the main window; closing windows for account
switch / exit / tray never persists `detached = false`; Wayland's separate-video-window mode is
transient for the detached window; a `tab-state` event for a window's own tab; the > 8 confirm in
a detached window; `set_size`, `set_focus`, `minimize`, `maximize`, `unminimize` cited when used.

### Owner's answers to the review's questions (2026-11-18)

4. **A detached tab leaves the strip**; Dock puts it back. **Connect all** connects detached
   windows too.
5. **The clipboard follows the window used last**, also while another app is in front; a
   pop-out window counts as its session.
6. **Wayland:** a minimised window keeps streaming (the OS does not say it is minimised);
   documented as a Wayland limit, no Pause button.
7. **Closing the main window** closes every detached window and exits. On the Windows service
   host (hides to the tray) they hide with it, the sessions end, and they come back disconnected
   when the window is shown again.
8. **A tab closed on another computer** while detached here: its window goes at the next
   sign-in, never while in use.
9. **Reopening** a tab that was detached when it was closed opens it **in the strip**; its old
   placement is forgotten.
10. **Detach is allowed while connecting or waiting for a password**; it finishes in its window.
11. **A detached window has its own Settings**; the macOS / Linux menu bar acts on the window in
    use.
12. **Position:** a computer that cannot know window positions (Wayland) keeps the position
    other computers saved; it syncs only size and state.

(Numbered after §5's decisions 1–3.)

