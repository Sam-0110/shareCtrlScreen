# 10a — Remote unlock: structure, contracts and flows

Companion to [10](10-REMOTE-UNLOCK.md). Plan 10 says *what* and *why*; this document is the
*how*: who owns what, where the code goes, the types and messages, the state machines, every
flow and the invariants. It is **step S**. Paths are relative to `native/`. Tags in flows:
[VU] viewer web page, [V] viewer engine, [H] host engine, [W] the host's Windows capture
wrapper and injector (`platform-windows`), [A] the agent, [S] the service.

Windows behaviour this document relies on is listed in plan 10 §8 and is not yet verified from
Microsoft's documentation. §8 below (the sign-in screen) is an outline; step S6 completes it.

## 1. Ownership

| Owner | Owns | Does not own |
|---|---|---|
| **Service** (`sharectrl-service.exe --run-service`, session 0, LocalSystem) | Which console session is active and whether a user is signed in; starting, watching and ending the host and the agent; the `SendSAS` call and the policy around it | Capture, input, the pipe to the host, any remote session |
| **Agent** (`sharectrl-service.exe --secure-agent`, the console session, LocalSystem) | The pipe (as server) and the check of who connects; capture of and input into the input desktop **while it is not `Default`**; the shared frame section; asking the service for Ctrl+Alt+Del | The network, remote sessions, permissions, the user's files, the `Default` desktop |
| **Host's Windows platform** (`crates/platform-windows`) | The pipe client and the check of who serves it; one `ScreenCapture` over two sources (the local duplication, the agent) and its D3D11 device; an `InputInjector` that forwards while the secure desktop has input; the `ScreenState` it reports | Who is connected, permissions |
| **Host engine** (`crates/engine`) | Who is connected and with which permission (unchanged); sending `hstate` when the capture's `ScreenState` changes; honouring `sas` and `lock` only with control | Anything Windows-specific |
| **Viewer engine and UI** | Hiding the video while the host is `blocked`; the message in the tab; offering the two actions when the host advertises them | — |

## 2. Code layout

```
crates/protocol/src/control.rs           HostState, SecureAttention, LockSession (§3.2)
crates/protocol/src/signaling.rs         HostCaps { sas, lock } (§3.2)
crates/protocol/src/config/mod.rs        allow_lock_screen_control (§3.7)
crates/platform-api/src/capture.rs       ScreenState, BlockReason, ScreenCapture::screen_state (§3.1)
crates/platform-api/src/platform.rs      HostFeatures, secure_attention, lock_session,
                                         set_secure_desktop_control (§3.1)
crates/platform-api/src/mock.rs          a capture that stays lost; scripted ScreenState
crates/agent-ipc/            (new)       pure: the pipe's messages, framing, limits (§3.4). No OS code,
                                         tested on every OS
crates/secure-agent/         (new)       Windows-only: the agent's runtime — pipe server loop, capture
                                         thread, input thread, the frame section (§3.5)
crates/elevation/src/agent.rs    (new)   the SYSTEM-in-session token, the job object, the launch
crates/elevation/src/pipe.rs     (new)   the named pipe: creation with its security descriptor, the
                                         client open, both peer checks
crates/elevation/src/sas.rs      (new)   SendSAS and the policy handling (U4)
crates/elevation/src/session.rs          session-change events as plain values; whether a user is signed in
crates/capture/src/lib.rs                Duplicator: device creation split from duplication (U3a)
crates/platform-windows/src/secure_desktop.rs (new)
                                         AgentClient, SecureDesktopCapture, the forwarding injector
crates/platform-windows/src/adapters.rs  DdaCapture and SendInputInjector stay; create_capture and
                                         create_injector return the wrappers
crates/engine/src/media/host.rs          sends hstate (§3.3)
crates/engine/src/session/driver.rs      host: sas, lock (control only); viewer: hstate
crates/engine/src/session/active.rs      the view's new condition: the host has a picture
crates/engine/src/events.rs              SessionEvent::HostScreen
crates/engine/src/sessions.rs            HostCaps from the platform; send_secure_attention, lock_remote
service/src/main.rs                      --secure-agent; HandlerEx; the event-driven loop
service/src/supervise.rs                 a sign-in resets the delay; a second Supervisor for the agent
app/src/commands.rs, bridge.rs, menu.rs  the two commands, the host-screen event, the menu items
app/ui/api.js, session.js, settings.js, index.html
                                         the message in the tab, the two actions, the setting
app/installer-hooks.nsh                  unchanged (the agent is the service binary)
plans/checklists/UNLOCK-U1.md … U6.md    device checklists
```

`secure-agent` depends on `capture`, `input`, `elevation` and `agent-ipc`. `platform-windows`
depends on `agent-ipc` and `elevation`. Neither depends on the other, so the service binary does
not link the host's codecs and the host does not link the agent's runtime.

## 3. Types and contracts

### 3.1 `platform-api`

```rust
/// What the host's screen is, as far as the capture backend can tell.
pub enum ScreenState {
    Normal,
    /// A secure desktop (lock, sign-in, UAC) is showing and is being captured.
    Secure,
    /// A secure desktop is showing and cannot be captured.
    Blocked(BlockReason),
}
pub enum BlockReason { NoService, Disabled, AgentUnavailable, Unknown }

pub trait ScreenCapture {
    // … as today …
    /// Default: `Normal`. Read by the host loop after every `acquire` / `reinit`.
    fn screen_state(&self) -> ScreenState { ScreenState::Normal }
}

pub struct HostFeatures { pub secure_attention: bool, pub lock_session: bool }

pub trait Platform {
    // … as today …
    fn host_features(&self) -> HostFeatures { HostFeatures::default() }      // all false
    fn secure_attention(&self) -> Result<(), HostActionError> { Err(Unsupported) }
    fn lock_session(&self) -> Result<(), HostActionError> { Err(Unsupported) }
    /// The host setting (§3.7). Default: ignored.
    fn set_secure_desktop_control(&self, _on: bool) {}
}
pub enum HostActionError { Unsupported, BlockedByPolicy, Failed(String) }
```

`CaptureError` does not change. `Lost` stays the one recoverable error
(`crates/platform-api/src/capture.rs:62-66`); `SecureDesktop` stays unused by Windows for the
reason given at `crates/platform-windows/src/adapters.rs:66-68`.

The mock gains `MockCaptureConfig::outage` (lost at frame *n*, `reinit` fails *k* times, with a
`ScreenState` to report meanwhile). Today's `lose_at` loses once and `reinit` always succeeds
(`crates/platform-api/src/mock.rs:197-202`, `:278-285`).

### 3.2 `protocol`

```rust
// control.rs
/// Host → Viewer. Sent when the ctl channel opens and on every change.
/// Older viewers cannot parse it and ignore it.
#[serde(rename = "hstate")]
HostState {
    screen: HostScreen,                       // "normal" | "secure" | "blocked"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    why: Option<String>,                      // "no-service" | "disabled" | "agent"; absent = unknown
}
/// Viewer → Host. Control only. Sent only to a host with `HostCaps::sas`.
#[serde(rename = "sas")]
SecureAttention,
/// Viewer → Host. Control only. Sent only to a host with `HostCaps::lock`.
#[serde(rename = "lock")]
LockSession,
/// Host → Viewer: how a `sas` or `lock` ended. Older viewers ignore it.
#[serde(rename = "actr")]
ActionResult {
    action: String,                           // "sas" | "lock"
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    why: Option<String>,                      // "policy" | "unsupported" | text
}

// signaling.rs
pub struct HostCaps { pub vpause: bool, pub sas: bool, pub lock: bool }   // each default false, omitted when false
```

`HostScreen` parses an unknown value as `normal`, so a later host can add states. Compatibility:

| Pair | Result |
|---|---|
| New host → old viewer | `hstate` is not a `ControlMsg` the viewer knows; it is ignored (the precedent of `curshape` and `displays`, `control.rs:83`, `:89`). The viewer behaves as today |
| Old host → new viewer | No `hstate` ever arrives: the viewer assumes `normal`. No `sas` / `lock` in `HostCaps`: the actions are not offered |
| New viewer → old host | Not sent (capability). If sent anyway, the host fails to parse it as `ControlMsg`, then as `InputMsg`, and drops it (`crates/engine/src/session/driver.rs:1042-1052`) |

No signaling-server change.

### 3.3 Engine

```rust
// events.rs
SessionEvent::HostScreen { screen: protocol::HostScreen, why: Option<String> }

// sessions.rs
impl Engine {
    pub fn send_secure_attention(&self, id: SessionId);   // viewer; no-op without HostCaps::sas
    pub fn lock_remote(&self, id: SessionId);             // viewer; no-op without HostCaps::lock
}
```

- **Host media loop** (`media/host.rs`). After `open_capture` returns, and after every `acquire`
  and `reinit`, it compares `cap.screen_state()` with the last value sent and sends `hstate` on
  `ctl_tx` when it differs (the channel `displays` already uses, `host.rs:190-195`). While
  `open_capture` waits on `Lost` (`host.rs:331-341`) it sends `blocked` once. Only the main
  stream (slot 0) reports.
- **Host driver** (`session/driver.rs`). `SecureAttention` and `LockSession` are handled next to
  `SelectDisplay`: only on the host, only while the control gate is open, otherwise ignored with
  a debug line. They call `platform.secure_attention()` / `platform.lock_session()` and answer
  with `ActionResult` (§3.2), so the viewer can say "Ctrl+Alt+Del is blocked by this computer's
  policy". Today only `ControlMsg` values cross the wire and a host-side `Toast` reaches the
  host's own UI only (`crates/engine/src/events.rs:113-114`), which is why a result message is
  needed.
- **Viewer driver.** `HostState` becomes `SessionEvent::HostScreen` and sets the view's new
  condition.
- **ActiveGate** (`session/active.rs`). A session's video is shown when it is on screen, ready,
  its link is not in trouble, no overlay is open in its window (`active.rs:5-8`) **and the host
  is not `blocked`**. `View::set_host_blocked(bool)` follows `set_trouble` (`active.rs:187`).
- **HostCaps.** The connect-response's `HostCaps { vpause: true }`
  (`crates/engine/src/sessions.rs:300`) adds `sas` and `lock` from `platform.host_features()`.

### 3.4 The pipe (`crates/agent-ipc`)

Name: `\\.\pipe\ShareCtrlScreen.SecureAgent.<session id>`. Byte mode, one instance.

Framing: `u32 LE header length`, `u32 LE tail length`, the header (JSON, UTF-8), the tail (raw
bytes, used only for cursor images). Limits: host → agent, header ≤ 4 KiB and no tail; agent →
host, header ≤ 64 KiB and tail ≤ 1 MiB. Anything over a limit, a header that does not parse, an
unknown `t`, or a message that is not valid in the current state closes the connection.

```rust
pub const VERSION: u32 = 1;

// host → agent
enum ToAgent {
    Hello { v: u32 },
    /// Capture the input desktop's picture of this output. Valid only while the input
    /// desktop is not Default; otherwise the answer is `Desktop { secure: false }`.
    Attach { output: u32 },
    Detach,
    /// The host has finished reading frame `seq`; the section may be rewritten.
    Ack { seq: u64 },
    TargetRect { rect: Option<Rect> },
    Input { msg: protocol::InputMsg },
    ReleaseAll,
    SecureAttention,
}

// agent → host
enum FromAgent {
    Hello { v: u32 },
    /// The answer to Attach: the section handle (already duplicated into the host,
    /// read-only), its size in bytes, and the picture's geometry.
    Attached { section: u64, len: u64, width: u32, height: u32, stride: u32 },
    /// Frame `seq` is in the section. `dirty` empty = everything. The tail carries a new
    /// cursor shape as Desktop Duplication delivered it (the host converts it with
    /// `pointer_shape_to_bgra`, `crates/platform-windows/src/adapters.rs:97`).
    Frame { seq: u64, dirty: Vec<Rect>, pointer_only: bool, cursor: Option<CursorNote> },
    /// The input desktop changed. `secure: false` ends an attachment.
    Desktop { secure: bool, name: String },
    /// The geometry changed (a mode change); a new Attached follows.
    Resized,
    SecureAttentionResult { ok: bool, why: Option<String> },   // why: "policy" | text
    Error { text: String },
}
```

A version mismatch in `Hello` closes the connection; the host then reports
`Blocked(AgentUnavailable)`. Host and agent come from one installer and are replaced together, so
a mismatch means a half-finished upgrade.

**Access.** The pipe's security descriptor: owner SYSTEM; SYSTEM full access; the session user's
SID read and write, without the right to create instances; an explicit medium integrity label;
`FILE_FLAG_FIRST_PIPE_INSTANCE`; `PIPE_REJECT_REMOTE_CLIENTS`. The agent takes the user's SID
from that session's own token, never from its command line.

**Peer checks.**
- Agent, on connect: the client's process id (`GetNamedPipeClientProcessId`) → a process handle
  with query and duplicate-handle rights, **kept open for the life of the connection** so the id
  cannot be reused → its image path is `sharectrl.exe` in the agent's own folder, its session is
  the agent's, its token's user is the session user. Any failure closes the connection and is
  logged once.
- Host, on connect: the server's process id (`GetNamedPipeServerProcessId`) → its image path is
  `sharectrl-service.exe` in the host's own folder and its token's user is LocalSystem. A failure
  means `Blocked(AgentUnavailable)`.

### 3.5 The agent (`crates/secure-agent`)

Threads, each created for one job and owning no windows and no hooks (so `SetThreadDesktop`
works on it):

| Thread | Job |
|---|---|
| `agent-pipe` | Accept one client, check it, read messages, route them. On disconnect: stop capture, release held keys and buttons, drop the section, accept again |
| `agent-capture` | While attached: follow the input desktop (`elevation::InputDesktopFollower`), duplicate the output, copy changed rectangles into the section, send `Frame`, wait for `Ack` |
| `agent-input` | Follow the input desktop before every event, inject with `input::Injector`, track held keys |
| `agent-control` | The inherited pipe pair to the service (§3.6); exits the process when the service's end closes |

Rules of the capture thread:
- It checks the input desktop's name on attach and whenever the duplication reports access lost.
  If the name is `Default` it sends `Desktop { secure: false }`, drops the duplication and the
  section and waits for the next `Attach`.
- Its own duplication losses (a mode change, the secure desktop being replaced by another) are
  handled inside the agent by re-duplicating; the host sees at most a `Resized`.
- One buffer, one writer: it writes the section only after the previous `Ack`. Changes that
  arrive meanwhile are merged into the next frame's rectangles.
- It never sends more than 30 frames a second.

Rules of the input thread:
- It injects only while the input desktop's name is not `Default`. An event that arrives while it
  is `Default` is dropped (the host injects those itself).
- When the input desktop changes it releases every key and button it holds, before following.

The frame section is unnamed (`CreateFileMappingW` without a name), sized `stride × height`, and
reaches the host only as a read-only handle duplicated into the checked client process.

Logging: `sharectrl-agent.log` next to the binary, for the same reason the service logs there
(`service/src/main.rs:124-127`). Never a key code, never a pointer position.

### 3.6 Service ↔ agent

At launch the service creates two anonymous pipes and passes exactly those two handles to the
agent (an explicit inherited-handle list, nothing else inherited). Line protocol, UTF-8:

```
agent → service:   sas
service → agent:   sas ok | sas policy | sas failed <text>
```

The service handles one request at a time and at most one `sas` a second. U6 adds the pre-login
profile requests here (§8).

**`SendSAS` and the policy (D3, recommended behaviour).** If the policy value that allows
services to send it is absent, the service sets it, calls `SendSAS(FALSE)`, and removes it again.
If the value is present and allows services, it just calls. If it is present and forbids, it
answers `sas policy` and changes nothing. A crash between set and remove is repaired at the next
service start (a marker value under the service's own key says "we set it").

### 3.7 Service changes

- `RegisterServiceCtrlHandlerExW`, accepting stop, shutdown and session change. The handler only
  records the event and signals the loop; all work stays on the loop's thread.
- The loop waits on that signal with a 2 s timeout instead of sleeping (`service/src/main.rs:300`),
  so the poll stays as the fallback.
- `Supervisor::user_arrived(now)`: a sign-in in the active session turns a pending `Retry` into
  `Wanted` and clears the failure count (plan 10 §2 fact 7).
- A second `Supervisor` for the agent. It wants an agent exactly when the host is wanted: a user
  is signed in at the active console session. Exit code 0 means "told to stop"; anything else is
  relaunched with the same doubling delay (`service/src/supervise.rs:28-33`).
- **Launch** (`elevation::agent`): duplicate the service's own token as a primary token, set its
  session id to the console session (SeTcbPrivilege is already enabled the same way for UIAccess,
  `crates/elevation/src/uiaccess.rs:172-209`), create the process suspended on `winsta0\default`
  with the two pipe handles, assign it to a job object that kills its processes when the job
  handle closes, resume.
- **Stop**: on service stop, on a session change away from the agent's session, and on sign-out,
  the service closes its ends of the pipe pair, waits up to 5 s for the agent to exit, then
  terminates it and waits for the handle. Only then does it report stopped (invariant K10).

### 3.8 Host's Windows platform (`secure_desktop.rs`)

```rust
pub(crate) struct AgentClient { /* pipe, reader thread, frame slot, desktop state */ }
impl AgentClient {
    fn connect() -> Result<Arc<Self>, AgentError>;      // open, check the server, Hello
    fn attach(&self, output: u32, timeout: Duration) -> Result<Attached, AgentError>;
    fn next_frame(&self, timeout: Duration) -> Result<Option<FrameNote>, AgentError>;
    fn ack(&self, seq: u64);
    fn send_input(&self, msg: &InputMsg);
    fn release_all(&self);
    fn secure_attention(&self) -> Result<(), HostActionError>;
    fn secure_desktop(&self) -> bool;                    // the last Desktop message
}

pub struct SecureDesktopCapture { device, source: Source, agent: Option<Arc<AgentClient>>, … }
enum Source { Local(Duplication), Agent { tex, attached, held: Option<u64> }, Blocked(BlockReason) }
```

One `AgentClient` per host process, created on first need and shared by the capture thread
(`sct-host-media`) and the injector's thread (`sct-transport`, where the injector is created,
`crates/engine/src/session/driver.rs:248-266`). It owns one reader thread (`sct-agent-rx`) that
turns incoming messages into: the frame slot (a mutex and a condition variable, read by the
capture thread), the desktop flag (an atomic, read by the injector), and the answers to `attach`
and `secure_attention` (one-shot slots). Writes from both threads go through one writer mutex.
Lock order: the writer mutex is a leaf; the frame slot's mutex is a leaf; neither is held while
the other is taken; neither is held across a D3D call or a UI event.

A broken pipe is not an error to the user: the client is dropped, the capture reports `Lost` and
then `Blocked(AgentUnavailable)`, and the next `reinit` tries to connect again, at most once a
second.

The injector: `SendInputInjector::dispatch` follows the desktop locally as today
(`adapters.rs:768-771`). When the follow is refused **and** the client says a secure desktop has
input, the event goes to the agent instead. `release_all` goes to both. `set_target_rect` is
remembered and forwarded. The first refused follow of an outage is logged once, with the count of
dropped events when the outage ends; never per event.

### 3.9 App and UI

| Piece | Contract |
|---|---|
| Event `host-screen` | `{ remoteId, screen, why }` from `SessionEvent::HostScreen` (`app/src/bridge.rs`) |
| Event `action-result` | `{ remoteId, action, ok, why }` |
| Command `send_secure_attention(remoteId)` | → `Engine::send_secure_attention` (the pattern of `request_refresh`, `app/src/commands.rs:266-274`, `app/ui/api.js:66`) |
| Command `lock_remote(remoteId)` | → `Engine::lock_remote` |
| Session bar (`app/ui/index.html:134-146`) | "Ctrl+Alt+Del" and "Lock", each hidden unless the session has control and the host advertises it; in detached windows too |
| Native menu (`app/src/menu.rs`) | The same two items under Session, on macOS and Linux |
| The tab's message | While `blocked`: "The remote computer is locked or showing a Windows security prompt. It cannot be shown because …" with the reason in plain words, and what to do (install on the remote with the installer; turn the setting on; wait). Uses the tab's no-picture body (`app/ui/index.html:147-149`), like the link-trouble message (`app/ui/session.js:153`) |
| Badge | While `secure`: "LOCK SCREEN" next to the peer label |
| Setting | Settings → "Allow control of the lock screen and Windows security prompts", Windows hosts only, default on. Saved as `allowLockScreenControl`; applied with `platform.set_secure_desktop_control` at start and on change (the pattern of `openh264.set_enabled`, `app/src/main.rs:210`) |

`Config` gains `allow_lock_screen_control: bool`, default `true`. The struct is
`#[serde(default)]` (`crates/protocol/src/config/mod.rs:119-121`), so an existing file loads with
the default and no migration or version bump is needed.

## 4. State machines

### 4.1 The capture wrapper's source

```
                 attach refused, setting on, agent attaches
      ┌──────────────────────────────────────────────────────────┐
      │                                                          ▼
   Local ◄──────────── local attach works ◄──────────────────  Agent
      │                                                          │
      │ attach refused and                    agent gone, error, │
      │ (no service | setting off | no agent)  or attach timeout │
      ▼                                                          │
   Blocked(reason) ◄─────────────────────────────────────────────┘
      │  every reinit: try Local, then Agent
      └────────────────────────────────────────► Local | Agent
```

- Transitions happen only inside `reinit` (and in the constructor, which is a first `reinit`).
- `acquire` returns `Lost` whenever the current source can no longer deliver: the duplication
  reports access lost or denied (`Local`), the agent says `Desktop { secure: false }` or the pipe
  breaks (`Agent`), always (`Blocked`).
- `screen_state()`: `Local` → `Normal`, `Agent` → `Secure`, `Blocked(r)` → `Blocked(r)`.
- `dimensions()`: the output's size from DXGI in `Local` and `Blocked`, the attached size in
  `Agent`.
- `device()`: the wrapper's device, the same in every state.

### 4.2 The agent's connection

```
Listening ──client connects, checks pass──► Connected ──Hello ok──► Ready
    ▲                                           │                     │ Attach, desktop is secure
    │                                           │                     ▼
    └──── disconnect / check fails / bad ───────┴──────────────── Attached
          message (from any state)                 Detach, or desktop → Default: back to Ready
```

Input and `SecureAttention` are accepted in `Ready` and `Attached`. `Attach` in `Ready` while the
desktop is `Default` is answered with `Desktop { secure: false }` and stays `Ready`.

### 4.3 The service's view of a session

```
NoUser ──a user signs in (event or poll)──► User: host Wanted, agent Wanted
User ──sign-out / session change──► NoUser: agent stopped, host left to end with its session
```

Each of host and agent then follows the existing `Supervisor` states (`Wanted`, `Running`,
`Stopped`, `Retry`, `service/src/supervise.rs:35-45`), independently.

## 5. Flows

### 5.1 The machine locks during a session

1. [W] The local duplication reports access lost; `acquire` returns `Lost`.
2. [H] The loop calls `reinit` (`media/host.rs:790-791`).
3. [W] The local attach is refused. The setting is on and the agent is connected (or connects
   now). [W] sends `Attach { output }`.
4. [A] The input desktop is not `Default`: it duplicates the output, creates the section, hands
   the host a read-only handle, answers `Attached`.
5. [W] Maps the section, creates its texture, source = `Agent`; `reinit` returns `Ok`.
6. [H] Reads `screen_state()` = `Secure`, sends `hstate secure`, forces a keyframe
   (`host.rs:818`).
7. [A] Sends `Frame` 1 with everything dirty. [W] copies it into the texture and returns it from
   `acquire`; [H] encodes; on `release` [W] sends `Ack`.
8. [V] Receives `hstate secure`; [VU] shows the badge. The picture is the lock screen.

If step 3 or 4 fails, the source is `Blocked(reason)`, `reinit` returns `Lost`, [H] sends
`hstate blocked` and retries every 250 ms as today; see 5.4.

### 5.2 The controller unlocks it

1. [VU] The controller presses a key. [V] sends the usual input messages.
2. [H] The control gate is open; the driver calls `dispatch`
   (`session/driver.rs:1044-1050`).
3. [W] The local follow is refused and the agent says the desktop is secure: `Input` goes to [A].
4. [A] The input thread follows the input desktop and injects. The lock screen shows the password
   box; the controller types; Windows signs in.
5. [A] The duplication reports access lost; the input desktop's name is now `Default`. [A]
   releases any key it still holds, drops the duplication and the section, sends
   `Desktop { secure: false }`.
6. [W] `acquire` returns `Lost`; [H] calls `reinit`; the local attach works; source = `Local`.
7. [H] Sends `hstate normal`, forces a keyframe. The session continues on the desktop.

### 5.3 Connecting to a machine that is already locked

1. [H] Password mode: the proof is checked and the session is accepted without a person
   (`pans/00-OVERVIEW-AND-PROTOCOL.md:118-130`). In approve mode nobody answers and the request
   times out; nothing in this plan changes that.
2. [H] `open_capture` calls `create_capture`. [W] creates the device, then runs its first
   `reinit`: local attach refused → agent → source = `Agent`. `create_capture` succeeds.
3. [H] Creates the encoder on the wrapper's device (`media/host.rs:422`) and continues as in 5.1
   from step 6.

If the agent cannot attach, `create_capture` still succeeds with source = `Blocked`: the loop
runs, sends `hstate blocked`, and retries. `open_capture`'s own wait (`host.rs:331-341`) then only
serves other platforms and hosts without a usable device.

### 5.4 No service, the setting off, or the agent unavailable

1. [W] The local attach is refused. No service is installed / the setting is off / the pipe
   cannot be opened or checked. Source = `Blocked(reason)`.
2. [H] Sends `hstate blocked` with `why`.
3. [V] Hides the video; [VU] shows the message with the reason.
4. [H] Keeps retrying. When the machine is unlocked locally the local attach works again and
   [H] sends `hstate normal`.

### 5.5 Ctrl+Alt+Del

1. [VU] The action is visible because the host advertised `sas` and the session has control.
   [V] sends `sas`.
2. [H] Control gate open → `platform.secure_attention()`.
3. [W] Sends `SecureAttention` to [A]; [A] writes `sas` to [S]; [S] applies the policy rule of
   §3.6 and calls `SendSAS`; the answer travels back.
4. [H] Sends `actr` to [V]. On `policy`, [VU] says so.

### 5.6 Lock from the viewer

[V] sends `lock` → [H] control gate → `platform.lock_session()` (the host itself, as the user) →
the machine locks → flow 5.1 follows by itself.

### 5.7 A UAC prompt

The same as 5.1 and 5.2: the prompt's desktop takes input, the wrapper moves to `Agent`, the
controller answers the prompt, the desktop returns, the wrapper moves to `Local`. A prompt that
comes and goes inside one `reinit` leaves the source `Local`: the attach is refused, the agent
answers `Desktop { secure: false }`, the next `reinit` 250 ms later succeeds locally.

### 5.8 Stopping the service, upgrading

1. [S] Receives stop. It closes the agent's control pipes.
2. [A] `agent-control` sees the close: release held keys, close the client's pipe, exit 0.
3. [S] Waits for the agent's process handle (5 s, then terminate and wait), then reports stopped.
4. [W] The pipe broke: `Lost`, then `Blocked(AgentUnavailable)` if a secure desktop is up.
5. The installer replaces both binaries and starts the service
   (`app/installer-hooks.nsh:98-106`); [S] launches a new agent; [W] connects at its next `reinit`.

### 5.9 The agent dies while the machine is locked

[W] broken pipe → `Lost` → `Blocked(AgentUnavailable)` → `hstate blocked`. [S] sees the exit code,
relaunches after the delay. [W]'s next connect succeeds → `Agent` → `hstate secure` + keyframe.
Keys held by the dead agent were released by Windows with its process.

### 5.10 The host dies or exits while attached

[A] The client's pipe closes: stop capture, release held keys and buttons, drop the section,
back to `Listening`. [S] handles the host as today (`service/src/main.rs:266-278`).

### 5.11 Control is revoked on the lock screen

[H] The engine calls `release_all` on revocation (the existing invariant,
`crates/platform-api/src/input.rs:28-29`). [W] forwards it; [A] releases what it holds. Input
after that is dropped by the control gate before it reaches [W].

### 5.12 Sign-out and fast user switching

[S] The session event or the poll reports the change. The old session's agent is stopped (5.8
steps 1–3). The new session gets a host and an agent when a user is signed in there
(`service/src/supervise.rs:85-97`). A viewer connected to the old session's host loses it as
today.

### 5.13 Several sessions on one host

Each session has its own capture wrapper and injector; they share the one `AgentClient`. The
agent serves one attachment per output; two sessions on the same output are served by one
duplication and one section, with `Frame` acknowledged by the slower reader. Whether U3 ships
this or one-secure-session-at-a-time (later sessions get `Blocked(AgentUnavailable)`) is decided
by the review; pop-out streams (slots 1..) do not use the agent in this plan and freeze while a
secure desktop is up.

## 6. Thread and failure rules

- No `unwrap` or `expect` on a pipe, token, process, section or D3D result. Each failure maps to
  a `BlockReason`, an `AgentError` with a message a person can act on, or a logged line.
- The agent's worker threads never create windows or hooks (`SetThreadDesktop` would fail).
- No UI call and no engine event is sent while a lock is held.
- Every wait has a timeout: `attach` 2 s, `secure_attention` 3 s, the service's wait for the
  agent 5 s, a frame wait is the engine's own `acquire` timeout.
- The host's capture thread never blocks on the pipe's writer for longer than one small message;
  a full pipe buffer (a stuck agent) is a broken pipe after 2 s.

## 7. Invariants

| # | Invariant |
|---|---|
| K1 | The agent captures and injects only while the input desktop is not `Default`. |
| K2 | The agent serves one client. It accepted that client only after the image-path, session and user checks, and holds the client's process handle until the connection ends. |
| K3 | The agent opens no network connection and no file except its log, and reads nothing from the user's profile. |
| K4 | No key code, typed character or pointer position is written to any log by the host, the agent or the service. |
| K5 | Every change of capture source reaches the engine as `Lost` and is followed by a forced keyframe. |
| K6 | The agent writes the frame section only between an `Ack` and its next `Frame`; the host reads it only between a `Frame` and its `Ack`. |
| K7 | A message over its size limit, unparsable, unknown or out of state closes the connection. The agent's parser has no `unsafe`. |
| K8 | Input reaches the agent only through the engine's control gate; the agent adds no permission of its own and removes none. |
| K9 | Keys and buttons the agent holds are released when control is revoked, the session ends, the pipe closes, the input desktop changes, or the agent is told to stop. |
| K10 | The service reports stopped only after the agent's process has ended. |
| K11 | With the setting off the host never opens the pipe and refuses `sas`. |
| K12 | A host without the service behaves as today, plus `hstate`. |
| K13 | `hstate` is sent when the ctl channel opens and whenever the capture's `ScreenState` changes, and never more often than that. |
| K14 | The pipe is created with its access list, its integrity label, first-instance and reject-remote; a creation that cannot set all four fails and the agent exits non-zero. |
| K15 | The agent's process dies with the service (job object) and exits by itself when its control pipe closes. |
| K16 | ShareCtrlScreen never stores, caches or replays a Windows password or PIN. |
| K17 | The normal desktop's capture and input path is the code that runs today; the agent is tried only after a refused local attach. |
| K18 | `sas` and `lock` are honoured only for a session with control, and `sas` at most once a second. |

## 8. The sign-in screen (outline; completed in step S6)

**The pre-login host.** When the active console session has no user and a pre-login profile
exists, the service starts the engine without a window, as LocalSystem, in that session. It
registers with the profile's address, accepts `password` mode only, sends `hstate secure`, and
has no clipboard, no file transfer and no pop-outs. It captures and injects in-process with the
same follower; it does not use the agent. Which binary carries it (`sharectrl.exe` in a headless
mode before Tauri starts, or the service binary linking the engine) is an S6 question.

**The profile.** Address, server, password hash and permission, ICE servers. The password hash is
enough to answer the challenge (`pans/00-OVERVIEW-AND-PROTOCOL.md:132-138`), so it is a secret:
the store must be readable only by SYSTEM and administrators. Candidates: a registry key under
`HKLM` with its own access list, or a file in the install folder. Written by the service on a
request from the checked host over §3.6's channel; cleared the same way and at uninstall.

**The handover at sign-in.** Recommended: the pre-login host keeps a live session through the
sign-in (as SYSTEM it can capture the user's desktop too) and exits when that session ends; the
user's host starts as usual but stays offline until the pre-login host has released the address.
With no live session the pre-login host exits at sign-in immediately. The alternative, ending the
session at sign-in with a reason and letting the viewer reconnect, is simpler and worse to use.

**Open for S6:** D4 and D6; the release and takeover of the address without a window where
neither host is registered or both are; what the user's host shows meanwhile; sign-out with a
live user session; what an account sync means for a host with no account; uninstall and the
profile.
