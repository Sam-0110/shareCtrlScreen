# 08a — Multi-session tabs: structure, contracts and flows

Companion to [08-MULTI-SESSION-TABS.md](08-MULTI-SESSION-TABS.md) (the *what* and *why*, the
owner's decisions D1–D7, the phases). This document is the *how*: where every piece lives,
the types and contracts between layers, the state machines, the invariants, and every flow
step by step. It is **step S** of the plan. Paths are relative to `native/` unless they start
with `server/` or `plans/`.

**Revision 2 (2026-11-18)** folds in the adversarial review (verdict NEEDS-REWORK, 2 critical,
19 major, 15 minor findings; every one verified against the code by the orchestrator). §10
maps each finding to where it is fixed.

## 1. Layering and ownership

```
 Web UI (app/ui)          renders snapshots, sends intents; holds NO truth
   │  invoke(command)                      ▲ listen(event)
 App layer (app/src)      TabManager · AccountManager · SurfaceFactory · EventBridge · Menus
   │  Rust calls                           ▲ UiEvent channel
 Engine (crates/engine)   SessionRegistry · ViewerSession · HostSession · ActiveGate ·
                          InputRouter · WorkspaceStore (config, behind the engine as today)
   │                                       ▲
 Transport / signaling / platform crates
```

| Owner | Owns (single source of truth) | Never owns |
|---|---|---|
| **Engine** | Live sessions (registry); the **connection gate** (hosting, approval pending, viewer sessions — D1); the config file and its workspaces; saved host passwords; the **ActiveGate** (which surface is shown, gets input and the clipboard) | Tab order/UI state (it stores tabs but does not interpret them) |
| **TabManager** (app) | The open tabs of the active workspace as the user sees them; tab ↔ session mapping; the active tab; the workspace **epoch** | Network sessions; config I/O (goes through the engine) |
| **AccountManager** (app) | Accounts known on this computer, the active account, tokens (through the secret store), sync scheduling and the clock skew | Workspace contents (asks the engine to switch/adopt/merge) |
| **SurfaceFactory** (app) | Creating and **destroying** native video surfaces, always on the UI thread (shared by tabs and D14 pop-outs) | When a surface is shown (the ActiveGate decides) |
| **EventBridge** (app) | Translating `UiEvent`s into Tauri events and TabManager calls, in order | Any decision |
| **Web UI** | Local view state only ("rename box open", "dropdown open") | Everything else |

Rules:
- **Every user-state read or write goes through `Engine::with_workspace(|ws| ..)`**, which
  resolves the active workspace. There is no other path to recents, names, tabs, prefs or saved
  logins.
- **Only the ActiveGate calls `VideoSurface::show()` / `hide()`** — for main surfaces, for
  pop-outs, and for overlays (§6.18). The driver and media threads ask it; they never call a
  surface directly.

## 2. Code layout (new and changed files)

### 2.1 `crates/protocol`

```
src/config/mod.rs        Config (device + accounts + workspaces), load/persist (moved from config.rs)
src/config/workspace.rs  Workspace, TabRecord, Prefs (booleans only), SavedLoginRef
src/config/migrate.rs    pure v1 → v2 transform (no I/O, no secret store); fixtures in testdata/config/
src/sync/mod.rs          SyncDoc v2 (entries + tabs + prefs), merge, normalize, prune  (moved from sync.rs)
src/sync/tabs.rs         SyncTab merge rules
src/sync/prefs.rs        SyncPref merge rules + the whitelist
src/sync/order.rs        fractional index keys (between(a,b) → key; charset [0-9a-z], ≤ 24 chars)
src/sync/clock.rs        Stamper: monotonic, skew-corrected stamps; clamp() for loaded/merged stamps
src/signaling.rs         ConnectResponse gains host `caps` (vpause) — caps flow host→viewer too
src/control.rs           + VPause / VResume
testdata/sync-vectors.json   merge cases shared with the server (§7.12)
```

`Config` gains `config_version: u32` (absent = 1) and a `#[serde(flatten)] extra` map so a
downgraded binary that rewrites the file cannot silently drop fields it does not know (§6.13).

### 2.2 `crates/engine`

```
src/lib.rs                Engine facade (thin): constructs subsystems, public API delegates
src/events.rs             UiEvent { App(..) | Session{..} }
src/gate.rs               connection gate: hosting / approval pending / viewer sessions (D1)
src/session/registry.rs   SessionRegistry: host slot, viewers map, peer index, id reservation
src/session/viewer.rs     ViewerSession: per-session state that is global today
                          (outbound, pending_nonce use, auto_attempted, negotiated, peer_os, phase)
src/session/active.rs     ActiveGate: decides the active session + overlay state under a lock,
                          then applies surface ops in one UI-thread closure (§6.4)
src/session/input_router.rs  InputRouter: per-session key path filter for tab chords + the
                          app-lifetime shortcut hook's single target
src/workspace.rs          WorkspaceStore: active-workspace resolution, switch (with epoch),
                          adopt, merge, saved logins, token migration (needs the secret store)
src/session/mod.rs        start_viewer / begin_host / teardown — per session
src/session/driver.rs     per-session death path (no global role writes; asks the gate to show)
src/session/clipboard.rs  re-baselines on activation (§6.4 step 4)
src/media/viewer.rs       no self-show(); drop-without-decode while inactive (old hosts)
src/media/host.rs         honours VPause/VResume for slot 0
```

Signaling: `app/src/main.rs`'s one `while … handle_signaling(event).await` loop stays the
**dispatcher** only. Per-message work that can wait (secret-store opens, the host approval
decision, bad-password delay, ICE gathering, `begin_host`, `start_viewer`) runs in a task
keyed by peer, so one tab or one host prompt never stalls the others (fixes B12 fully).

### 2.3 `app`

```
src/main.rs        setup + plugin wiring only
src/commands.rs    every #[tauri::command] (moved out of main.rs)
src/tabs.rs        TabManager
src/accounts.rs    AccountManager (grows out of account.rs)
src/surfaces.rs    SurfaceFactory (extracted from popout.rs::make_surface) incl. destroy()
src/bridge.rs      EventBridge (extracted from main.rs:908-972)
src/popout.rs      keyed by (SessionId, slot)
src/menu.rs        native menus (Session, Monitor, Tabs, key equivalents) — from main.rs
```

### 2.4 `app/ui`

`app.js` (≈ 1,200 lines, one global `state`) is split into ES modules loaded with
`<script type="module">`: `state.js`, `api.js` (every `invoke`/`listen`), `connect.js`
(new-tab page, recents, names), `tabs.js` (strip, tab body, rename, drag), `accounts.js`
(switcher, sign-in, bring-in prompt), `settings.js`, `host.js` (host screens, unchanged),
`overlay.js` (§6.18). **This split is its own step (P0) with no behaviour change**, because P1
(switcher) and P4 (tabs) both rewrite the UI.

As built in P0: the entry module is `main.js` (attaches every listener, in one order, after all
modules have loaded, then boots), shared mutable state is the exported object `S` in `state.js`
(an import binding is read-only), and the single live session (role changes, the viewer's menu
bar, monitor pickers and pop-outs, the link-lost panel) is in `session.js`, which P4 replaces
with per-tab views in `tabs.js`. `host.js` holds the approval modal and the host controls.

### 2.5 `server`

```
server/src/sync.js             pure normalizeDoc / mergeDocs / emptyDoc for v2 (no cloudflare:* imports)
server/test/sync-vectors.mjs   runs ../../native/crates/protocol/testdata/sync-vectors.json
.github/workflows/native-ci.yml   + a Node job: sync vectors + accounts smoke (wrangler dev)
.github/workflows/deploy-server.yml  deploy job depends on the same tests passing
```

## 3. Core types

```rust
// ---- protocol::config ---------------------------------------------------------
pub struct Config {
    pub config_version: u32,                       // 2
    // device (never synced, never per-account)
    pub uuid: String, pub server_url: String, pub mode: Mode,
    pub password_hash: Option<String>, pub password_permission: Permission,
    pub ice_servers: Vec<IceServer>, pub share_audio: bool,
    pub share_display_id: Option<String>, pub start_with_windows: bool, pub use_openh264: bool,
    // accounts
    pub accounts: Vec<AccountRef>,
    pub active_account: Option<AccountKey>,        // None ⇒ the "local" workspace
    pub workspaces: BTreeMap<WorkspaceKey, Workspace>,
    #[serde(flatten)] pub extra: serde_json::Map<String, Value>,   // preserve unknown fields
}
pub struct AccountRef {
    pub server: String, pub username: String,
    pub keep_signed_in: bool,
    pub token_ref: Option<SecretRef>,              // handle from SecretStore::seal; never the token
    pub last_sync: u64,
}
pub type AccountKey = String;   // "<server>|<username>", server lower-cased, no trailing '/'
pub type WorkspaceKey = String; // "local" | AccountKey
pub struct SecretRef(pub String);                  // opaque SecretStore handle (DPAPI blob / key id)

pub struct Workspace {
    pub recent_ids: Vec<String>, pub remote_names: BTreeMap<String, String>,
    pub id_stamps: BTreeMap<String, IdStamp>, pub recents_cleared_at: u64,
    pub tabs: BTreeMap<RemoteId, TabRecord>,       // keyed by remote: one tab per remote
    pub active_tab: Option<RemoteId>,              // per computer, never synced
    pub prefs: Prefs, pub pref_stamps: BTreeMap<PrefKey, u64>,
    pub saved_logins: BTreeMap<RemoteId, SecretRef>,
}
pub struct TabRecord {
    pub order: String, pub order_at: u64,          // fractional index
    pub opened_at: u64, pub closed_at: u64,        // open ⇔ opened_at > closed_at
}
pub struct Prefs { pub translate_modifiers: bool, pub capture_shortcuts: bool,
                   pub clipboard_sync: bool,       // one setting, both roles (D5, owner)
                   pub paste_dropped_files: bool } // booleans only (§7.13)
```

**Tab identity is the remote ID**, and **the tab's name is the remote's name**
(`remote_names[remote]`, ≤ 40 chars, today's cleaning rules). Renaming a tab renames that
remote everywhere it appears (recents chips, connect box, tab strip): one name per remote,
one stamp, no second copy that can disagree. A remote with no name shows its ID.

**"Open" and "connected" are different words.** *Open*: the record says `opened_at >
closed_at` — the tab is in the strip. *Connected*: the tab has a non-Ended session. Sync rules
talk about *open*; protection from remote closes talks about *connected*.

```rust
// ---- protocol::sync::clock ------------------------------------------------
pub struct Stamper { skew_ms: i64, last: u64 }
impl Stamper {
    pub fn now(&mut self) -> u64;                  // max(last + 1, wall + skew); never goes back
    pub fn set_skew(&mut self, server_time: u64);  // from each sync reply
    pub fn clamp(&self, t: u64) -> u64;            // min(t, wall + skew + 5 min): applied to every
                                                   // stamp on load and on merge (Rust and JS)
}
```

```rust
// ---- engine ---------------------------------------------------------------
pub struct SessionId(pub u64);                     // monotonic, process-local, never persisted

pub enum UiEvent {
    App(AppEvent),
    Session { id: SessionId, peer: String, ev: SessionEvent },
}
pub enum AppEvent { ServerStatus(..), IncomingRequest{..}, HostRole(..), Toast(String),
                    TabShortcut(TabStep) }         // TabStep = Next | Prev
pub enum SessionEvent {
    Phase(SessionPhase), Permission(Permission), LinkTrouble { secs_left: u32 },
    LinkRestored, Displays(DisplaysInfo), Toast(String), HostCaps(HostCaps),
    Released,                                      // threads joined, decoder freed: surface may go
}
pub enum SessionPhase {
    Requesting,                                    // connect-request sent (30 s timeout)
    PasswordRequired { reason: PwReason },         // First | SavedRejected | WrongPassword
    Negotiating, Live, Ended(EndReason),
}
pub enum EndReason { UserClosed, RemoteEnded, Rejected(String), Busy, Offline, Timeout,
                     LinkLost, Failed(String), AppClosing, AccountSwitch, SelfId }

impl Engine {
    pub fn reserve_session(&self) -> SessionId;                         // §6.2 step 2
    pub fn connect(&self, id: SessionId, peer: &str, surface: Arc<dyn VideoSurface>)
        -> Result<(), ConnectError>;
    pub fn disconnect(&self, id: SessionId, reason: EndReason);         // cancels Requesting too
    pub async fn disconnect_all(&self, reason: EndReason);              // resolves after every Released
    pub fn submit_password(&self, id: SessionId, pw: String, remember: bool);
    pub fn set_active(&self, id: Option<SessionId>);                    // ActiveGate
    pub fn set_overlay(&self, open: bool);                              // ActiveGate (§6.18)
    pub fn request_refresh(&self, id: SessionId);
    pub fn select_display(&self, id: SessionId, display: &str);
    pub fn popout_display(&self, id: SessionId, display: &str);
    pub fn with_workspace<R>(&self, f: impl FnOnce(&mut Workspace) -> R) -> R;  // persists after
    pub async fn switch_workspace(&self, to: WorkspaceKey) -> Epoch;    // §6.11, one operation
    pub async fn adopt_local_into(&self, to: WorkspaceKey);             // sign-up
    pub async fn merge_local_into(&self, to: WorkspaceKey);             // sign-in "Add"
}
pub enum ConnectError { AlreadyConnected(SessionId), Hosting /* incl. approval pending */,
                        SelfId, InvalidId, Switching }
```

## 4. Contracts: Tauri commands and events

### 4.1 Commands (UI → app). All return `Result<_, String>`.

| Command | Args | Effect | Idempotent |
|---|---|---|---|
| `tab_open` | `remoteId, confirmed` | Not open → create + activate + connect. Open but disconnected → activate + connect. Connected → activate | yes |
| `tab_connect` | `remoteId, confirmed` | Connect a disconnected tab | yes |
| `tab_disconnect` | `remoteId` | End/cancel its session (also from Requesting/PasswordRequired); the tab stays | yes |
| `tab_close` | `remoteId` | Disconnect + stamp `closed_at` | yes |
| `tab_activate` | `remoteId` | §6.4 | yes |
| `tab_next` / `tab_prev` | — | Activate neighbour by `order`, wrapping | — |
| `tab_rename` | `remoteId, name` | Renames the **remote** (`set_remote_name`); empty clears it | yes |
| `tab_move` | `remoteId, before?` | New fractional key between neighbours | yes |
| `tab_connect_all` | `confirmed` | §6.7 | yes |
| `tab_password` | `remoteId, password, remember` | Answer that tab's prompt | — |
| `tab_forget_password` | `remoteId` | Delete that host's saved login in this workspace | yes |
| `ui_overlay` | `open` | A web overlay opened/closed (§6.18) | yes (ref-counted by id) |
| `get_snapshot` | — | Current `TabsSnapshot` (UI boot / reload) | yes |
| `account_sign_in` | `username, password, keepSignedIn` | §6.10 | — |
| `account_sign_up` | `username, password, keepSignedIn` | §6.9 | — |
| `account_resolve_local` | `"add" \| "separate"` | Answers the bring-in prompt | yes |
| `account_switch` | `accountKey \| "local"` | §6.11 | yes |
| `account_sign_out` | — | §6.11 | yes |
| `account_remove` | `accountKey` | §6.11 | yes |
| host-side commands | unchanged | `approve`, `set_permission`, `end_session` (host only) | — |

`confirmed` exists on every command that can start a connection: when it is `false` and the
result would pass 8 connected tabs, the command returns `NeedsConfirm{wouldBe}` and does
nothing; the UI asks and calls again with `true`.

### 4.2 Events (app → UI)

| Event | Payload | When |
|---|---|---|
| `tabs-changed` | `TabsSnapshot` (full) | After **every** tab/session/account change |
| `tab-toast` | `{remoteId, message}` | Per-session toasts |
| `tab-displays` | `{remoteId, displays, current, canSwitch, popouts}` | Monitor list of that tab |
| `account-bring-in` | `{account, tabs: n, names: n}` | §6.10 |
| `account-state` | `{state, account?, error?}` | AccountManager state changes (§5.3) |
| `server-status`, `approval-request`, `role-changed` (host only), `toast` (app) | unchanged | host side and app-level |

`TabsSnapshot = { tabs: [TabView], active: remoteId?, epoch, hosting: bool, connected: n,
account: AccountView }`, `TabView = { remoteId, name, order, phase, session?, permission?,
autoSignIn, troubleSecs?, lastEnd? }`. The UI renders **only** from snapshots; it never infers
state from a command's return value.

## 5. State machines

### 5.1 Tab (app, per open remote)

```
            tab_connect / tab_open
 Disconnected ───────────────────────▶ Connecting ──Ended──▶ Disconnected
     ▲                                   │ Phase(PasswordRequired)
     │                                   ▼
     │◀──────────Ended──────────── PasswordRequired ──tab_password──▶ Connecting
     │                                   │ Phase(Live)
     │◀──────────Ended────────────────── Live ◀──LinkRestored── Trouble
                                         └──LinkTrouble─────────────▲
 tab_close (any state) → disconnect, then removed from the open list
```

- A transition applies only if the event's `SessionId` is the tab's **current** session (I4).
- `lastEnd` keeps the last `EndReason` for the tab body's one sentence ("The other computer is
  busy", "No answer from the other computer", "This is this computer's own ID").

### 5.2 ViewerSession (engine)

```
reserve → connect() → Requesting ─accepted─▶ Negotiating ─▶ Live
                        │   ▲                    │            │
                        ▼   │ fresh request      │ failure    │ link dies / remote end
                 PasswordRequired                ▼            ▼
                        └──────────────▶ Ended(reason) ──join threads, free decoder──▶ Released
```

`Requesting` times out after 30 s (`Timeout`). `Ended` is emitted at once (the UI updates);
`Released` follows after the threads are joined, and only then may the surface be destroyed.

### 5.3 Account (app)

```
LocalOnly ──sign_up──▶ SignedIn(acct)                         (local adopted, no disconnect)
LocalOnly ──sign_in──▶ ResolvingLocal ──choice──▶ SignedIn(acct)   (connects blocked meanwhile)
SignedIn(a) ──switch(b)──▶ SignedIn(b) | Locked(b)
SignedIn(a) ──sign_out──▶ LocalOnly                           (token deleted)
SignedIn(a) ──401 on sync──▶ Locked(a)
Launch: active account kept + token opens ──▶ SignedIn(a);  else ──▶ Locked(a)
Locked(a) ──password──▶ SignedIn(a);   Locked(a) ──"use locally"──▶ LocalOnly
```

**`Locked(a)` shows only a sign-in form for *a*, never *a*'s workspace.** An account's tabs,
names and saved host passwords are reachable only while that account is signed in (M16). The
local workspace is always reachable.

As built in P1 (engine `workspace.rs`, app `accounts.rs`):
- The engine's selection is `WorkspaceSel::{Local, Account(key), Locked(key)}`; `resolving` is
  the AccountManager's (it holds new connections with `hold_connects` while it asks).
- `with_workspace(|ws, stamper| ..) -> Option<R>`: `None` while Locked, so a signed-out account's
  data cannot be read or written by any path (I13); the closure takes its stamps from the
  engine's `Stamper`.
- `switch_workspace(to, end_viewer)`: `end_viewer = false` only where the same data changes
  owner (sign-up adopting local, "Add", launch); every other switch ends the viewer session.
- `ConnectError` gains `SignedOut` (connect while Locked) next to `Hosting` and `Switching`.
- If the app quits while the bring-in question is open, the account stays listed (its token
  kept if asked) but the computer starts on the local workspace; the account is one switch
  away. Nothing is merged without an answer.
- Sync scheduling runs in Rust (the AccountManager's loop: workspace change + 1.5 s, 5 min,
  window focus after 60 s, sign-in/switch, "Sync now"); the UI only reacts to
  `account-state` and `workspace-changed`.

## 6. Flows

Notation: **[UI]** web view, **[TM]** TabManager, **[AM]** AccountManager, **[E]** engine,
**[UI-T]** the Tauri main thread, **[H]** the host machine. Every flow ends with **[TM]**
emitting `tabs-changed` unless it says otherwise.

### 6.1 Open a tab from the connect box
1. [UI] `tab_open(id, false)`.
2. [TM] `id == own uuid` → error "That's this computer's ID". Not a UUID → error.
3. [TM] Open and connected → `tab_activate(id)`, done. Open and disconnected → activate, then
   §6.2. Not open → `E.with_workspace`: `tabs[id] = {order: after last, opened_at: stamp}`,
   `active_tab = id`, then §6.2.

### 6.2 Connect a tab
1. [TM] Guards: workspace switch in progress → refuse; would pass 8 connected and not
   `confirmed` → `NeedsConfirm`.
2. [TM] `s = E.reserve_session()`; record `tab(id).session = s`, phase Connecting — **before**
   anything can emit an event for `s` (fixes the first-event race).
3. [TM]→[UI-T] `SurfaceFactory::create(main window)` → `surface` (hidden, with the tab-bar
   inset).
4. [E] `E.connect(s, id, surface)`; the gate refuses with `Hosting` while this computer is
   hosting **or an approval is pending**; refuses `SelfId`. On refusal [TM] destroys the surface
   and shows the reason.
5. [E] registry inserts `ViewerSession{s, phase: Requesting}`, indexes `peer → s`, sends
   `connect-request` (with caps), starts the 30 s timer.
6. [H] response: accepted → step 7; password → §6.3; rejected/busy/offline → `Ended(reason)`.
   The response carries the host's caps (vpause) → `HostCaps`.
7. [E] on the offer, in a task keyed by peer: `bind_and_gather`, build the Rtc, spawn the
   session's threads; Negotiating → Live. The gate re-checks D1: if this computer became a host
   in the meantime, it sends `end-session` and ends `s` with `Busy`.
8. [E] media thread: first frame decoded → asks the ActiveGate; it shows the surface only if `s`
   is active.

### 6.3 Host password (typed once per host)
1. [E] `password-required` for `s`: a saved login for that host in the **signed-in** workspace,
   not yet tried for `s` → open it in a task (secret store, ≤ 60 s) and answer; no UI.
2. Else `Phase(PasswordRequired{First})` → the prompt renders **inside that tab**, "Remember"
   checked. Other tabs are untouched.
3. [UI] `tab_password(id, pw, remember)` → [E] `submit_password(s, ..)`.
4. Accepted + remember → [E] `seal` → store the `SecretRef` in the workspace's `saved_logins`.
5. **Rejected** arrives as the terminal `connect-response {accepted:false, reason:"bad-password"}`
   (`lib.rs:715-728`); the nonce is gone, so the session cannot simply re-prompt:
   - after an automatic answer → forget that saved login, send a **fresh `connect-request`** on
     the same `s` (new challenge), then `PasswordRequired{SavedRejected}` ("The saved password
     no longer works");
   - after a typed answer → the same fresh request, then `PasswordRequired{WrongPassword}`.
   The host's own lockout bounds retries.
6. `tab_disconnect` / `tab_close` while PasswordRequired → `end-session` to the host, `Ended`.

### 6.4 Switch tab (`tab_activate`, `tab_next`, `tab_prev`, click)
1. [TM] `active_tab = id` in the workspace (per computer, not synced).
2. [TM] `E.set_active(Some(s))` if the tab is connected, else `E.set_active(None)` (the UI shows
   that tab's card).
3. [E] ActiveGate, **under its lock**: record the new active session; set the clipboard gate
   off for the old and on for the new (if `clipboard_sync`); set `InputRouter.target`; queue
   `VPause` to the old host and `VResume` (or `KeyframeRequest` without vpause) to the new.
   **Drop the lock.** Then one `run_on_main_thread` closure: hide old → show new (unless an
   overlay is open) → focus (`SetFocus` / `makeFirstResponder` / X11 grab re-evaluated) →
   repaint the held frame. Hide always precedes show (X11 `XdndProxy`, XI2 grab).
4. [E] the new session's clipboard loop **re-baselines** (`last_count = change_count()`,
   `last_text = current text`) before it starts sending, so text copied from another host is
   never forwarded. Only copies made while this tab is active are sent.

### 6.5 Disconnect / close a tab
1. [TM] `E.disconnect(s, UserClosed)`. From Requesting/PasswordRequired this also sends
   `end-session` so an approve-mode host does not keep a dead session.
2. [E] emit `Phase(Ended)` (UI updates now); if `s` was active, ActiveGate → none; stop and
   join the threads **on a worker**; release the decoder; clear `peer → s`; emit `Released`.
3. [TM] on `Released` → [UI-T] `SurfaceFactory::destroy(surface)` (Windows `DestroyWindow` +
   `WinState` removal, explicit — not `Drop`; macOS `removeFromSuperview`; X11/Wayland
   teardown). Pop-outs of `s` are destroyed first.
4. Close only: stamp `closed_at`; activate the next tab by order (or none).

### 6.6 Session dies on its own
1. [E] the driver of `s` detects death → `Phase(Ended(LinkLost|RemoteEnded|Offline))`. It never
   writes app-level state or touches other sessions.
2. As §6.5 steps 2–3 (no `closed_at`). The tab body offers Reconnect.

### 6.7 Connect all, and the > 8 confirm
1. [UI] `tab_connect_all(false)` → [TM] `pending` = disconnected open tabs in order;
   `connected + pending > 8` → `NeedsConfirm{wouldBe}`; the UI asks; Continue → `(true)`.
2. [TM] capture the current **epoch**; schedule one §6.2 start every 500 ms. Each timer checks
   the epoch first and does nothing if it changed (account switch, sign-out, app close).
3. Hosts with saved logins sign in silently (§6.3), each in its own task.

### 6.8 App launch
1. [E] load config (§6.13 on first run of the new version).
2. [AM] `active_account` set, `keep_signed_in`, token opens → SignedIn, background sync. Token
   missing or not kept → Locked (sign-in form for that account; "Use without an account" →
   LocalOnly).
3. [TM] snapshot of the reachable workspace: every tab **Disconnected**, `active_tab`
   selected (not connected). **Nothing auto-connects.**

### 6.9 Sign up (adopt the local workspace)
1. [UI] `account_sign_up(user, pw, keep)` → [AM] POST signup → token.
2. [E] `adopt_local_into(key)`: move `workspaces["local"]` → `workspaces[key]` (the
   `SecretRef` handles move with it; the secret store is not touched); `"local"` = empty.
   **Connected tabs stay connected** — same tabs, same sessions.
3. [AM] `AccountRef{keep, token_ref: seal(token) if keep}`; `active_account = key`; sync uploads
   the workspace.

### 6.10 Sign in to an existing account
1. [AM] POST login → token. Local workspace empty → step 4.
2. Local workspace has open tabs or names → `account-bring-in{tabs, names}`; state
   ResolvingLocal; **new connections are refused** until it resolves. Existing connected local
   tabs are disconnected only if the user chooses "Keep separate" (they are not part of the
   account).
3. [UI] `account_resolve_local`:
   - `"add"` → [E] `merge_local_into(key)`: tabs and names union with the sync rules (a remote
     open in both stays one tab; the newer name wins); **the account's prefs win** (local prefs
     are not merged); saved logins: the account's wins per host, else the local one is adopted;
     `"local"` becomes empty. Connected local tabs stay connected.
   - `"separate"` → `disconnect_all(AccountSwitch)`; `"local"` untouched.
   - Dismissed or the app quits mid-prompt → treated as "separate"; the prompt does not return.
4. [AM] `active_account = key`; token sealed if kept; sync (pull + push).

### 6.11 Switch account / sign out / remove
1. With connected tabs the UI confirms first ("This disconnects your 3 open sessions").
2. [E] `switch_workspace(target)` is **one** operation: block new connects (`ConnectError::
   Switching`), `disconnect_all(AccountSwitch).await`, swap, bump the epoch, unblock.
3. Sign out → `forget(token_ref)`; `active_account = None`. Remove → also delete
   `workspaces[key]` and `forget` every `SecretRef` in it.
4. Target account not signed in → Locked (§5.3) until its password is entered.

### 6.12 Sync cycle
1. Triggers: launch, sign-in/switch, 1.5 s after any workspace change, window focus (> 60 s since
   the last), every 5 min.
2. [AM] build the `SyncDoc` from the active workspace. Write `tabs`/`prefs` only if the last reply
   advertised them; the first sync after launch sends v1 fields and immediately re-syncs once
   the reply advertises them.
3. Stamps come from the `Stamper` (`max(last + 1, now + skew)`); every stamp loaded or merged is
   clamped to `now + skew + 5 min` — in Rust and in the server's JS (stops a fast clock from
   reopening closed tabs forever).
4. POST `/api/sync {doc}` → `{doc, features, serverTime}`; `set_skew(serverTime)`.
5. Apply only if the account and token are unchanged since step 2. **A remote close of a tab
   that is connected here is applied when it disconnects**, not while it is connected; nothing
   is re-stamped. A remote close of a disconnected tab removes it from the strip at once.
6. 401 → Locked; network error → next trigger.
7. Preferences are stamped **only when their value changes** (compare before stamping), never
   on every Save.

### 6.13 Migration from today's config (`config_version` absent)
1. [E] after a successful parse, `migrate::v1_to_v2` (pure) builds the v2 struct in memory:
   - target workspace = today's `account` (`server|username`) if it has a token, else `"local"`;
   - move `recent_ids`, `remote_names`, `id_stamps`, `recents_cleared_at`, `saved_logins`;
   - `auto_login == false` → the saved logins are **forgotten**, not moved (the user had turned
     automatic sign-in off; retiring the toggle must not start using them);
   - `translate_modifiers`, `clipboard_sync`, `paste_dropped_files`, `capture_shortcuts` →
     workspace prefs, unchanged values.
2. [E] write a backup `config.v1.json` (once), then persist v2 atomically.
3. [E] **token**, in a task: `seal(token)` → `token_ref` → only then remove the plaintext token
   from the file. `seal` fails (e.g. Linux without a keyring) → keep it in memory for this run,
   `keep_signed_in = false`, toast "Sign in again next time: this computer has no keyring".
4. **Any migration error** → keep running on the parsed v1 values in memory, **do not persist**,
   log it. The machine `uuid` is **never** regenerated when the old file has one.
5. Downgrade: an older binary keeps unknown fields via `extra`; a re-upgrade finds
   `config_version = 2` and skips migration.

### 6.14 Background pause / resume
1. [H] advertises `vpause` in the connect-response caps → the viewer learns it per session.
2. `VPause` → the host stops capture+encode for slot 0; ctl, cursor, heartbeat continue;
   pop-out slots keep streaming. `VResume` → restart + forced keyframe.
3. Host without `vpause` → the inactive session drops received frames without decoding;
   activation sends `KeyframeRequest`.

### 6.15 Incoming connection while tabs exist (D1)
1. [E] gate: any viewer session not Released → reply `busy`. An approval pending or a host
   session → `connect` returns `Hosting`.
2. The gate is one lock, so a host accept and a viewer start cannot both pass.

### 6.16 Pop-outs
1. Offered only on the active tab. Registry key `(SessionId, slot)`, label
   `popout-<session>-<slot>-<n>`.
2. Pop-outs stay visible when their tab goes to the background; they close with their session
   (§6.5 step 3).
3. Show/hide (including link-restore) goes through the ActiveGate.
4. Mouse input in a pop-out goes to its own surface's sink (as D14 today). The **keyboard
   shortcut hook and the clipboard** follow the active tab of the **main window** only.

### 6.17 App close
1. [UI-T] close requested → start `disconnect_all(AppClosing)` **on a worker**; the window stays
   until it resolves (≤ 3 s, then exit regardless); persist; exit.
2. Windows service host (W10): closing hides to the tray as today; viewer tabs disconnect.

### 6.18 Web overlays over the native video
Settings, confirms, the bring-in prompt, the account-switcher dropdown and tab context menus
are web UI and would render **behind** the native video (airspace).
1. [UI] `overlay.js` calls `ui_overlay(true)` when any overlay opens and `(false)` when the last
   one closes (ref-counted).
2. [E] ActiveGate hides the active surface while an overlay is open, then shows and repaints it.
3. The **+** opens the connect page as a normal page (active = none), not a popover.

### 6.19 Tab shortcuts (D3)
1. Chords: Windows/Linux `Ctrl+Alt+PageDown` / `PageUp`; macOS `Cmd+Option+PageDown` /
   `PageUp` (Fn+↓/↑ on laptops).
2. [E] InputRouter filters them out of **every** session's key path, **always** — whether or
   not shortcut capture is on — and emits `AppEvent::TabShortcut(Next|Prev)`; they are never
   forwarded.
3. When the web view has focus (a disconnected tab's card, the connect page), `app/ui` handles
   the same chords on keydown; on macOS they are also the native Tabs menu's key equivalents.

## 7. Cross-cutting rules

1. **Threads.** Surfaces are created and destroyed only on the UI thread. Engine calls never
   block the UI thread or the signaling dispatcher; ICE, secret-store opens, approvals, joins run
   in tasks/workers.
2. **Locks.** Order: `gate` → `registry` → `session` → `workspace`. **No UI-thread call, no
   app callback and no event send while holding any of them**; decide under the lock, act after.
3. **Ids.** `SessionId` never leaves the process except as an opaque number in the snapshot.
4. **Errors.** Every `EndReason` has one user-facing sentence.
5. **Persistence.** Every workspace write persists atomically and schedules a sync.
6. **Secrets.** Tokens and host passwords exist only behind `SecretRef`s; never in
   `config.json` (after migration step 3), logs, or any event.
7. **Old peers.** New ctl messages are capability-gated in both directions.
8. **Old servers.** v2 fields are written only after the server advertises them.
9. **Logging.** Every session log line carries `s=<id> peer=<short>`.
10. **Feature flag.** A config flag hides the tab UI (single-tab behaviour) in the P4 release.
    It covers only the UI; P1's config split and P2's engine changes are not behind it.
11. **Testing seams.** `SurfaceFactory`, the secret store and the clock are traits.
12. **Merge parity.** `sync-vectors.json` is the contract between the Rust and JS merges, run in
    CI and required before a server deploy.
13. **Prefs are booleans** (tie-break `false < true`), so Rust and JS serialization cannot
    disagree.
14. **Fractional keys**: generated keys break ties with the remote ID; keys are rebalanced when
    they exceed 24 chars; the server rejects other charsets/lengths.
15. **Pruning**: closed tabs are pruned by age (90 days) only; the server's count cap is far above
    any real workspace and rejects instead of pruning.

## 8. Invariants (asserted in debug builds and tests)

| # | Invariant |
|---|---|
| I1 | At most one non-Released `ViewerSession` per peer; at most one open tab per remote |
| I2 | At most one active session; only its main surface is shown (unless an overlay is open); it alone is the InputRouter target and the clipboard owner |
| I3 | No viewer session starts or continues while this computer hosts or has an approval pending (D1) |
| I4 | TabManager applies a `SessionEvent` only if its `SessionId` is the tab's current session |
| I5 | Every workspace read/write resolves the active workspace through one function |
| I6 | No token or host password in `config.json` (after migration), logs or any event |
| I7 | Only the ActiveGate calls `VideoSurface::show()` / `hide()` |
| I8 | A tab command on a remote that is not open is an error, never a panic |
| I9 | After `disconnect_all` **resolves**: zero sessions, zero viewer surfaces, zero pop-outs |
| I10 | A workspace swap happens only inside `switch_workspace`, with connects blocked and zero sessions |
| I11 | Stamps never go backwards on a device, and no stored stamp exceeds `now + skew + 5 min` |
| I12 | The machine `uuid` is never regenerated when a config file with one exists |
| I13 | An account's workspace is reachable only while that account is signed in |

## 9. What this changes in plan 08

- Top inset is **new layout code** on Windows, X11 and Wayland (the reveal there is a clip,
  shape or crop, not an inset); only macOS has a true inset. P3 becomes L.
- Synced settings stay as decided in D5 (`clipboard_sync` and `paste_dropped_files` follow the
  account as single settings); the review's host/viewer split was declined by the owner.
- Tab name = remote name.
- Tab shortcuts use PageUp/PageDown on every OS (owner's wording; Cmd+Option+←/→ is the Mac
  browsers' own tab switch). D3 amended.
- New early work: UI module split + `main.rs` split, a Node CI job gating server deploys.
- P1 is L; P1 and P2 overlap in `engine/src/lib.rs` (saved logins) — P1 merges first, P2 rebases.
- P8's "the server can never read a host password" needs a changed login (client-side key
  derivation), since today the account password is sent to the server.

## 10. Review findings → fixes

| Finding | Verified | Fixed in |
|---|---|---|
| C1 migration failure regenerates the machine ID | `config.rs:450-470`, `:157` | §6.13 steps 2, 4; I12 |
| C2 two "synced prefs" are host security | `session/mod.rs:329`, `:371`, `:415` | **Declined by the owner** (risk accepted: single settings that follow the account; host side still needs the Control permission). Plan 08 §3.8, D5 |
| M1 "live" overloaded; re-stamp breaks close | design | §3 open vs connected; §6.12 step 5 |
| M2 fast clock resurrects tabs; stamps go back | `accounts.js:182-185` | §3 `Stamper`; §6.12 step 3; I11 |
| M3 Save stamps every pref | `app.js` `saveSettings` sends all fields | §6.12 step 7 |
| M4 stale-password re-prompt cannot work | `lib.rs:715-728` | §6.3 step 5 |
| M5 signaling loop serial; D1 race | `main.rs:978`; `lib.rs:420`, `:439` | §2.2 dispatcher; §6.2 step 7; §6.15; I3 |
| M6 connect/mapping race | design | §6.2 step 2 (`reserve_session`) |
| M7 Connect all vs switch; I10 | design | §6.7 epoch; §6.11 one operation; §6.10 step 2 |
| M8 set_active under a lock calls the UI thread | `window.rs:544-567`; mac `on_main` | §6.4 step 3; §7.2 |
| M9 teardown thread/order; I9 | `adapters.rs:528`; mac `Drop` already removes | §5.2 `Released`; §6.5; §6.17; I9 |
| M10 top inset is not reveal reuse | `window.rs:585`, `:547` | §9; plan 08 §3.5, P3 = L |
| M11 web overlays under the video | `app.js` `set_video_visible` uses | §6.18; `ui_overlay` |
| M12 tab chords with capture off | `keyhook.rs:97-108`; capture off by default `config.rs:174` | §6.19 |
| M13 clipboard gate re-sends on activation | `clipboard.rs:60-92` | §6.4 step 4 |
| M14 secret-store model wrong | random ids in `platform-macos/src/secret.rs:38-93` | §3 `SecretRef`, `token_ref`; §6.9; §6.13 step 3 |
| M15 retiring auto_login overrides "off" | `lib.rs:578` | §6.13 step 1 |
| M16 no isolation between accounts | design | §5.3 `Locked`; I13 |
| M17 P1/P2 overlap; P1 underestimated; UI split | design | §9; plan 08 §4 |
| M18 no Node CI; deploy untested | `native-ci.yml`, `deploy-server.yml` | §2.5; plan 08 P0 |
| M19 P8 server-blind claim false | `accounts.js:357-366` | §9; plan 08 P8 + risks |
| Minor 1 host caps can't reach the viewer | `signaling.rs:136` (caps only in the request) | §2.1; §6.14 step 1 |
| Minor 2 tab name duplicates remote name | `config.rs:152` (40-char cap) | §3 tab name = remote name |
| Minor 3 confirm/phase/tab_open gaps | design | §4.1 `confirmed`; §3 `EndReason`; §6.1 |
| Minor 4 Requesting has no timeout/cancel; own ID | `server/src/index.js:177` | §5.2 timeout; §6.1 step 2; §6.5 step 1 |
| Minor 5 fractional keys | design | §7.14 |
| Minor 6 prune rules disagree | design | §7.15 |
| Minor 7 pref tie-break | design | §7.13 |
| Minor 8 downgrade drops v2 fields | design | §2.1 `extra`; §6.13 steps 2, 5 |
| Minor 9 invariants vs pop-outs | `driver.rs:405-425` | §6.16; I2, I7 |
| Minor 10 features rationale overstated | design | kept as harmless; §6.12 step 2 (second round) |
| Minor 11 sign-up disconnects needlessly | design | §6.9 step 2 |
| Minor 12 bring-in merge underspecified | design | §6.10 step 3 |
| Minor 13 decoder fallback is H.264 only | `codec-openh264/src/fallback.rs` | plan 08 §5 |
| Minor 14 Wayland inhibitor scope | design | plan 08 §3.5 (release while no connected tab is active) |
| Minor 15 feature flag scope | design | §7.10 |
