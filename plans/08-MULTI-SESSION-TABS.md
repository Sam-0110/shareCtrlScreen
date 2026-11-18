# 08 — Multi-session tabs and a bigger per-account sync

Written 2026-11-18 against `main` @ `3d69de4` (after PRs #21–#27). Read-only research of the
engine, all four surface backends (Windows, macOS, Linux X11, Linux Wayland), and the
config / sync / account server; every claim below cites `file:line` (paths relative to
`native/` unless they start with `server/` or `plans/`). Nothing here is built yet.
**The owner's decisions are locked in §7** (2026-11-18). Scope is **P0–P7**; P8 (host
passwords syncing between computers) is deferred until P0–P7 are released and verified.

## Progress

Execution (owner, 2026-11-18): everything is built on **one branch, `feature/multi-session-tabs`**,
off `main`. The orchestrator writes the code itself; every phase is gated (Windows + WSL) and
reviewed by an independent reviewer whose findings are verified before acting. **No release
until the end**: a final review of the whole feature, then merge to `main`, release and tests.
Each row is marked complete in the commit that finishes it.

| Step | What | Status |
|---|---|---|
| S | Structure, contracts and flows ([08a](08a-STRUCTURE-AND-FLOWS.md)), adversarial review folded in | **complete** (plan commit on the feature branch) |
| P0 | Bugs + signaling dispatcher + UI/main.rs split + server CI | **complete**: 5454fc1, reviewed; the review's blocker (a cancelled password prompt left the computer busy), two majors (D1 check not atomic with the role change; bad proofs could hold the admission) and two minors fixed in the following commit, with a loopback test (gates TABS-10 Windows + TABS-11 WSL PASS). Unverified: macOS compile of the moved menu code (CI); server-tests.yml has not run on GitHub yet; the UI split has been smoke-tested in Node, not in the app |
| P1 | Accounts own the workspace + sync v2 + synced prefs + migration | **complete**: 14e83a4, reviewed; the review's 4 majors (locked account fell back to default prefs; "Add" carried local clears; connect/switch race; downgrade lost v2 data) and 11 minors fixed in the following commit (gates TABS-18 Windows + TABS-19 WSL PASS; native CI incl. macOS PASS on 14e83a4; server 1.2.0 smoke + 14 shared merge vectors PASS locally). Unverified: the UI in the real app, a real keyring (Keychain / Secret Service / DPAPI token sealing), server-tests.yml on GitHub (runs on the final PR), the server deploy |
| P2 | Engine multi-session core | not started |
| P3 | Platform multi-surface (inset, active gate, focus, hooks, clipboard) | not started |
| P6 | Background tabs pause | not started |
| P4 | Tab UI and TabManager | not started |
| P5 | Saved tabs follow the account | not started |
| P7 | Ctrl+Tab reaches the remote | not started |
| Final | Whole-feature review, merge to `main`, release, tests | not started |

## 1. What the owner asked for

1. **Tabs.** A viewer can be connected to **several different remote machines at once**, one
   per tab, in one app window. Switching tabs switches which remote you see and control.
2. **Nameable tabs.** Each tab has a user-chosen name.
3. **Saved workspace.** The open tabs (remote, name, order) are saved. On the next sign-in —
   on any computer — the same tabs come back **disconnected**; the user clicks connect per tab
   and never has to pick the remote again.
4. **Bigger per-account settings.** More of the user's state follows their account, so
   "actions we do are saved".
5. Correct, scalable, maintainable, on **Windows, macOS and Linux (X11 + Wayland)**.

## 2. Where we are today (the facts the design must respect)

### 2.1 The engine is single-session by construction (the main cost)

| Fact | Evidence |
|---|---|
| One role for the whole app: `Idle \| Host{peer} \| Viewer{peer}` | `crates/engine/src/lib.rs:37-52`, field `:113` |
| One session slot, "one at a time" | `lib.rs:160-161` (`session: Mutex<Option<Session>>`) |
| One video surface, attached once at app start | `lib.rs:147`, `:916-923`; `app/src/main.rs:536-552`, `:982-983` |
| Per-connection state stored once: `outbound`, `pending_login`, `auto_attempted`, `negotiated` codec, `peer_os` | `lib.rs:134-154` |
| **A second viewer session tears the first one down** | `session/mod.rs:464-465` ("An offer always means a new session") |
| A dying link sets the **global** role to Idle and emits a global `RoleChanged(Idle)` | `session/driver.rs:199-227`, `:1175` |
| `end_session`, `set_video_visible`, `request_repaint`, `send_ctl`, pop-outs: no session argument | `lib.rs:793-940`; `session/mod.rs:823-1078` |
| `UiEvent`s carry no session id; only `role-changed`, `approval-request`, `password-required` carry a peer | `lib.rs:55-95`; bridge `app/src/main.rs:908-972` |
| Incoming connections are refused "busy" whenever the role is not Idle | `lib.rs:372-384` |
| No session id exists anywhere (signaling, events, commands) | `protocol/src/signaling.rs:18-154`; `session/mod.rs:140-167` |

### 2.2 What already works in our favour

- **Signaling can carry several viewer sessions.** One WebSocket per app identity, every
  message routed by peer uuid (`crates/signaling/src/lib.rs:77-83`; `lib.rs:258-327`). Two
  *different* hosts never collide. The server forwards message bodies untouched
  (`server/src/index.js:185-189`), so an optional session id can be added later without a
  server change.
- **Several native surfaces per parent window already work on every backend** (D14 pop-outs
  proved the presenter side; the research proved the same-window side):
  - Windows: all per-window state is in a per-HWND `WinState` registry since #23
    (`crates/render/src/window.rs:11-14`, `:44-121`); `create_surface` makes a new child HWND
    each call (`crates/platform-windows/src/adapters.rs:519-531`).
  - macOS: per-view ivars, layer and key monitor (`platform-macos/src/view.rs:115-135`,
    `:516`, `:582-626`); the key monitor already only acts when its view is visible + first
    responder.
  - X11: per-surface connection/window/event thread (`platform-linux/src/surface/x11.rs:167-192`);
    keys gated on `shown && has_sink` (`x11.rs:402-404`); hidden surfaces skip presenting.
  - Wayland: per-surface subsurface tree; hidden = unmapped (`wayland/mod.rs:498-631`).
- **A persistent tab bar needs new layout code on three of four backends** (corrected by the
  review, verified). The retired top-edge "reveal" is **not** an inset on Windows, X11 or
  Wayland: Windows clips the window region and keeps the full client area ("the revealed bar is
  a clip, not an offset", `render/src/window.rs:585`; `show()` resets it, `:547`), X11 uses a
  shape mask (`x11.rs:253-292`), Wayland crops the viewport source (`wayland/geometry.rs`).
  Reusing them permanently would hide the top ~40 px of the **remote screen**. Only macOS's
  `apply_layout` is a true inset (`view.rs:826-835`). So the tab bar needs a real base inset on
  every backend: Windows `MoveWindow(0, bar, w, h − bar)` in `show`/`fit`, X11 child geometry,
  Wayland subsurface position + destination size, macOS the existing frame inset — plus
  `client_size`/`ViewSize` and the pointer mapping measured on the inset area.
- **Accounts + a last-writer-wins sync document exist** (#24): `protocol/src/sync.rs`
  (`SyncDoc`, per-entry LWW, tombstones, commutative/idempotent merge, tested), mapped to
  `Config` in `protocol/src/config.rs:293-373`, synced by `app/src/account.rs`, stored and
  **merged server-side** by the `AccountStore` Durable Object (`server/src/accounts.js:147-274`).
- **No remote audio exists** (no audio code in `crates/`), so N sessions raise no audio question.

### 2.3 Blockers found outside the engine (ranked)

| # | Blocker | Platforms | Evidence | Size |
|---|---|---|---|---|
| B1 | Video surfaces always fill the whole client area → they cover any web tab bar | all | `window.rs:544-599`; `view.rs:826-835`; `gtk_host.rs:130-139` | M |
| B2 | Windows keyboard hook is process-global: last install wins, any uninstall kills it for everyone | Windows | `crates/input/src/keyhook.rs:42-51`, `:207-211`, `:249-287` | M |
| B3 | Clipboard: every session polls the one OS clipboard → **text from host A is forwarded to host B** (data leak) | all | `session/clipboard.rs:21-92`; spawned `session/mod.rs:524-538` | S |
| B4 | The engine calls `surface.show()` itself (first frame, link restore) → a background tab would pop over the active one | all | `media/viewer.rs:51`; `session/driver.rs:403`, `:417` | S–M |
| B5 | Hidden tabs keep decoding and presenting (no shown-gate on Windows/macOS), and the host keeps streaming | Win, mac | `adapters.rs:586-608`; `lib.rs:925-926` ("The stream keeps running") | M |
| B6 | Windows child surfaces are never destroyed (only with the parent) → closing tabs leaks HWNDs | Windows | `adapters.rs:528` | S |
| B7 | Wayland shortcut inhibitor is a process-global claim; 2nd session fails | Wayland | `platform-linux/src/shortcuts/wayland.rs:68-88`, `:168` | S–M |
| B8 | Wayland drop-sink keep-alive stored under one fixed key on the toplevel | Wayland | `wayland/mod.rs:80`, `:464` | S |
| B9 | X11 `XdndProxy` and the XI2 grab are per-toplevel → must hide old tab before showing new | X11 | `xdnd.rs:179-200`; `shortcuts/x11.rs:58-64`, `:600` | S |
| B10 | macOS `yield_focus` can hand first responder to another (hidden) video view | macOS | `view.rs:798-819`, `:156-159` | S |
| B11 | Windows: a hidden tab's cursor refresh calls thread-wide `SetCursor`; no `SetFocus` on tab activation | Windows | `window.rs:311-384`, `:764-786` | S |
| B12 | `bind_and_gather` (STUN/TURN, up to ~2.4 s) runs **inside** the signaling task → opening one tab stalls signaling for all | all | `session/mod.rs:493-494`; `ice.rs:60-203` | S |
| B13 | Pop-outs keyed by slot only (`u8`), labels `popout-<slot>-<n>` → collide across sessions | all | `app/src/popout.rs:40-41`, `:62-65`, `:137` | S |
| B14 | Server rebuilds the sync doc from `{v:1, entries, recentsClearedAt}` only → **today's server drops any new field** | server | `server/src/accounts.js:147-211` | S |
| B15 | Account token is plaintext in `config.json` (worth more once the doc is bigger) | all | `protocol/src/config.rs:141` | S |

Bug found during research (fix independently, now): Settings → "Forget all saved passwords"
clears `cfg.saved_logins` directly (`app/src/main.rs:356-363`) instead of
`Engine::clear_saved_logins` (`lib.rs:686-693`), so the Keychain (macOS) and Secret Service
(Linux) items are orphaned. Size S.

## 3. Target design

### 3.1 Vocabulary

| Term | Meaning | Lifetime | Owner |
|---|---|---|---|
| **Tab** | A saved slot in the workspace, **identified by its remote ID**: `{remoteId, name, order}` | Until the user closes it; persisted + synced | App (`TabManager`) |
| **Session** | One live connection to one host | One connect attempt → disconnect | Engine (`SessionId`) |
| **Active tab** | The tab currently shown and receiving input | Per device, never synced | App |

A tab has **zero or one** session. Reconnecting a tab creates a **new** `SessionId`, so late
events from a dead session can never be applied to its successor.

**Tab identity is the remote ID** (from the structure review, [08a §3](08a-STRUCTURE-AND-FLOWS.md)).
A random tab id would let two computers each create a tab for the same remote and end up with
duplicates after sync; keying by remote ID makes them converge on one tab and makes "one tab
per remote" (§3.6) true by construction. Commands below that take a `tabId` take the remote ID.

### 3.2 Engine: a session registry instead of a slot

```
Engine
├── host:    Mutex<Option<HostSession>>        // unchanged semantics: one incoming viewer
└── viewers: Mutex<HashMap<SessionId, ViewerSession>>
             ViewerSession { id, peer, phase, permission, negotiated, peer_os,
                             pending_login, auto_attempted, surface, active: AtomicBool,
                             session: Session /* threads, Rtc, shared */ }
SessionId = u64 (monotonic, process-local; never persisted)
```

- **Lookup by peer** for inbound signaling (`peers: HashMap<PeerUuid, SessionId>`): routing
  stays peer-based (§2.2), which is unambiguous because **one tab per remote** (§3.6).
- Everything that was "the" session takes a `SessionId`: `end_session(id)`,
  `set_session_active(id, bool)`, `request_refresh(id)`, `select_display(id, ..)`,
  `popout_display(id, ..)`, `request_repaint(id)`, `send_ctl(id, ..)`.
- **Per-session teardown**: `start_viewer` stops calling the global `teardown`
  (`session/mod.rs:465`); a session's death marks **that** session dead and emits
  `Session{id, Ended{reason}}` — it never touches the host slot or other viewers
  (`driver.rs:199-227` becomes per-session).
- `role` is no longer stored; `Engine::status()` derives `{hosting: Option<..>, viewers: [..]}`.
- `bind_and_gather` moves to `spawn_blocking` / a worker so tab N's ICE setup never stalls
  signaling (B12).
- **Host + viewer at the same time** is a policy, not a mechanism (§7 decision D1). Phase 1
  keeps today's behaviour: incoming requests are refused "busy" while **any** session is live.

### 3.3 Events: tagged, plus a snapshot

- `UiEvent::Session { id: SessionId, peer, ev: SessionEvent }` where `SessionEvent` covers
  what is untagged today: `Phase{Connecting|PasswordRequired|Live|Ended(reason)}`,
  `LinkTrouble`, `LinkRestored`, `Displays`, `Toast`, `Permission`.
- The app's `TabManager` owns the **tab ↔ session** map and emits one `tabs-changed`
  event with the **full snapshot** `{tabs:[{remoteId, name, order, sessionId?,
  phase, permission}], activeRemoteId}` after every change. A full snapshot (a few hundred
  bytes) is simpler and harder to get wrong than incremental UI patches.
- Transient per-session events (`link-trouble`, `toast`, `displays`) are forwarded with the
  `remoteId`, so the UI updates the right tab's badge.

### 3.4 App layer: `TabManager` (new `app/src/tabs.rs`)

The single authority for tabs. The web UI renders and calls commands; it holds no tab truth.

| Command | Does |
|---|---|
| `tab_open(remoteId)` | Focus the existing tab for that remote, or create one (+ name default = the remote's saved name or its ID) |
| `tab_connect(remoteId)` / `tab_disconnect(remoteId)` | Start/stop that tab's session |
| `tab_activate(remoteId)` | Make it active (see switch sequence below) |
| `tab_close(remoteId)` | Disconnect, destroy its surface, tombstone it |
| `tab_rename(remoteId, name)` / `tab_move(remoteId, beforeRemoteId?)` | Edit; stamps for sync |
| `tab_connect_all()` | Restore helper: connect every disconnected tab, **staggered** (one start per 500 ms; §5); one confirm if the result passes 8 connected |
| `tab_disconnect_all()` | Used by sign-out, account switch and app close (D6) |

**Tab switch sequence** (the order matters on X11 and for focus):
1. Old tab: `set_session_active(old, false)` → hide its surface, clipboard gate off, shortcut
   forwarder detached, host asked to pause (§3.7).
2. New tab: `set_session_active(new, true)` → show + raise its surface, `SetFocus` (Windows) /
   `makeFirstResponder` (macOS) / grab re-evaluated (X11), clipboard gate on, shortcut
   forwarder attached, host asked to resume + keyframe.
3. Emit `tabs-changed`.

Surfaces: one per **connected** tab, created on connect via the existing
`Platform::create_surface(main window)` path (`popout.rs:280-306` shows the threading) and
**destroyed** on disconnect/close. Disconnected tabs hold no surface, no decoder, no threads.

### 3.5 Platform work (per backend)

| Change | Windows | macOS | X11 | Wayland |
|---|---|---|---|---|
| **Top inset for a persistent tab bar** (new defaulted `VideoSurface::set_top_inset(css_px)`; **new base-inset code**, separate from the reveal — §2.2) | `MoveWindow` offset in `show`/`fit`; client size + input mapping on the inset area | `apply_layout` frame inset (already a true inset) | child window geometry + input offset | subsurface position + destination size |
| Active gate: engine never shows a surface; only `set_session_active` does (B4) | engine | engine | engine | engine |
| Skip present while hidden (B5) | add gate in `D3dPresenter::present` | add gate in `MetalPresenter` | already | already |
| Destroy surface (B6) — **explicit `SurfaceFactory::destroy` on the UI thread after the session's `Released`** (08a §6.5), never on whichever thread drops the last `Arc` | explicit `DestroyWindow` + `WinState` removal | already `removeFromSuperview` in `Drop` (`surface.rs:220-249`); call it on the main thread | per-window teardown | destroy subsurfaces |
| Focus on activate | `SetFocus(hwnd)` + D11 `reevaluate_pointer_inside` | `makeFirstResponder`; fix `yield_focus` to skip video views (B10) | hide-before-show order (B9) | — |
| Cursor | gate `WM_REFRESH_CURSOR` on visibility (B11) | rects already per view | per window | n/a |
| Shortcut capture → **app-level, one owner, switchable target** | one app-lifetime `keyhook` with `set_target(forwarder)` (B2) | per-view taps already gate on `has_key_focus` | per-surface grabs already gate on viewable | refcounted app-level inhibitor (B7), **released whenever no connected tab is active** so the compositor's own shortcuts work on a disconnected card |
| Drop sink | per HWND, fine | per view, fine | refcount `XdndProxy` or hide-before-show (B9) | per-surface `SINK_KEY` (B8) |

`set_top_inset` is the only `platform-api` trait change. It is additive and defaulted, like
D10's `local_pointer`/`set_cursor_control`.

### 3.6 Policies that keep it simple and correct

- **One tab per remote.** `tab_open` of an already-open remote focuses that tab. Hosts are
  single-viewer anyway (`lib.rs:373-383`), and it keeps peer-keyed routing unambiguous.
- **No hard tab cap; a confirm past 8 (D2).** Opening or connecting a 9th live tab shows a
  confirm dialog: "You're about to have 9 connected tabs. Each one uses its own connection
  and video decoder, and this computer may slow down. Continue?" — Cancel / Continue. The
  warning threshold is a constant (`TAB_WARN_AT = 8`). It counts **connected** tabs
  (disconnected tabs cost nothing), and **Connect all** asks once for the whole batch, not
  per tab.
- **Not while hosting (D1, unchanged behaviour).** While this computer is being controlled,
  outgoing tab connections are refused, and while any tab is connected, incoming requests get
  "busy" — exactly as today with one session.
- **Clipboard follows the active tab only** (fixes B3). Each session already has a live
  `clip_on` gate (`session/mod.rs:526`); the switch sequence toggles it, and inbound clipboard
  for an inactive tab is dropped (`clipboard.rs:69`). **Gating alone does not stop the leak**
  (review M13, verified at `clipboard.rs:80-92`): on re-enable the loop sees a changed counter
  and sends whatever is on the clipboard — text copied from host A goes straight to host B. So
  activation also **re-baselines** the loop (`last_count`, `last_text` = the current
  clipboard): only copies made while a tab is active are sent to its host (08a §6.4 step 4).
- **Pop-outs belong to their session**: keyed `(SessionId, slot)` with labels
  `popout-<session>-<slot>-<n>` (B13). They stay open when their tab goes to the background
  (they are separate windows), and close with their session.
- **Fullscreen**: the tab bar collapses (inset 0). Windows keeps the top-edge reveal to bring
  it back; macOS/Linux get a native **Tabs** submenu (list / switch / rename / close / new)
  next to M15's Session menu, so tabs stay reachable without a visible bar.
- **Local tab-switch shortcuts that are never forwarded (D3).** Next / previous tab only, on
  PageUp/PageDown as the owner asked: Windows/Linux `Ctrl+Alt+PageDown` (next) /
  `Ctrl+Alt+PageUp` (previous); macOS `Cmd+Option+PageDown` / `PageUp` (Fn+↓/↑ on laptop
  keyboards). The earlier macOS proposal `Cmd+Option+←/→` is dropped: it is the Mac browsers'
  own tab switch, so Mac users controlling a Mac would lose it. They wrap around and work in
  fullscreen. **They must work with shortcut capture off** (the default, `config.rs:174`;
  the Windows hook only takes Win and Alt+Tab/Esc, `keyhook.rs:97-108`), so they are filtered
  in the engine's per-session key path, always, plus the web view's keydown and the macOS
  menu's key equivalents (08a §6.19).
- **Known bug (owner report):** Ctrl+Tab does **not** reach the remote today, although capture
  is supposed to forward it. It is tangled with window/focus handling, so it is fixed **last**
  (P7), on top of P3's rebuilt app-level shortcut owner rather than the code P3 replaces. The
  tab shortcuts above use different chords, so they do not wait on it.

### 3.7 Bandwidth: background tabs pause

- New optional control messages `vpause` / `vresume` (resume forces a keyframe), advertised
  through the existing `caps` mechanism (`protocol/src/control.rs`; D14 used `caps.popout`).
  The host stops capturing+encoding slot 0 for a paused viewer; the ctl channel, cursor and
  heartbeat stay up.
- **Old hosts** (no cap): the viewer drops frames for a background tab without decoding and
  sends a `KeyframeRequest` on activation (both already exist). Bandwidth is still spent, but
  CPU/GPU is not.
- Result: only the active tab (plus any open pop-outs) streams, so the hosts' independent
  bandwidth estimators (`driver.rs:343-349`) do not fight over the downlink.

### 3.8 Persistence and sync: the account owns the workspace

**Decision D4.** An account is the owner of the **whole user workspace**: its tabs, its
synced preferences, its named remote IDs and its recents. The app can hold **several accounts**
and switch between them; switching swaps the entire workspace. Device settings (this machine's
ID, host password, codec, autostart, …) belong to the **computer** and never change when the
account changes. Saved host passwords belong to the account's workspace (§3.8 "Host passwords").

**No account yet → local workspace (owner, 2026-11-18).** Without an account the app works
fully: tabs, names, recents, preferences and saved host passwords all save on this computer
in the `"local"` workspace. **Creating an account adopts it:** the local workspace becomes the
new account's workspace and syncs from then on, so nothing the user built before signing up is
lost. Signing in to an **existing** account while the local workspace has data asks once:
"Bring this computer's 3 tabs and 12 names into *account*?" — Add / Keep separate (D4,
decided). "Add" merges them in with the normal sync rules (a remote that is already a tab in
the account is not duplicated); "Keep separate" leaves them in the local workspace, which comes
back on sign-out.

**Config shape** (`protocol/src/config.rs`). Today the user state sits at the top level next
to device settings (`config.rs:60-118`), which only works for one account. It splits in two:

```
Config {
  // ---- device: never synced, never per-account (unchanged fields) ----
  uuid, server_url, mode, password_hash, password_permission, ice_servers,
  share_audio, share_display_id, start_with_windows, use_openh264,

  // ---- accounts known on this computer ----
  accounts:       Vec<AccountRef>         // remembered accounts, for the switcher
  active_account: Option<AccountKey>      // None = local workspace
  workspaces:     BTreeMap<WorkspaceKey, Workspace>   // "local" + one per account
}
AccountRef { server, username, keep_signed_in: bool, last_sync }   // NO token here
AccountKey = "<server>|<username>"        WorkspaceKey = "local" | AccountKey

Workspace {                               // everything that follows the user
  recent_ids, remote_names, id_stamps, recents_cleared_at,   // moved here from the top level
  tabs:        BTreeMap<RemoteId, TabRecord>  // keyed by remote; live and closed (tombstones)
  active_tab:  Option<RemoteId>           // per computer, never synced
  prefs:       Prefs                      // the synced-preference values (whitelist below)
  pref_stamps: BTreeMap<PrefKey, u64>
  saved_logins: BTreeMap<HostId, Sealed>  // moved here from the device level; per workspace
}
TabRecord  { name, name_at, order: String /*fractional key*/, order_at,
             opened_at, closed_at }   // live ⇔ opened_at > closed_at
```

- **Tokens** are sealed with the existing `SecretStore` (`seal(plain) → opaque handle`,
  `platform-api/src/os.rs:38-51`); `AccountRef.token_ref` holds the handle, never the token
  (fixes B15). The store hands out **random** ids on macOS/Linux and the DPAPI ciphertext on
  Windows, so isolation between accounts comes from **which workspace map holds a handle**,
  not from how the secret is keyed; nothing is ever re-keyed in the store.
- **Migration from today's file** (one-time, in the **engine** after load, because it needs
  the secret store; the pure transform lives in `protocol::config::migrate`). Full steps in
  [08a §6.13](08a-STRUCTURE-AND-FLOWS.md); the essentials:
  - names, recents, stamps, saved logins and the synced prefs move into the workspace of the
    account signed in today, or into `"local"`;
  - `auto_login == false` ⇒ the saved logins are **forgotten**, not moved (retiring the toggle
    must not start using passwords the user told us not to use);
  - a backup `config.v1.json` is written; the plaintext token is removed only after it is
    sealed (Linux with no keyring: kept in memory for this run, sign in again next time);
  - **any migration error ⇒ keep running on the parsed v1 values without persisting** — never
    on defaults, because `Config::default()` makes a new machine ID (`config.rs:157`; today a
    parse error renames the file to `.bad` and does exactly that, `config.rs:450-470`).
- **Each workspace syncs only with its own account.** `sync_doc` / `apply_sync_doc` read and
  write the active workspace only. The `"local"` workspace does not sync while it is local;
  creating an account re-keys it to that account and it syncs from then on.

**Account lifecycle (decision D6):**

| Event | What happens |
|---|---|
| Sign up (new account) | The local workspace **becomes** the account's; connected tabs **stay connected** (same tabs); a sync uploads it |
| Sign in to an existing account | No local data → switch to the account's workspace. Local data → "Bring this computer's tabs and names in?" — **Add** merges them (connected tabs stay connected; the account's settings win), **Keep separate** disconnects them and switches. New connections are blocked until answered |
| **Keep me signed in** ticked at sign-in | The token is stored in the secret store, and on the next launch the app signs that account in **automatically** |
| Keep me signed in **not** ticked | The token is kept in memory only; on the next launch the account is remembered in the switcher but asks for the password |
| Switch account | Same as sign-in: disconnect all, swap workspace |
| Sign out | Disconnect all tabs; delete the token; switch to the local workspace (the account stays in the switcher until "Remove") |
| Close the app | All connected tabs disconnect (the sessions end; the tab list is kept) |
| Next launch | Auto sign-in if kept; the tabs come back disconnected with **Connect all** |

**`SyncDoc` v2** (`protocol/src/sync.rs`), added as optional fields so v1 readers ignore them
(`SyncDoc` has `serde(default)` and no `deny_unknown_fields`, tested at `sync.rs:215-227`):

```
tabs:  BTreeMap<RemoteId, SyncTab { name, nameAt, order, orderAt, openedAt, closedAt }>
prefs: BTreeMap<PrefKey, SyncPref { value: serde_json::Value, at }>
```

Merge rules (same style as names today, `sync.rs:43-49`):
- Per tab, `(nameAt, name)`, `(orderAt, order)`, `openedAt` and `closedAt` merge
  **independently** — newer stamp wins (`openedAt`/`closedAt` take the max), ties broken by
  code-point order of the value. A rename on laptop A and a reorder on desktop B both survive.
- A tab is live when `openedAt > closedAt`; closing stamps `closedAt`, reopening the same
  remote stamps a later `openedAt`. Closed tabs are pruned after 90 days (not by count first),
  so an old device cannot resurrect a recently closed tab.
- A tab that is **live on this computer** is never closed by an incoming sync; it is
  re-stamped open (the local action wins while it is in use, 08a §6.12).
- `order` is a **fractional index string** (insert between two keys without renumbering), so
  concurrent reorders converge without conflicts.
- Prefs: per key `(at, value)`, newer wins, tie-break on the serialized value.
- **Active tab is not synced** (two devices used at once would fight over it).
- Named remote IDs and recents keep today's rules (`sync.rs:43-129`); they simply live in the
  account's workspace now, so two accounts on one computer never mix their names.

**Which preferences sync** (decision D5: whitelist; everything else stays on the device):

| Syncs | Stays on this device (and why) |
|---|---|
| `translate_modifiers`, `clipboard_sync`, `paste_dropped_files`, `capture_shortcuts`¹ — stored in the workspace's `prefs`, booleans only | `uuid`, `server_url`, accounts + tokens, `mode` / `password_hash` / `password_permission` (host security), `ice_servers`, `share_display_id`, `start_with_windows`, `use_openh264` (licence + codec per machine), `share_audio` (host-side, no audio yet). Saved **host passwords** are per workspace but stay on the computer until P8 |

**Owner decision after review (2026-11-18): the clipboard setting follows the account as one
setting — "it's just another setting".** The review (C2, verified) pointed out that
`clipboard_sync` and `paste_dropped_files` also act on the **host** side
(`session/mod.rs:329`, `:371`, `:415`), so a change made on one computer changes what another
computer allows when it is controlled. The owner accepts that: the settings stay single and
follow the account. They are still **not** enough on their own to share a host's clipboard —
the host side also requires the live Control permission (`session/mod.rs:329`).

¹ On macOS turning `capture_shortcuts` on triggers the Input Monitoring preflight
(`app/ui/app.js:883-888`); a synced "on" is applied but the preflight runs on first use, not
at sync time.

**Host passwords: typed once per host, then remembered (owner, 2026-11-18).** When a tab's
host asks for a password, the user types it once; it is saved and every later connect to that
host — including all of **Connect all** — signs in by itself.

- **Today** a host password can already be saved: sealed with DPAPI / Keychain / Secret
  Service (`platform-*/src/secret.rs`), keyed by host ID (`config.rs:117`), and replayed on the
  next challenge (`lib.rs:547-600`, `:627-668`). But the "Remember" box starts **unchecked**
  until it has been ticked once (`app/ui/app.js:1000-1001`), and the store is shared by
  everyone on the computer.
- **New:** "Remember this password" is **checked by default**. The saved password belongs to
  the **active workspace** (`Workspace.saved_logins`), so account A's host passwords are never
  used or shown under account B. The workspace holds only the store's opaque handle
  (`SecretRef`); `config.json` never holds the password. An account's workspace — and so its
  saved passwords — is reachable **only while that account is signed in** (review M16): a
  signed-out account shows a sign-in form, not its tabs.
- A stale saved password (the host changed it) is detected only in the host's terminal
  "bad-password" reply (`lib.rs:715-728`; the "second challenge" path rarely fires). The engine
  then forgets it, sends a **fresh connect request** to get a new challenge, and prompts in the
  tab ("The saved password no longer works") — 08a §6.3.
- Each tab gets a **"signs in automatically"** key icon from a per-host boolean (never the
  value) plus a "Forget saved password" item in the tab's menu.
- **Password prompts are per tab, not one global modal.** With several tabs connecting at once,
  each tab that needs a password shows its own prompt inside the tab and waits; the others keep
  connecting. This needs the engine's single `pending_login` / `auto_attempted` slots
  (`lib.rs:134-138`) to become per session (P2).
- **Across computers (D7):** sealed passwords only open on the computer that sealed them, so
  today they cannot follow the account. Syncing them needs **end-to-end encryption**: a vault
  key derived from the account password on the client (the server only ever stores
  ciphertext), like a password manager. That is its own phase (P8), after P5.

**Server** (`server/src/accounts.js`):
- `normalizeDoc` / `mergeDocs` / `emptyDoc` learn `tabs` and `prefs` with the same rules,
  validation (tab key = remote ID = uuid v4, name ≤ 64 chars, ≤ 100 live + ≤ 200 closed tabs as
  an **abuse guard only** — D2 has no product cap — pref keys from a fixed whitelist, value
  ≤ 1 KB) and future-timestamp clamp.
- The reply advertises `features: ["tabs","prefs"]`. A client only *writes* the new fields
  after seeing them advertised, so a new client against an old server never silently loses
  tabs (B14). Deploy order: **server first** (auto-deploy on the version bump, set up in #26).
- Server replies include `serverTime`; the client stamps with a corrected clock, so a device
  whose clock runs behind does not lose every LWW race.
- Size: whole doc is one KV value; 8 tabs + 200 names is a few KB against a 256 KB request cap
  and Cloudflare's 2 MB value limit.

**Rust ⇄ JS merge parity.** The merge exists twice (Rust client, JS server). Add one JSON
file of test vectors (`protocol/testdata/sync-vectors.json`: inputs + expected merged doc)
that **both** `cargo test -p protocol` and `node server/test/sync-vectors.mjs` run. Without
it the two drift and devices flip-flop.

**When to sync:** today's triggers (`app.js` boot, debounced after changes, focus, every
5 min) plus: after `saveSettings` (missing today), after every tab command, and once right
after a sign-in or account switch.

**Server impact of several accounts per computer: none.** Each account is already its own
Durable Object keyed by username (`server/src/accounts.js:329-331`), and a computer simply
holds one token per remembered account.

### 3.9 UI (`app/ui/`)

- **Tab strip** at the top of the window, in the inset area: name, connection dot
  (disconnected / connecting / live / trouble), a "will sign in automatically" key icon,
  close ×, and a **+** that opens the connect box. Double-click (or F2) renames inline.
  Drag to reorder.
- A **disconnected tab** shows a card: remote name/ID, "Connect" (Enter), last-used time.
  The home screen becomes the "+ / new tab" page; recents and named IDs stay there.
- Restore after sign-in or launch: tabs appear disconnected with one **Connect all** button.
- **Account switcher** next to the tab strip: shows the active account (or "Local"), lists
  remembered accounts, "Add account…", "Sign out", "Remove from this computer". Switching
  warns first if tabs are connected ("This disconnects your 3 open sessions").
- Sign-in form gains **Keep me signed in** (checked by default).
- The 9th-connection confirm dialog (§3.6).
- `app.js`'s single `state` (`app.js:16-62`) splits into **app state** (signed-in, hosting,
  incoming request modal) and **per-tab state** from `tabs-changed`. Host-side screens are
  unchanged.
- The Windows `.view-bar` (Disconnect / Refresh / Monitor / Pop out / Settings) acts on the
  active tab; the macOS/Linux Session + Monitor menus do the same.

## 4. Phases (each one ships on its own)

Sizes: S ≤ 1 day, M 2–4 days, L 1–2 weeks of agent time. Every phase: Windows + WSL lane
gates, platform-macos aarch64 check, loopback ×3, and a device checklist under
`plans/checklists/`.

```
S ── P0 (M) ─┬─ P1 Accounts + sync v2 (L) ── merges first ──────────────────────┐
             └─ P2 Engine multi-session core (L) ─┬─ P3 Platform multi-surface (L) ─┴─ P4 Tab UI (L) ── P5 Saved tabs (M)
                                                  └─ P6 Background pause (M)
after P3 + P6 ── P7 Ctrl+Tab forwarding fix (M, last in scope)
after P5 ────── P8 Host passwords sync, E2E (deferred; needs a new login protocol first)
```

### Step S — Structure, contracts and flows first (S) — before everything
- [08a-STRUCTURE-AND-FLOWS.md](08a-STRUCTURE-AND-FLOWS.md): layering and ownership, file
  layout, core types, every Tauri command and event, the state machines, all flows step by
  step, cross-cutting rules and invariants.
- **Adversarially reviewed (2026-11-18): NEEDS-REWORK, 2 critical / 19 major / 15 minor.**
  Every finding was verified against the code by the orchestrator, and all were accepted (one,
  minor 10, accepted with a corrected rationale). Fixes are in 08a rev. 2 (its §10 maps each
  finding to its fix) and in this document. Builders implement **against 08a**; a change to a
  contract there needs the orchestrator's approval, like a `platform-api` change today.

### P0 — Prerequisites, bugs and groundwork (M)
- Fix "Forget all saved passwords" (use `Engine::clear_saved_logins`).
- **Signaling dispatcher:** per-message work (ICE gathering, secret-store opens, the host
  approval decision, the bad-password delay, `begin_host`, `start_viewer`) moves into tasks
  keyed by peer; the loop only dispatches (B12 and review M5 — one Keychain prompt or one
  approval no longer freezes everything).
- **Split without behaviour change:** `app/ui/app.js` into ES modules and `app/src/main.rs`
  into `commands.rs` / `bridge.rs` / `menu.rs` (08a §2.3–2.4), so P1 and P4 do not both rewrite
  one 1,200-line file.
- **Server CI:** a Node job running the accounts smoke test (and the sync vectors once P1 adds
  them); Deploy Server waits for it (review M18 — today the server auto-deploys untested).

### P1 — Accounts own the workspace + sync v2 + synced preferences (L) — after P0
(Overlaps P2 in `engine/src/lib.rs` around saved logins: **P1 merges first**, P2 rebases. The
review sized the original "M–L" as underestimated; it is L.)
- `Config` split into device settings + `workspaces` (§3.8), the one-time migration of
  today's file, and a migration test on a captured 2.1.x `config.json`.
- **Several accounts per computer:** `accounts` list, `active_account`, account switcher,
  "Remove from this computer"; sign-in / switch / sign-out disconnect everything first (D6).
- **Keep me signed in** + tokens in the OS secret store per account (fixes B15); auto sign-in
  at launch when kept.
- **Local workspace adopted on sign-up**; "Bring this computer's tabs and names into
  *account*?" on sign-in to an existing account when the local workspace has data.
- **Host passwords per workspace, remembered by default** (§3.8): `saved_logins` (handles)
  move into the workspace, the "Remember" box starts checked, `auto_login` is retired (saved
  logins of users who had it off are forgotten), and "Forget all saved passwords" (fixed in P0)
  clears only the active workspace's.
- **Synced settings:** `translate_modifiers`, `clipboard_sync`, `paste_dropped_files`,
  `capture_shortcuts` move into the workspace's `prefs` and follow the account (D5).
- **Accounts are isolated:** a signed-out account's workspace is not shown until its password
  is entered (`Locked`, 08a §5.3).
- `SyncDoc` v2 `prefs` (the tabs schema is defined now, used in P5), merge/normalize/prune,
  Rust tests.
- Server: same rules, `features`, `serverTime`, validation; `server/package.json` → 1.2.0 so
  the auto-deploy ships it.
- Shared test vectors run by Rust and Node.
- Whitelisted prefs stamped in `save_settings` **only when their value changes** (Save sends
  every field today, so stamping on Save would let a stale computer overwrite newer changes —
  review M3); `scheduleSync` after Save.
- `Stamper`: monotonic, server-skew-corrected stamps, and every loaded or merged stamp clamped,
  in Rust and in the server (review M2).
- **Acceptance:** with accounts A and B on one computer, switching shows each account's own
  names, recents and preferences and never mixes them; "translate modifiers" changed on computer
  1 under account A flips on computer 2 under account A within one sync; a kept account signs
  in by itself after a restart, an unkept one asks for the password; upgrading from 2.1.x keeps
  every name, recent and saved host password; names created before sign-up appear in the new
  account and on a second computer after it signs in; a host password saved under account A is
  not used under account B; an old app on the new server keeps working; a new app on an old
  server keeps prefs local.

### P2 — Engine multi-session core (L) — no visible change yet
- `SessionId`, `ViewerSession` registry, per-session state (§3.2), per-session teardown and
  death, tagged `UiEvent`s, session-taking commands. The app still opens one tab, so behaviour
  is identical — this lands the risky refactor on its own.
- **Loopback test (new):** one viewer engine, **two host engines** (different uuids) on the
  mock signaling: both live at once; ending one leaves the other live; a link death in one emits
  only its own `Ended`; events are tagged correctly; a second connect never tears down the first.
- **Acceptance:** all existing loopback/engine tests green; the new two-host test green ×3.

### P3 — Platform multi-surface (L) — needs P2
- `VideoSurface::set_top_inset` as **new base-inset layout code** on Windows, X11 and Wayland
  (§2.2) plus macOS's existing inset, with client size and pointer mapping on the inset area;
  explicit UI-thread surface destroy after `Released` (B6); present gate when hidden (B5); the
  ActiveGate as the only caller of show/hide, applied in one UI-thread closure (B4, review
  M8); focus-on-activate; the X11/Wayland/macOS fixes (B7–B11); app-level shortcut owner with
  switchable target (B2); clipboard gate **with re-baseline** on activation (B3, review M13);
  `ui_overlay` hiding the video under web dialogs (review M11).
- **Acceptance (device, per OS):** two sessions alive, switch 20× — only the active video is
  visible and receives keys/mouse/clipboard; Alt+Tab / Cmd+Tab captured only for the active
  tab; closing a tab frees its surface (no leaked windows); drag-drop lands on the active host.

### P4 — Tab UI and `TabManager` (L) — needs P1, P2, P3
- `app/src/tabs.rs`, the commands in §3.4, `tabs-changed` snapshot, tab strip, rename,
  reorder, close, `+`, disconnected-tab card, native **Tabs** submenu (macOS/Linux), local
  switch shortcuts (next/previous only), pop-outs namespaced per session (B13), Windows
  `.view-bar` + menus act on the active tab, the confirm past 8 connected tabs, and
  disconnect-all on close / sign-out / account switch. Tabs persist in the active
  **workspace** (no sync yet). **Password prompts live inside their tab** (one per tab,
  concurrent), with the "signs in automatically" icon and "Forget saved password" per tab.
- **Acceptance:** open 3 tabs to 3 hosts, rename, reorder, switch by click and shortcut,
  close one, quit (all sessions end), relaunch → the 3 tabs return disconnected with their
  names and order; connecting a 9th tab asks first; account B shows none of account A's tabs;
  8 tabs to 8 password-protected hosts: each password is typed once, and after a restart
  **Connect all** brings all 8 up with no prompt.

### P5 — Saved tabs follow the account (M) — needs P1, P4
- `tabs` in `SyncDoc` (schema from P1), `Config` ⇄ doc mapping, tombstones, fractional order,
  server validation, **Connect all** with the 500 ms stagger, per-host "auto sign-in" flag.
- **Acceptance:** sign in on laptop A, open 3 named tabs; sign in on desktop B → the same 3
  tabs appear disconnected; rename on B shows on A after the next sync; close on A disappears
  on B and does not come back after B was offline for a week.

### P6 — Background tabs pause (M) — needs P2
- `vpause`/`vresume` + `caps.vpause`, host capture/encode pause for slot 0, viewer
  drop-without-decode + keyframe-on-activate fallback for old hosts.
- **Acceptance:** 3 live tabs, 2 in the background → host logs show the background hosts
  sending ~0 kbps video; switching to a background tab shows a picture within ~1 s.

### P7 — Ctrl+Tab reaches the remote (M) — last in scope; needs P3, done after P6
- Owner report: Ctrl+Tab is not forwarded to the remote today; the bug involves window /
  focus handling. Fixed last so it is debugged once, against P3's app-level shortcut owner
  and the multi-surface focus rules, not against code P3 replaces.
- Find which layer drops it on each OS (the Windows keyboard hook and WebView2 accelerator
  handling; the macOS event tap / key monitor; the Linux XI2 grab and Wayland inhibitor) and
  fix it there. Check the neighbours too: Ctrl+Shift+Tab, Ctrl+PageUp/PageDown, Alt+Tab.
- Add these chords to every OS's capture checklist.
- **Acceptance (device, every OS):** with capture on and a tab active, Ctrl+Tab and
  Ctrl+Shift+Tab switch tabs **on the remote** (e.g. in its browser), never in our app; our own
  tab shortcuts (§3.6) still switch our tabs.

### P8 — Host passwords follow the account, end-to-end encrypted (M–L) — needs P5; **deferred** (D7: decide after P0–P7 ship)
- **Prerequisite: a new login protocol.** Today sign-in sends the account password itself to
  the server (`server/src/accounts.js:357-366`), so any key derived from it is derivable by the
  server. The client must derive, from the password, an *auth key* (sent instead of the
  password) and a separate *vault key* (never sent) — or adopt OPAQUE — plus a
  password-change endpoint that re-wraps the vault.
- Then a per-account **vault key** wraps a random data key; the server stores only ciphertext
  and the wrapped key, and can no longer read a host password.
- Vault entries `(HostId → encrypted password)` sync with the same per-entry last-writer-wins
  and tombstones as names.
- A password change re-wraps the data key; **a forgotten account password loses the vault**
  (by design — say so in the UI).
- **Acceptance:** a host password typed on computer 1 signs that host in on computer 2 under
  the same account; the server's stored document contains no readable password.

**Parallelism:** Step S first, then P0. After P0, P1 and P2 run at the same time but **P1
merges first** (they overlap in `engine/src/lib.rs`). P3 and P6 both follow P2 and can run in
parallel. P4 needs P1 + P3 (tabs are stored in P1's workspace). P5 needs P4. P7 comes last.

## 5. Scalability and limits

| Concern | Bound |
|---|---|
| Threads | ~6 per **live** session (`session/mod.rs:519-645`); disconnected tabs cost 0. 8 live ≈ 48 threads |
| Decoders / GPU | One decoder + presenter per live session; background tabs paused (P6) do no decode. Hardware decoders have per-device session limits — fall back to software per session on open failure (the existing W2/D2 path). That fallback exists for **H.264 only** (`codec-openh264/src/fallback.rs`); an HEVC/AV1 session that hits the limit ends with a clear message, and the viewer prefers H.264 when it already holds several hardware sessions |
| Downlink | Only the active tab streams (P6) |
| Signaling | Server drops > 30 msgs/s per socket (`server/src/index.js:21`, `:74-80`); candidates ride in the SDP, so a connect is a handful of messages. **Connect all** staggers 500 ms per tab |
| Sync doc | 8 tabs + 200 names + prefs ≈ a few KB; caps enforced server-side |
| Restore | Disconnected by default (D6); no surprise connections on sign-in or launch |
| Tab count | No hard cap (D2); the confirm past 8 connected tabs is the guard rail |

## 6. Testing

- **Unit:** sync merge for tabs/prefs (commutative, associative, idempotent; rename-vs-reorder;
  tombstone vs late edit; fractional ordering); config migration from today's file.
- **Shared vectors:** the same JSON cases in Rust and Node (§3.8).
- **Loopback:** the new two-host test (P2); pause/resume (P6); a tab-switch test against
  `MockSurface` asserting only the active surface is shown and receives input.
- **Server smoke:** extend `server/test/accounts-smoke.mjs` with tabs/prefs, caps, old-client
  posts (must not drop tabs), `features`, `serverTime`.
- **Device checklists:** one per phase, per OS (Windows, macOS, X11, Wayland), covering focus,
  shortcuts, clipboard isolation, drag-drop, pop-outs, fullscreen tab access.

## 7. Decisions (owner, 2026-11-18)

| # | Question | Decision |
|---|---|---|
| D1 | May a machine **host** while it is viewing tabs? | **No — keep today's behaviour**: "busy" while any session is live, both ways |
| D2 | Max tabs | **No hard cap.** Past **8 connected** tabs, a strong warning with a confirm dialog (§3.6) |
| D3 | Tab-switch shortcuts | **Next / previous only, no numbers, PageUp/PageDown.** Win/Linux `Ctrl+Alt+PageUp/PageDown`; macOS `Cmd+Option+PageUp/PageDown` (amended after review: `Cmd+Option+←/→` is the Mac browsers' own tab switch). Work with capture off. Owner notes Ctrl+Tab is not forwarded to the remote today — a bug, fixed **last** (P7) |
| D4 | Who owns tabs and settings? | **The account owns the whole workspace** (tabs, synced settings, names, recents, saved host passwords). One account at a time, several accounts per computer with a switcher. **Without an account everything saves locally; creating an account adopts it and syncs it.** **Signing in to an existing account with local data asks "Bring this computer's tabs and names into *account*?" — Add / Keep separate** (decided) |
| D5 | Synced preferences | Translate modifiers, clipboard sync, paste dropped files, capture shortcuts — each as **one** setting that follows the account. The review's split into viewer/host halves was **declined by the owner** ("it's just another setting"); the host side still needs the live Control permission (§3.8) |
| D6 | Restore | Sign-out, account switch and closing the app **disconnect every tab**. On the next start the account signs in automatically if "Keep me signed in" was chosen; tabs come back disconnected with **Connect all** |
| D7 | Host passwords | **Typed once per host, remembered by default**, per account workspace, so Connect all needs no prompts (decided). Whether they also **sync between computers** (end-to-end encrypted, P8) is **deferred**: P0–P7 are finished, built, released and verified first, then the owner decides on P8 |

## 8. Risks

| Risk | Mitigation |
|---|---|
| P2 is a deep refactor of code that just stabilised (D10–D12, V2/V3) | Ship P2 with **no UI change**, behind the full loopback suite + the new two-host test; keep V3's `RateFilter` per transport untouched |
| Rust and JS merges drift → devices flip-flop | Shared test vectors run in both CI legs |
| New client + old server silently drops tabs | `features` handshake; deploy server first |
| Clock skew loses LWW races | `serverTime` correction |
| Shortcut carve-outs annoy users who need those chords on the remote | Only two chords are taken per OS; make them configurable later if anyone asks |
| The config split + migration loses a user's names or recents on upgrade | One-time migration in `normalize`, tested on a captured 2.1.x file; the old top-level fields are read but never deleted until a later release |
| Two accounts' data mixed on one computer | Every read/write goes through the active workspace; acceptance test switches A ⇄ B |
| P8 vault: a server or token leak exposes host passwords | **Not achievable with today's login** (review M19): sign-in sends the account password to the Worker (`server/src/accounts.js:357-366`), so a vault key derived from it is derivable by the server. P8 first needs a changed login (client-side key derivation splitting the password into an auth key and a vault key, or OPAQUE) and a password-change endpoint |
| P8 vault: a forgotten account password | The vault is unrecoverable by design; the UI says so before the user turns sync on |
| macOS/Linux behaviour of several surfaces in **one** window is research-verified, not device-verified | P3 device checklist on every OS before P4 starts |

## 9. Prior art

- RustDesk gives each tab its own session object and has an open request for local
  tab-switch shortcuts because Ctrl+Tab is forwarded in fullscreen
  ([discussion #15344](https://github.com/rustdesk/rustdesk/discussions/15344),
  [UI architecture](https://deepwiki.com/rustdesk/rustdesk/4-user-interface)); its users also
  asked for tear-off tabs ([PR #16962](https://github.com/rustdesk/rustdesk/pull/16962)) —
  D14 pop-out windows already cover part of that.
- Cloudflare Durable Object value limit (2 MB documented for SQLite-backed objects):
  [limits](https://github.com/cloudflare/cloudflare-docs/blob/production/src/content/docs/durable-objects/platform/limits.mdx).
