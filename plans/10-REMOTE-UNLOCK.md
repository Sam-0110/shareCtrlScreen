# 10 — Remote unlock: the lock screen, UAC prompts and the sign-in screen on Windows

Written 2026-11-27 against `main` @ `0091c93` (v2.3.3). Read-only research of the Windows
service, the `elevation` crate, the capture and input adapters, the host media loop and the
control protocol; every claim cites `file:line` (paths relative to `native/` unless they start
with `plans/`, `pans/`, `docs/`, `README.md` or `CLAUDE.md`). Structure, contracts and flows are
in [10a](10a-STRUCTURE-AND-FLOWS.md). Nothing here is built yet.

**Revision 2**, after the design review of the same day (§7). The review changed the trust
model, the phases and several contracts; §2–§6 below already include its results.

**Not verified from Microsoft's documentation.** `learn.microsoft.com` was unreachable from the
session that wrote this plan and from its reviewer. Every statement about Windows behaviour comes
from this repository's own design (`pans/04-NATIVE-REWRITE-PLAN.md` §8b) and general knowledge.
§8 lists each one; step S is not complete until they are read and cited.

## Progress

| Step | What | Status |
|---|---|---|
| S | This plan + structure ([10a](10a-STRUCTURE-AND-FLOWS.md)), adversarial review folded in, Microsoft references read | **in progress**: plan and 10a written, review folded in (§7). Open: the Microsoft references (§8), the owner's answers to D3, D7 and D8 (§5) |
| U1 | The viewer is told what the host's screen is doing | not started |
| U2 | Service: session events; a sign-in no longer waits out the launch delay | not started |
| U3 | The agent: the lock screen of a signed-in user | not started (begins with a device spike) |
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
- The approved outline also had a "Lock remote computer" action next to Ctrl+Alt+Del.

This matches decision #8 (`plans/00-DECISIONS.md:15`): lock screen in scope on every OS, the
sign-in screen on Windows only. This plan covers Windows. The macOS and Linux lock screens stay
with their own tasks (`plans/checklists/M1.md:74-77`).

## 2. Where we are today (facts the design must respect)

1. **The host is the user's process.** The service asks for the signed-in user's token and starts
   `sharectrl.exe` with it (`service/src/main.rs:305-315`) on `winsta0\default`
   (`crates/elevation/src/session.rs:82`). The only addition is the UIAccess flag; the host stays
   the same user at the same integrity level (`crates/elevation/src/uiaccess.rs:10-13`,
   `:234-256`). Nothing runs as SYSTEM inside the user's session.
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
   keeps the same session id can leave the machine without a host for up to five minutes. Read
   from the code, not observed.
8. **The service reacts to nothing but stop.** It registers `RegisterServiceCtrlHandlerW`, accepts
   only `SERVICE_ACCEPT_STOP` (`service/src/main.rs:177`, `:187-191`, `:197`) and polls the active
   console session every 2 s (`main.rs:279`, `:300`). Lock, unlock, sign-in and sign-out are not
   events for it.
9. **There is no channel between the service and the host.** The service passes flags
   (`main.rs:245-251`); the host reads the service's state (`app/src/updater.rs:177-183`).
   `pans/04` §8b line 485 calls for "Service ↔ engine IPC over a named pipe"; it was never built.
10. **The documents disagree with the code and with each other.**
    - `README.md:35` says the service "reaches the lock screen and UAC prompts";
      `docs/FIRST-RUN.md:24-26` adds the sign-in screen; `README.md:132` says the secure desktop
      is out of reach; `README.md:140` says "SYSTEM + desktop-follow" for the lock screen.
    - `README.md:141` says the login window works. `crates/platform-windows/src/lib.rs:141` sets
      `login_screen_supported: true`, which hides the note "someone must sign in on this machine
      before you can connect" (`app/ui/settings.js:33`, `app/ui/index.html:411-414`).
    - The same claim is in `CLAUDE.md:53`, `native/README.md:97`,
      `app/installer-hooks.nsh:57-58` and the header of `crates/elevation/src/lib.rs:1-2`.
    - `plans/checklists/W8.md:63` expects "Clicking a UAC prompt on H from B still works". The
      code gives no path for that.
    - `plans/05-TASKS.md:240-242` says the capability "is tracked separately". No task tracks it.
11. **What already works and must keep working.**
    - The capture frame is a D3D11 texture on the device the encoder was built from
      (`adapters.rs:333-342`, `media/host.rs:422`).
    - The host loop recovers from `Lost` by `reinit`, a re-read of the size and a forced keyframe
      (`host.rs:790-819`). The size is re-read only there (`host.rs:813-817`).
    - Input is injected only while the session has control
      (`crates/engine/src/session/driver.rs:1044-1052`).
    - A host→viewer message an old viewer does not know is ignored (`control.rs:83`, `:89`).
12. **One host session, several captures.** A host serves one viewer at a time
    (`crates/engine/src/sessions.rs:266`, `crates/engine/src/session/registry.rs:170`). That one
    session can hold several capture objects at once: a monitor switch creates the new capture
    while the old one is alive (`media/host.rs:496-497`), and every pop-out stream opens its own
    on its own thread (`host.rs:396`).
13. **A paused stream does not capture.** A background tab's main stream skips `acquire`
    altogether (`host.rs:660-663`).
14. **Approval needs a person.** In `approve` mode the host's prompt auto-denies after 30 s
    (`pans/00-OVERVIEW-AND-PROTOCOL.md:193-195`). A locked machine has nobody to click Allow, so
    reaching one needs `password` mode (`pans/00:26-32`) or a session that was already connected.

## 3. Target design

### 3.1 A SYSTEM agent in the user's session, used only for the secure desktop

The service starts a second process next to the host: the **secure-desktop agent**, running as
LocalSystem inside the console session. It is the "SYSTEM-context capture/inject helper" of
`pans/04` §8b (lines 480-484). It captures the secure desktop, injects input into it, and asks
the service for Ctrl+Alt+Del. It never talks to the network and has no notion of a remote
session; the host stays the one place that knows who is connected and with which permission.

On the normal desktop the host captures and injects exactly as today. The host asks for the
agent only after `OpenInputDesktop` was refused, so the verified path (W8) is not touched.

### 3.2 Who the agent trusts: nobody in the user's session

The host is a medium-integrity process of the signed-in user (fact 1). Any other program running
as that user can read its memory, copy its handles or load code into it. So the design assumes:

> **Whatever the host can ask of the agent, any program running as the signed-in user can ask.**

The checks on who connects to the pipe keep honest mistakes and other accounts out. They are not
a defence against the user's own programs, and nothing in this plan claims they are. What the
agent will do is therefore decided **on the SYSTEM side**, by the state of the machine:

| The secure desktop is up because… | What a same-user program could gain | The agent |
|---|---|---|
| The session is **locked** | The lock screen's picture; typing guesses; restart from the power button. Unlocking still needs the password | Serves it (U3). The user's own setting can turn this off (a preference, not a protection) |
| A **UAC prompt** or the Ctrl+Alt+Del screen is up in an **unlocked** session | On an administrator's account: approving its own elevation with one click, which removes what UAC does | Refuses unless an **administrator** enabled it for the machine (U5, decision D7) |
| **Nobody is signed in** | — (no user programs run) | Not used; see §3.8 |

One cost remains by construction: the Windows password a controller types travels through the
user-level host on its way to the agent, where the user's own programs could read it. Windows
normally keeps that password away from user programs. The alternative that avoids it (the whole
engine as SYSTEM) puts the network stack in a SYSTEM process instead. This is stated in the
README's limits when U3 ships.

### 3.3 The pipe

One named pipe per console session, served by the agent, with a **random name** the service
publishes where only SYSTEM can write and users can read. Only SYSTEM and that session's user may
open it; remote clients are refused. The host accepts a pipe only if the pipe object is owned by
SYSTEM and served from the host's own session.

Messages are small JSON documents with a length prefix and a version. The agent parses input from
a less privileged process: every message has a size limit, and a malformed, unknown or
out-of-state message closes the connection. Both ends use overlapped I/O with one writer thread,
and nothing on the host's transport thread ever waits for the pipe.

### 3.4 Frames

The agent copies changed regions of the captured desktop into a shared-memory section and tells
the host which rectangles changed. The host copies them into a texture on its own D3D11 device
and hands that texture to the encoder, so the encoder path is today's. The agent does not write
the next frame until the host has acknowledged the last one, and never reads the section back.
Shared memory was chosen over shared GPU textures by the review (D5): the lock screen changes
little, and shared memory has none of the cross-process GPU failure modes.

### 3.5 One capture object for the engine

In `platform-windows` a wrapper implements `ScreenCapture` over two sources: the existing
duplication for the normal desktop and an **attachment** to the agent for the secure desktop.
Every change of source, and every change of size, reaches the engine as `CaptureError::Lost`, so
the handover is the reinit-then-keyframe path the engine already has (fact 11). Each capture
object has its own attachment, so a monitor switch and pop-out windows work on the lock screen
like the main picture (fact 12).

This needs the duplication to be creatable later than its device (`capture::Duplicator::new`
creates both, `crates/capture/src/lib.rs:140`); that split is a refactor with no behaviour change
and lands first. Whether the hardware encoder can be created and keeps encoding while the
Winlogon desktop has input is unknown; **U3 starts with a device spike** that answers it before
the rest of U3 is built.

### 3.6 Telling the viewer

A new host→viewer message `hstate` says what the host's screen is: `normal`, `secure` (a secure
desktop, streamed through the agent) or `blocked` (a secure desktop that cannot be shown, with
the reason). While `blocked`, the viewer hides the video and its pop-outs, drops the frame it was
holding, and shows a message in the tab. This ships first and is useful alone: it turns today's
silent black screen into an explanation. `hstate` is sent only to a viewer that asked for it in
its capabilities.

### 3.7 Ctrl+Alt+Del and Lock

Two viewer→host messages, `sas` and `lock`, each offered only to a host that advertises it and
honoured only for a session with control. `sas` goes host → agent → service, and the service
calls `SendSAS`; the result returns to the viewer asynchronously. `lock` locks the host's
session; a host advertises it only in `password` mode, because locking an `approve` host would
leave nobody able to approve the next connection.

On current Windows the lock screen does not need Ctrl+Alt+Del: a key press or a click dismisses
it and the password box appears. `sas` is for machines whose policy requires it.

### 3.8 The sign-in screen (U6)

With nobody signed in there is no user, no user host and no user profile, so something must run
without a user and answer for the machine. That is a larger design than the agent and has its own
structure and review step (S6). The review fixed three constraints for it now:

- The profile that says which address and password answer at the sign-in screen is machine-wide.
  **Writing it needs an administrator's consent**, never a request from a user-level process.
- **A session that began at the sign-in screen never continues into another account's desktop.**
- S6 must compare a low-privilege pre-login process that uses the agent against putting the
  engine and its network stack in a SYSTEM process, and justify the choice.

### 3.9 Policies

- **Permission.** Typing on the lock screen needs control, exactly like typing on the desktop.
  Whether a view-only viewer sees secure screens at all is decision D8.
- **Setting.** `allowLockScreenControl`, default on (D2). Off: the host never uses the agent.
- **Credentials.** ShareCtrlScreen never stores, caches or types Windows passwords by itself. Key
  events are never logged, on any side, at any log level.
- **Honest limits.** A UAC credential prompt still needs credentials. Protected content stays
  black. A portable host has no agent. A remote desktop (RDP) session is not the console and is
  not covered.

## 4. Phases (each ships on its own)

Sizes: S ≤ 1 day, M 2–4 days, L 1–2 weeks. Every phase passes the lane gates on Windows and WSL,
the macOS compile check and the loopback suite, and writes its device checklist
(`plans/checklists/UNLOCK-<phase>.md`). Tests that need the installed service follow the
`SCT_*_REQUIRED` convention with `SCT_AGENT_TESTS_REQUIRED`.

```
S ─┬─► U1 ─┬─► U3 ─┬─► U4 ─┬─► S6 ─► U6 ─► Final
   └─► U2 ─┘       └─► U5 ─┘
```

### S — Structure and review (S)

This plan and [10a](10a-STRUCTURE-AND-FLOWS.md); the adversarial review folded in (§7); the
Microsoft references of §8 read and cited; D3, D7 and D8 answered by the owner.

### U1 — The viewer is told (M)

- Deliverable: `ScreenState` in `platform-api`; `hstate` and the viewer capability in `protocol`;
  the host loop sends it on change and while it waits in `open_capture`; `DdaCapture` reports
  `Blocked(NotSupported)` after a refused attach; the viewer hides the video and pop-outs, drops
  its held frame and shows the message. Dropped input is logged once per outage, never per event.
  `login_screen_supported` becomes `false` until U6. Every document of fact 10 says what is true.
- Tests: serde round trips (an old viewer never receives `hstate`); a mock capture that stays
  lost; a loopback test that the viewer receives `blocked` and then `normal`.
- Acceptance: a locked host shows "The remote computer is locked" on the viewer within 2 s, and
  the picture returns by itself after a local unlock.

### U2 — Service: session events (S–M)

- Deliverable: `RegisterServiceCtrlHandlerExW` with session-change controls; the loop waits on
  them with the 2 s poll as fallback; a sign-in resets the launch delay (fact 7).
- Tests: the supervisor with session events, on every OS.
- Acceptance: on a device, signing in after the service has been failing to launch for several
  minutes brings the host up within 5 s. Independent of U1; it fixes something that affects users
  today.

### U3 — The agent: the lock screen of a signed-in user (L)

Built on a feature branch and merged when the whole phase passes, so no release ever ships an
agent that does nothing.

- U3.0 **Spike on a device, before anything else:** with the Winlogon desktop up, can the host
  create the hardware encoder on its device, and does an encoder created earlier keep encoding?
  Does Desktop Duplication work from a SYSTEM process in the session, with the display on and
  with it asleep? Can that process be put in a job object created in session 0? If any answer is
  no, U3's design returns to review before more is built.
- U3.1 (no behaviour change) `capture::Duplicator` splits device creation from duplication; the
  `input` crate's keyboard hook moves behind a feature the agent does not enable; the handle leak
  in `InputDesktopFollower::follow` (`crates/elevation/src/desktop.rs:58`) is fixed.
- U3.2 The pipe (`agent-ipc`, `elevation::pipe`), the agent's launch and supervision.
- U3.3 The agent's capture and input **while the session is locked**; the wrapper and the
  forwarding injector; the held-key ledger; `hstate secure`; the setting.
- Tests: the wrapper's state machine against a fake agent; framing and limits (oversized,
  truncated, unknown, wrong version, out of state); attachment ids and late answers; the held-key
  ledger across source changes; the engine loopback with a mock that changes source.
- Acceptance: lock the host, see the lock screen, dismiss it, type the password or PIN, arrive on
  the desktop, in one session without reconnecting. The same when connecting to an already locked
  host in password mode, with its display asleep. Win+L from the viewer leaves no key stuck.
  Clicks land right on a scaled second monitor. A UAC prompt still shows `blocked`. Latency on
  the normal desktop is unchanged against the W8 checklist.

### U4 — Ctrl+Alt+Del and Lock (S)

- Deliverable: `sas`, `lock` and the result message; the two `HostCaps` flags; the viewer
  actions; `SendSAS` in the service; the policy handling of D3.
- Acceptance: on a machine that requires Ctrl+Alt+Del to sign in, the action brings up the
  password box; where an administrator forbids software Ctrl+Alt+Del the viewer is told so; Lock
  is not offered by a host in `approve` mode.

### U5 — UAC prompts (M)

- Deliverable: the machine-wide switch an administrator sets (D7), read by the agent; the agent
  serves the secure desktop of an unlocked session when it is on; `hstate` says which kind of
  screen it is; the README's limits and the stated risk.
- Acceptance: with the switch on, a consent prompt and a credential prompt are visible and usable
  from the viewer, and a prompt that appears and is dismissed locally in under a second leaves the
  session running. With it off, the viewer sees `blocked` with the reason.

### S6 — Structure and review for the sign-in screen (S)

10a §8 completed under the three constraints of §3.8, reviewed adversarially; D4 and D6 answered.

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
| D2 | Default for the lock screen | On by default, a host setting turns it off (owner, 2026-11-27). The setting is the user's preference; it does not protect against the user's own programs (§3.2) |
| D3 | Windows may refuse software Ctrl+Alt+Del unless a machine policy allows it. May the service change that policy? | **Open.** Recommended: allow it only for the moment of sending and put the previous value back; if an administrator set the policy explicitly, leave it and tell the viewer |
| D4 | Which identity answers at the sign-in screen? | **Open, in S6.** Fixed by the review: an administrator must consent to setting it |
| D5 | Shared memory or shared GPU textures for the agent's frames? | Shared memory (design review, 2026-11-27) |
| D6 | Is reaching the sign-in screen on by default? | **Open, in S6.** Recommended: off |
| D7 | UAC prompts: once the agent serves them, any program running as the signed-in user can approve its own elevation on an administrator's account (§3.2). What is the default? | **Open.** Recommended: off until an administrator turns it on for the machine. The alternative, on by default, matches AnyDesk and gives up UAC's protection on every machine that installs ShareCtrlScreen |
| D8 | Does a **view-only** viewer see the lock screen and UAC prompts? They can show account names, notifications and a password someone reveals while typing locally | **Open.** Recommended: no; a view-only viewer gets `blocked` with the reason |
| D9 | "Lock remote computer" | Kept from the approved outline, offered only by a host in `password` mode (design review, 2026-11-27) |

## 6. Risks

| Risk | Mitigation |
|---|---|
| A program running as the signed-in user uses the agent | The agent's reach is decided on the SYSTEM side (§3.2): the lock screen only, unless an administrator enables UAC prompts. What remains (the lock screen's picture, password guesses that can lock the account out, the typed password passing through the host) is stated in the README |
| Another local account squats the pipe's name to imitate the agent or to block it | A random name published by the service after the pipe exists; the host accepts only a pipe owned by SYSTEM; the host connects without letting the server impersonate it |
| The hardware encoder cannot be created, or stops, while the Winlogon desktop has input | U3.0 answers this on a device before the rest is built |
| The verified normal-desktop path regresses | The duplication code does not change behaviour (U3.1); the agent is tried only after `OpenInputDesktop` is refused; the W8 checklist is re-run in U3 |
| Keys stuck across the change between desktops (the Enter that submits the password, Win+L) | One ledger of held keys in the host, which sees every event; key-ups always pass (10a §3.8) |
| The agent or the pipe dies while the machine is locked | The wrapper falls back to today's retry loop and reports `blocked`; the service restarts the agent with the existing doubling delay |
| The installer cannot replace `sharectrl-service.exe` because an agent still runs from it | The service ends the agent and waits for it before it reports stopped; `--stop-service` also ends agents left behind by a dead service |
| A SYSTEM process parsing messages from a user process | Size limits, strict parsing, no `unsafe` in the parser, a test for every malformed shape; a parse error closes the connection and logs no payload |
| A locked machine's display is asleep and delivers no picture | The attachment does not depend on a first frame; the first forwarded input wakes the display (10a §5.4) |
| An unsigned SYSTEM process that injects into Winlogon looks like malware to antivirus | Unknown until device runs; noted in the checklists |
| A pre-login session is live when someone signs in (U6) | Constrained in §3.8, designed in S6 |
| A manual install in a folder the user can write lets the user replace the service binary | Exists today (`README.md:307`), not made worse by this plan; noted for the owner |

## 7. Design review (2026-11-27) → changes

One independent adversarial review of this plan and 10a at `469a212`. Its claims about the code
were checked against the code before being accepted; its claims about Windows behaviour could not
be checked (§8) and were accepted where the safer design costs little. Where this section and
§2–§6 or 10a disagree, this section wins.

### Blockers, fixed in the design

1. **The agent is a confused deputy for the user's own programs.** The first draft listed the
   pipe's access list and the peer checks as the mitigation. They are not one: the host is medium
   integrity (`crates/elevation/src/uiaccess.rs:10-13`), so a same-user program can copy its pipe
   handle or run code inside it. → §3.2: the threat model is written down; the agent decides by
   machine state on the SYSTEM side; the lock screen and UAC prompts are separated; UAC control is
   an administrator's machine-wide switch and an owner decision (D7).
2. **The host could not authenticate the agent, and the pipe's name could be squatted.** A
   predictable name in a machine-wide namespace lets any local account create it first; the
   server check as drafted needed the host to open a SYSTEM process. → §3.3, 10a §3.4: a random
   name published by the service; the host checks the pipe object's owner and session.
3. **The recommended sign-in handover was a cross-account takeover.** A session begun with one
   account's ShareCtrlScreen password would have continued into whichever account signed in
   locally, and a user-level process would have written the machine-wide profile. → §3.8: the
   recommendation is struck; two constraints are fixed now; the rest is S6.
4. **"Several sessions on one host" cannot happen, and several captures in one session can**
   (facts 12). The draft's protocol had one frame slot and no way to tell captures apart. →
   10a §3.4 and §5.11: attachments with ids, one per capture object; pop-outs and the monitor
   switch use them like the main picture.

### Major, folded in

5. **Pipe I/O.** Blocking reads and writes on one handle from two threads serialise each other,
   and the draft made the transport thread wait up to 3 s. → overlapped I/O, one writer thread
   per end, results delivered asynchronously, nothing on `sct-transport` waits (10a §3.8, §6).
6. **Held keys.** The draft had the agent release keys after the desktop had left its reach, and
   left unsaid who releases a key pressed on one side and released on the other. → one ledger in
   the host; key-ups always pass (10a §3.8, K9).
7. **The "create the encoder later" fallback was not a design.** → removed; U3.0 is a gate.
8. **Display asleep.** → 10a §5.4; first step of U3's checklist.
9. **State machine gaps.** A size change now reaches the engine as `Lost` (the size is re-read
   only there, `media/host.rs:813-817`); "refused" means `OpenInputDesktop` was denied, nothing
   else; the constructor still returns `NoOutput` so the primary-display fallback keeps working
   (`host.rs:308-318`); U1 has its own reason, `NotSupported` (10a §4.1).
10. **A late answer leaked a section into the host.** → attachments have ids; an answer nobody
    waits for is closed and detached; sizes are validated before mapping (10a §3.4).
11. **Job objects across sessions** may not be allowed. → unverified list; the agent also exits
    when its control pipe closes; `--stop-service` ends agents left behind (10a §3.7).
12. **The agent's DPI awareness.** The service binary has no manifest (`service/build.rs`), the
    app is Per-Monitor-V2 (`app/sharectrl.manifest:16`) and the injector's geometry depends on it
    (`crates/input/src/lib.rs:106`). → the agent sets it at start (10a §3.5).
13. **Owner decisions presented as settled.** View-only viewers and secure screens → D8. UAC
    default → D7. Lock → D9. D5 was the reviewer's to decide: shared memory.
14. **Phases.** UAC control would have shipped in U3 and been verified in U5; U2 would have
    shipped an agent that does nothing. → U3 serves the locked session only and is built on a
    feature branch; U5 adds prompts; U2 is the service fix alone and can ship first.
15. **A network-facing SYSTEM process in U6** contradicted §3.1 without discussion. → an S6
    constraint (§3.8).

### Minor, folded in

- Citations corrected: the forced keyframe is `media/host.rs:819`; `Toast` is
  `crates/engine/src/events.rs:113`; the viewer's link-trouble panel is `app/ui/tabs.js:250`
  (`session.js:153` is the host's modal); the view type is `SessionView`
  (`crates/engine/src/session/active.rs:117`); `README.md:35` does not mention the sign-in screen.
- The native Session menu is documented as "exactly three always-enabled items"
  (`app/src/menu.rs:15`); adding two conditional items changes that contract and is part of U4.
- The agent acts on an allow-list of desktops (`Winlogon`), not on "anything but `Default`".
- A file drop presses the paste chord after the transfer
  (`crates/engine/src/session/files.rs:169-170`); it is skipped unless the screen is `normal`.
- A paused stream learns of a lock when it resumes (fact 13); the invariant says so.
- `hstate` carries the kind of secure screen, so the viewer does not call a UAC prompt a lock
  screen.
- `hstate` and the result message are sent only to a viewer that advertises them, as CLAUDE.md
  requires, instead of relying on old viewers ignoring them.
- The pre-login password hash is enough to answer the challenge
  (`pans/00-OVERVIEW-AND-PROTOCOL.md:132-138`), so it is a secret and belongs in the OS secret
  store at machine scope, not in an access-controlled file or key (S6).
- Small contracts added in 10a: how the agent gets its control handles; an agent started by hand
  refuses to run; the setting toggled while attached; logs carry no payload and are not truncated
  at each start; the agent is terminated at system shutdown without the 5 s wait; no COM
  single-threaded apartment on the agent's threads.

### Not changed

- Each change of source forces a keyframe outside the one-per-second limit
  (`media/host.rs:819` against `:727`), so a UAC prompt costs two. Kept; U5's checklist watches
  the bitrate.
- Starting the host at raised integrity was suggested as a defence. Not adopted: the review
  itself notes it does not close the ways code gets loaded into the host, and §3.2 no longer
  depends on the host being trustworthy.

## 8. Unverified: to read and cite in step S

Each of these is used by the design and comes from memory or from `pans/04` §8b, not from a
documentation read.

| Claim | Where it matters |
|---|---|
| The lock screen, the sign-in screen and UAC prompts run on the `Winlogon` desktop, and only SYSTEM may open it | The whole design |
| Desktop Duplication works on the secure desktop only for a LocalSystem process in that session whose thread is attached to the input desktop | U3.0 |
| Desktop Duplication with the display asleep: delivers nothing, or fails | U3.0, 10a §5.4 |
| The hardware encoder can be created and keeps working while the Winlogon desktop has input | U3.0 |
| Setting `TokenSessionId` on a duplicate of a LocalSystem token needs SeTcbPrivilege and yields a SYSTEM process in the user's session | U3 |
| A process in a user's session can be assigned to a job object created in session 0 | U3.0 |
| A medium-integrity process cannot open a SYSTEM process or its token | 10a §3.4 (the design no longer needs it either way) |
| Synchronous pipe I/O on one handle is serialised across threads | 10a §3.8 (the design uses overlapped I/O either way) |
| An unnamed section's handle can be re-duplicated with more access by its holder | 10a §3.4 (the agent never reads the section) |
| An unlabelled pipe is treated as medium integrity; an explicit label keeps low-integrity processes out | U3 |
| Whether a session is locked can be read by a SYSTEM process (`WTSQuerySessionInformation`), reliably at the moment the desktop switches | U3, U5 |
| `SendSAS(FALSE)` works from a LocalSystem service and is governed by the `SoftwareSASGeneration` policy, whose default forbids services; whether it also works from the agent | U4, D3 |
| `RegisterServiceCtrlHandlerExW` with `SERVICE_ACCEPT_SESSIONCHANGE` delivers lock, unlock, logon, logoff and console connect events | U2 |
| `OpenInputDesktop` in a disconnected session | U3 |
| `SetThreadDesktop` fails on a thread that owns windows or hooks | U3 (already relied on, `adapters.rs:254-257`) |
| A current Windows lock screen is dismissed by any key or click without Ctrl+Alt+Del unless policy says otherwise | U3, U4 |

Nothing in this plan counts as working until its device checklist passes (decision #15).
