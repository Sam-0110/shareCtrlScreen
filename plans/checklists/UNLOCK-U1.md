# UNLOCK-U1 — The viewer is told what the host's screen is doing (plans/10 U1)

Device-only. The loopback suite proves the wire, the engine and the viewer's hiding with a mock
capture (`crates/engine/tests/loopback.rs`: `a_locked_host_screen_is_explained_and_comes_back_by_itself`,
`a_frame_from_before_the_lock_is_never_shown_again`,
`connecting_to_an_already_locked_host_explains_the_missing_picture`). What only a real Windows
host can show: that `OpenInputDesktop` is refused with *access denied* while the lock screen or a
UAC prompt is up (the one signal `DdaCapture` reports as `Blocked`), that it is **not** refused
in a disconnected session, and that the picture returns by itself after a local unlock.

Two machines: **B** the viewer (any OS, this version), **H** the Windows host (this version,
installed, so the service launches the host; repeat §2 once with a host started by hand). H in
**password** mode, so B can connect while nobody is at H to approve (plans/10 §2 fact 14). The
host log is `%APPDATA%\ShareCtrlScreen\app.log` on H; the viewer log is B's `app.log` (on
Windows the same path, on Linux `~/.local/state/sharectrl/app.log`, native/README.md).

Nothing here may show a key, a typed character or a pointer position in either log (K4).

## 1. Locking during a session (acceptance)
1. From B, connect to H with control. The picture is live.
2. On H press Win+L.
   - [ ] Within **2 s** B's tab shows "The remote computer is locked" (with the text about a
     security prompt) instead of a black or frozen picture. The session bar stays; Disconnect
     works from the card.
   - [ ] H's log has **one** `capture: the input desktop refused this process (Access is
     denied. (0x80070005))` line and **one** `host: the screen is now Blocked(NotSupported)` —
     not one per 250 ms retry.
   - [ ] B's log has one `viewer: the host's screen is blocked`.
3. On B move the mouse over the card and type a few keys.
   - [ ] H's log has at most one `input: the input desktop refused this process …` line for
     the whole lock, never one per event.
4. Unlock H locally.
   - [ ] Within about a second B shows the live picture again, without reconnecting.
   - [ ] H's log: `capture: the input desktop is reachable again`, `host: the screen is now
     Normal`, `host: screen capture re-acquired after …`; B's: `viewer: the host's screen is
     normal`.
   - [ ] The first picture B shows after the unlock is the unlocked desktop, never the picture
     from before the lock (K21). Best seen with a window moved or closed on H while it was
     locked (moved by someone at H before locking is not enough: lock, then unlock and check).
5. [ ] Repeat steps 2–4 once more: it is reliable, not a one-shot.

## 2. Connecting to a host that is already locked
1. Lock H (Win+L). Wait for its screen to show the lock screen.
2. From B, connect with H's password.
   - [ ] Within **2 s** of the connection opening B shows "The remote computer is locked" —
     not a black rectangle — although no frame ever arrived.
   - [ ] H's log: `host: screen capture not ready yet …` once, then nothing per retry.
3. Unlock H locally.
   - [ ] The picture appears on B by itself.

## 3. A UAC prompt and Ctrl+Alt+Del
1. During a session, start an elevated program on H (Run as administrator) so a UAC prompt
   shows on the secure desktop.
   - [ ] B shows the same message (before plans/10 U3 the host cannot tell a prompt from the
     lock screen; the text names both).
   - [ ] Answering the prompt on H brings the picture back by itself.
2. Press Ctrl+Alt+Del on H, then Cancel.
   - [ ] Same: message while it is up, picture after.

## 4. Pop-outs and background tabs
1. With control, open a second monitor of H in a pop-out window on B (H needs two monitors).
2. Lock H.
   - [ ] The pop-out window shows nothing (hidden) while the main tab shows the message; no
     frozen picture of the second monitor is left on screen.
   - [ ] After the unlock the pop-out shows a live picture again (it may take a second).
3. Open a second tab on B (another computer, or the new-tab page) so H's tab is in the
   background. Lock H, wait 5 s, unlock H, wait 5 s, switch back to H's tab.
   - [ ] H's tab shows the live picture (a paused stream notices the lock only when it resumes,
     10a §5.14; it must not show a stale picture or a stuck message).
4. Detach H's tab into its own window (⧉) and repeat §1 steps 2–4 there.
   - [ ] The detached window shows the message and then the picture, like the main window.

## 5. Not a lock: no false message
1. With H connected through Remote Desktop from a third machine, disconnect that RDP session
   (do not sign out) while B is connected to H's console session; or, simpler, let H's display
   go to sleep while unlocked.
   - [ ] B never shows the locked message for a disconnected session or a sleeping display
     (`OpenInputDesktop` succeeds then; only *access denied* is reported). Write down what B
     shows instead — a frozen picture during display sleep is today's behaviour.
2. [ ] A Windows 10 host, a Windows 11 host: §1 passes on both.

## 6. Dropped files and Settings
1. On B drop a large file (a few hundred MB) on a window in H's picture and lock H while it
   is still being sent.
   - [ ] When it finishes, H's log says `file left on the clipboard, not pasted`; nothing was
     typed into the lock screen's password box. After the unlock the file is on H's clipboard.
   - [ ] Unlocked, a dropped file is pasted into the window under the drop point as before.
2. On H open Settings.
   - [ ] Under "Start with Windows" the note "After a restart, someone must sign in on this
     machine before you can connect to it." is shown (`login_screen_supported` is false until
     plans/10 U6).

## 7. Old peers
1. B on this version, H on v2.3.3: lock H.
   - [ ] B behaves as before this change (black or frozen picture, no message, no error).
2. B on v2.3.3, H on this version: lock H.
   - [ ] B behaves as before; H's log has no error about an unknown capability.
