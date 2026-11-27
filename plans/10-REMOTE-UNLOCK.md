# 10 — Remote unlock: the lock screen, UAC prompts and the sign-in screen on Windows

Written 2026-11-27 against `main` @ `0091c93` (v2.3.3). Read-only research of the Windows
service, the `elevation` crate, the capture and input adapters, the host media loop and the
control protocol; every claim cites `file:line` (paths relative to `native/` unless they start
with `plans/`, `pans/`, `docs/` or `README.md`). Structure, contracts and flows are in
[10a](10a-STRUCTURE-AND-FLOWS.md). Nothing here is built yet.

**Not verified from Microsoft's documentation.** `learn.microsoft.com` was unreachable from the
session that wrote this plan. Every statement about Windows behaviour below comes from this
repository's own design (`pans/04-NATIVE-REWRITE-PLAN.md` §8b) and general knowledge. §8 lists
each one; step S is not complete until they are read and cited.

## Progress

| Step | What | Status |
|---|---|---|
| S | This plan + structure ([10a](10a-STRUCTURE-AND-FLOWS.md)), adversarial review folded in, Microsoft references read | **in progress**: plan and 10a drafted. Open: the adversarial review (§7), the Microsoft references (§8), owner's answers to §5 D3–D6 |
| U1 | The viewer is told what the host's screen is doing | not started |
| U2 | Service: session events, the agent's lifecycle, the pipe | not started |
| U3 | The agent captures and types on the secure desktop | not started |
| U4 | Ctrl+Alt+Del and Lock | not started |
| U5 | UAC prompts | not started |
| S6 | Structure and review for the sign-in screen | not started |
| U6 | The sign-in screen with nobody signed in | not started |
| Final | Whole-feature review, merge, release | not started |

## 1. What the owner asked for (2026-11-27)

- "When I connect to a Windows computer that's locked, it just shows a black screen and I can't
  unlock it." The remote computer must be unlockable through ShareCtrlScreen.
- Scope, chosen from three options: the **lock screen** of a signed-in user, **UAC prompts**, and
  the **sign-in screen** when nobody is signed in (after a restart or a sign-out), in phases.
- Controlling the lock screen is **on by default** for an installed host, with a setting on the
  host to turn it off.

This matches decision #8 (`plans/00-DECISIONS.md:15`): lock screen in scope on every OS, the
sign-in screen on Windows only. This plan covers Windows. The macOS and Linux lock screens stay
with their own tasks (`plans/checklists/M1.md:74-77`).

## 2. Where we are today (facts the design must respect)

1. **The host is the user's process.** The service asks for the signed-in user's token and starts
   `sharectrl.exe` with it (`service/src/main.rs:305-315`) on `winsta0\default`
   (`crates/elevation/src/session.rs:82`). The only addition is the UIAccess flag
   (`crates/elevation/src/uiaccess.rs:234-256`). Nothing runs as SYSTEM inside the user's session.
2. **The lock screen, the sign-in screen and UAC prompts are on the Winlogon desktop.** A user
   process cannot open it, and UIAccess does not help (`uiaccess.rs:56-59`, `pans/04` §8b
   lines 448-451). The desktop follower that would attach to it exists
   (`crates/elevation/src/desktop.rs:38-68`) and is called by the capture thread
   (`crates/platform-windows/src/adapters.rs:260-274`) and the injector (`adapters.rs:769`), but
   it runs in the user's process, so `OpenInputDesktop` is refused.
3. **Capture retries forever and sends nothing.** Both `AccessLost` and `AccessDenied` become
   `CaptureError::Lost` (`adapters.rs:69`). Mid-session the host loop calls `reinit`, sleeps
   250 ms and tries again without limit (`crates/engine/src/media/host.rs:790-803`). Connecting
   while already locked never leaves `open_capture` (`host.rs:296-345`), so no encoder is created
   and no frame is ever sent.
4. **The viewer is not told.** No host-state message exists: `ControlMsg` has `perm`, `bye`, `kf`,
   `vpause`, `vresume`, `cur`, `vsize`, `ping`, `pong`, `clip`, `curshape`, `displays`, `seldisp`,
   `popout`, `popclose`, `itgt` (`crates/protocol/src/control.rs:10-126`). The viewer reveals its
   surface once its decoder exists (`crates/engine/src/media/viewer.rs:66-69`) and the renderer
   clears to black (`crates/render/src/lib.rs:345-348`). That is the black screen. A session that
   was connected before the lock shows its last frame instead.
5. **Input is dropped without a trace.** `let _ = self.desktop.follow();` (`adapters.rs:769`)
   discards the refusal, and the `SendInput` results are not checked
   (`crates/input/src/lib.rs:100`, `:218`). Events go to the desktop the thread was already on.
6. **Ctrl+Alt+Del cannot be sent.** No `SendSAS` call and no wire message exist. The platform hint
   says it "can never be captured" (`crates/platform-windows/src/lib.rs:138-140`), which is true
   for capturing on the viewer and says nothing about sending it to the host.
7. **Nobody signed in means no host.** `WTSQueryUserToken` fails, the launch is retried with a
   doubling delay up to 300 s (`service/src/main.rs:287-290`, `service/src/supervise.rs:28-33`).
   The delay resets only when the session id changes (`supervise.rs:86-90`), so a sign-in that
   keeps the same session id can leave the machine without a host for up to five minutes. This is
   read from the code and has not been observed.
8. **The service reacts to nothing but stop.** It registers `RegisterServiceCtrlHandlerW`, accepts
   only `SERVICE_ACCEPT_STOP` (`service/src/main.rs:177`, `:187-191`, `:197`) and polls the active
   console session every 2 s (`main.rs:279`, `:300`). Lock, unlock, sign-in and sign-out are not
   events for it.
9. **There is no channel between the service and the host.** The service passes flags
   (`main.rs:245-251`); the host reads the service's state (`app/src/updater.rs:177-183`). `pans/04`
   §8b line 485 calls for "Service ↔ engine IPC over a named pipe"; it was never built.
10. **The documents disagree with the code and with each other.**
    - `README.md:35` and `docs/FIRST-RUN.md:24-26` say the service reaches the lock screen, UAC
      prompts and the sign-in screen. `README.md:132` says the secure desktop is out of reach.
    - `README.md:141` says the login window works. `crates/platform-windows/src/lib.rs:141` sets
      `login_screen_supported: true`, which hides the note "someone must sign in on this machine
      before you can connect" (`app/ui/settings.js:33`, `app/ui/index.html:411-414`).
    - `plans/checklists/W8.md:63` expects "Clicking a UAC prompt on H from B still works". The
      code gives no path for that.
    - `plans/05-TASKS.md:240-242` says the capability "is tracked separately". No task tracks it.
11. **What already works and must keep working.** The capture frame is a D3D11 texture on the
    device the encoder was built from (`adapters.rs:333-342`, `host.rs` encoder creation from
    `cap.device()`). The host loop already recovers from `Lost` by `reinit` and a forced keyframe
    (`host.rs:790-818`). Input is injected only while the session has control
    (`crates/engine/src/session/driver.rs:1044-1052`). New host→viewer messages are tolerated by
    old viewers, which cannot parse them and ignore them (`control.rs:83`, `:89`).
12. **Approval needs a person.** In `approve` mode the host's prompt auto-denies after 30 s
    (`pans/00-OVERVIEW-AND-PROTOCOL.md:193-195`). A locked machine has nobody to click Allow, so
    reaching one needs `password` mode (`pans/00:26-32`) or a session that was already connected.

## 3. Target design

### 3.1 A SYSTEM agent in the user's session, used only for the secure desktop

The service starts a second process next to the host: the **secure-desktop agent**, running as
LocalSystem inside the console session. It is the "SYSTEM-context capture/inject helper" of
`pans/04` §8b (lines 480-484). It does three things and nothing else: capture the input desktop,
inject input into it, and ask the service for Ctrl+Alt+Del.

The agent works **only while the input desktop is not `Default`**. On the normal desktop the host
captures and injects exactly as today; the verified path (W8) is not touched. The host asks for
the agent only after its own attach to the input desktop was refused.

The host stays the one place that knows who is connected and with which permission. The agent
never talks to the network and has no notion of a remote session.

### 3.2 The pipe

One named pipe per console session, served by the agent. Only SYSTEM and that session's user may
open it; remote clients are refused. The agent accepts a client only if it is `sharectrl.exe`
from the agent's own folder, in the agent's session, running as the session's user. The host
accepts the server only if it is `sharectrl-service.exe` from its own folder running as SYSTEM.
A portable host (not next to the service) therefore gets no agent; that is intended.

Messages are small JSON documents with a length prefix and a version. The agent parses input
from a less privileged process, so every message has a size limit and unknown or malformed
messages close the connection.

### 3.3 Frames

The agent copies changed regions of the captured desktop into a shared-memory section and tells
the host which rectangles changed. The host copies them into a texture on its own D3D11 device
and hands that texture to the encoder, so the encoder path is the one used today. The agent does
not write the next frame until the host has acknowledged the last one.

Shared GPU textures would avoid the copies. They are not chosen: the lock screen and UAC prompts
change little, and shared memory has none of the cross-process GPU failure modes (two adapters on
a laptop, a keyed mutex abandoned by a dead process, device loss on one side). Decision D5.

### 3.4 One capture object for the engine

In `platform-windows` a wrapper implements `ScreenCapture` over two sources: the existing
duplication for the normal desktop and the agent for the secure desktop. Every change of source
is reported to the engine as `CaptureError::Lost`, so the handover is the reinit-then-keyframe
path the engine already has (fact 11). The wrapper owns the D3D11 device, so a session that
starts while the machine is locked gets its encoder and starts with the agent's picture.

This needs the duplication to be creatable later than its device. `capture::Duplicator::new`
creates both at once (`crates/capture/src/lib.rs:140`); splitting them is a refactor with no
behaviour change and lands first.

### 3.5 Telling the viewer

A new host→viewer message `hstate` says what the host's screen is: `normal`, `secure` (a secure
desktop, streamed through the agent) or `blocked` (a secure desktop that cannot be shown, with
the reason: no service, the setting is off, the agent is not answering). The viewer shows a clear
message in the tab instead of a black or frozen picture. This ships first and is useful on its
own: it turns today's silent black screen into an explanation.

### 3.6 Ctrl+Alt+Del and Lock

Two new viewer→host messages, `sas` and `lock`, each offered only to a host that advertises it in
`HostCaps` (`crates/protocol/src/signaling.rs:150-159`) and honoured only for a session with
control. `sas` goes host → agent → service, and the service calls `SendSAS`. `lock` locks the
host's session. The viewer gets "Send Ctrl+Alt+Del" and "Lock remote computer" in the session bar
(`app/ui/index.html:134-146`) and in the native Session menu on macOS and Linux
(`app/src/menu.rs`).

On current Windows the lock screen does not need Ctrl+Alt+Del: a key press or a click dismisses
it and the password box appears. `sas` is for machines whose policy requires it.

### 3.7 The sign-in screen (U6)

With nobody signed in there is no user, no user host and no user profile. The service starts a
**pre-login host**: the engine without a window, as SYSTEM, in the console session. It registers
with the identity a user of this machine opted in (D4), accepts `password` mode only, and has no
clipboard and no file transfer. Being SYSTEM, it captures the sign-in screen itself and needs no
agent.

The hard part is the moment someone signs in while a pre-login session is live. The recommended
design keeps that session alive: the pre-login host can also capture the user's desktop, so it
keeps serving until the viewer disconnects, and the user's host waits to go online until then.
This has its own structure section and review (step S6) before any of it is built.

### 3.8 Policies

- **Permission.** Unchanged. Typing on the lock screen needs control, exactly like typing on the
  desktop. A view-only viewer sees the lock screen.
- **Setting.** `allowLockScreenControl`, default on. Off: the host never uses the agent and
  refuses `sas`; the viewer gets `blocked` with the reason.
- **Credentials.** ShareCtrlScreen never stores, caches or types Windows passwords by itself.
  The password is typed by the controller and travels as ordinary key events. Key events are
  never logged, on any side, at any log level.
- **Honest limits.** A UAC credential prompt still needs credentials. Protected content stays
  black. A portable host has no agent. A remote desktop (RDP) session is not the console and is
  not covered.

## 4. Phases (each ships on its own)

Sizes: S ≤ 1 day, M 2–4 days, L 1–2 weeks. Every phase passes the lane gates on Windows and WSL,
the macOS compile check and the loopback suite, and writes its device checklist
(`plans/checklists/UNLOCK-<phase>.md`).

```
S ─► U1 ─► U2 ─► U3 ─┬─► U4 ─┬─► S6 ─► U6 ─► Final
                     └─► U5 ─┘
```

### S — Structure and review (S)

This plan and [10a](10a-STRUCTURE-AND-FLOWS.md); one independent adversarial review, each finding
verified against the code and folded into §7; the Microsoft references of §8 read and cited;
D3–D6 answered by the owner.

### U1 — The viewer is told (M)

- Deliverable: `ScreenState` in `platform-api`; `hstate` in `protocol`; the host media loop sends
  it on every change and while it waits in `open_capture`; the viewer shows the message in the
  tab and in detached and pop-out windows. Dropped input is logged once per outage, never per
  event. `login_screen_supported` becomes `false` until U6. The README, `docs/FIRST-RUN.md` and
  `plans/checklists/W8.md` say what is true.
- Tests: serde round trips (an old viewer ignores `hstate`); a mock capture that stays lost; a
  loopback test that the viewer receives `blocked` and then `normal`.
- Acceptance: a locked host shows "The remote computer is locked" with the reason on the viewer
  within 2 s, and the picture returns by itself after a local unlock.

### U2 — Service: session events, agent lifecycle, the pipe (M) — no visible change

- Deliverable: the service handles session-change controls and keeps the 2 s poll as a fallback;
  a sign-in resets the launch delay (fact 7). The agent process: token, job object, launch,
  supervision, stop. The pipe: framing, limits, peer checks, both ends. The agent answers `hello`
  and nothing else yet.
- Tests: framing and limits (oversized, truncated, unknown, wrong version); the supervisor with
  the agent and with session events; `#[ignore = "needs the installed service"]` tests for the
  token and the peer checks.
- Acceptance: on a device the agent runs as SYSTEM in the user's session, the host connects and
  logs the handshake, another program is refused, stopping the service ends the agent before the
  service reports stopped, an upgrade install succeeds.

### U3 — The agent captures and types on the secure desktop (L)

- U3a (no behaviour change): `capture::Duplicator` splits device creation from duplication.
- U3b: the agent's capture and input; the shared-memory frames; the wrapper capture and the
  forwarding injector in `platform-windows`; `hstate` reports `secure`; the setting.
- Tests: the wrapper's source state machine against a fake agent; frame flow control; the engine
  loopback with a mock that switches source.
- Acceptance: lock the host, see the lock screen on the viewer, dismiss it, type the password or
  PIN, arrive on the desktop, all in one session without reconnecting. Connect to an already
  locked host in password mode and do the same. Latency on the normal desktop is unchanged
  against the W8 checklist.

### U4 — Ctrl+Alt+Del and Lock (S)

- Deliverable: `sas` and `lock`, the two `HostCaps` flags, the viewer actions, `SendSAS` in the
  service, the policy handling of D3.
- Acceptance: on a machine that requires Ctrl+Alt+Del to sign in, the action brings up the
  password box; where an administrator forbids software Ctrl+Alt+Del the viewer is told so.

### U5 — UAC prompts (S)

- Deliverable: nothing new is expected beyond U3; this phase verifies the short desktop switches
  of a prompt (appearing and disappearing within a second) and fixes what they show, then updates
  the README limits.
- Acceptance: a consent prompt and a credential prompt are visible and usable from the viewer;
  a prompt that appears and is dismissed locally in under a second leaves the session running.

### S6 — Structure and review for the sign-in screen (S)

10a §8 completed (the pre-login profile, the identity, the handover at sign-in and sign-out,
what the user's host does meanwhile), reviewed adversarially, D4 and D6 answered.

### U6 — The sign-in screen (L)

- Deliverable: per S6.
- Acceptance: restart the host, connect from the viewer without anyone touching it, sign in,
  work. Sign out and connect again. With the option off, the machine is unreachable until someone
  signs in and the settings say so.

### Final (S)

Whole-feature review, the README status table, version bump, release.

## 5. Decisions

| # | Question | Decision |
|---|---|---|
| D1 | Scope | Lock screen + UAC prompts + sign-in screen, in phases (owner, 2026-11-27) |
| D2 | Default | Lock-screen control on by default, a host setting turns it off (owner, 2026-11-27) |
| D3 | Windows may refuse software Ctrl+Alt+Del unless a policy allows it. What do we do? | **Open.** Recommended: allow it only for the moment of sending and put the previous value back; if an administrator set the policy explicitly, leave it and tell the viewer |
| D4 | Which identity answers at the sign-in screen when the machine has several Windows accounts? | **Open, before U6.** Recommended: one account opts in under Settings; its address and password are copied to a store only SYSTEM and administrators can read; a second account that opts in replaces it after a warning |
| D5 | Shared memory or shared GPU textures for the agent's frames? | **Open, technical.** Recommended: shared memory (§3.3) |
| D6 | Is reaching the sign-in screen on by default too? | **Open, before U6.** Recommended: off by default, because it publishes one account's address for the whole machine and works only with a password |

## 6. Risks

| Risk | Mitigation |
|---|---|
| A program running as the same user tampers with the host process and drives the secure desktop through it (for example approving a UAC prompt on an administrator's account) | The pipe's access list and both peer checks; the agent acts only off the `Default` desktop; one service-log line per secure-desktop use; step S evaluates starting the host with a process access list that denies memory writes to the user. The remaining risk is stated in the README |
| The verified normal-desktop path regresses | The existing duplication code is not changed in behaviour (U3a); the agent is used only after a refused attach; the W8 checklist is re-run in U3 |
| The hardware encoder cannot be created while the Winlogon desktop has input (a session that starts while locked) | Unknown until a device run; U3's checklist has this as its first step. If it fails, the encoder is created on first return to the desktop and the session starts with `blocked` |
| The agent or the pipe dies while the machine is locked | The wrapper falls back to today's retry loop and reports `blocked`; the service restarts the agent with the existing doubling delay |
| The installer cannot replace `sharectrl-service.exe` because the agent still runs from it | The service ends the agent and waits for it before it reports stopped; a job object ends the agent if the service dies |
| A SYSTEM process parsing messages from a user process | Size limits, strict parsing, no `unsafe` in the parser, a test for every malformed shape; a parse error closes the connection |
| The pipe is created at system integrity and a medium-integrity host cannot write to it | The pipe gets an explicit medium integrity label. Unverified until U2's device step |
| A pre-login session is live when someone signs in (U6) | Designed in S6 before any code |
| An administrator's policy forbids software Ctrl+Alt+Del | Detected and reported (D3) |
| A manual install in a folder the user can write lets the user replace the service binary | Exists today (`README.md:307`), not made worse by this plan; noted for the owner |

## 7. Design review (2026-11-27) → changes

To be filled in by step S's review. Where this section and §2–§6 or 10a disagree, this section
wins.

## 8. Unverified: to read and cite in step S

Each of these is used by the design and comes from memory or from `pans/04` §8b, not from a
documentation read in this session.

| Claim | Where it matters |
|---|---|
| The lock screen, the sign-in screen and UAC prompts run on the `Winlogon` desktop, and only SYSTEM may open it | The whole design |
| Desktop Duplication works on the secure desktop only for a LocalSystem process in that session whose thread is attached to the input desktop | U3 |
| Setting `TokenSessionId` on a duplicate of a LocalSystem token needs SeTcbPrivilege and yields a SYSTEM process in the user's session | U2 |
| `SendSAS(FALSE)` works from a LocalSystem service and is governed by the `SoftwareSASGeneration` policy, whose default forbids services | U4, D3 |
| `RegisterServiceCtrlHandlerExW` with `SERVICE_ACCEPT_SESSIONCHANGE` delivers lock, unlock, logon, logoff and console connect events | U2 |
| An object created by a system-integrity process is not writable from medium integrity without an explicit label | U2 |
| `SetThreadDesktop` fails on a thread that owns windows or hooks | U3 (already relied on, `adapters.rs:254-257`) |
| A current Windows lock screen is dismissed by any key or click without Ctrl+Alt+Del unless policy says otherwise | U3, U4 |

Nothing in this plan counts as working until its device checklist passes (decision #15).
