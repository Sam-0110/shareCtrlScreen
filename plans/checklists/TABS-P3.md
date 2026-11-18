# TABS-P3 — several video surfaces in one window: tester checklist

What this proves: with two remote sessions open in one app window, only the session on
screen (the **active** tab) shows its video, gets keyboard, mouse and clipboard, and holds
shortcut capture (Alt+Tab / Cmd+Tab). Switching back and forth never shows two videos, never
leaves the wrong one on top, and never sends a key or a copy to the hidden session. Closing a
session frees its native surface. Plan: `plans/08-MULTI-SESSION-TABS.md` P3, structure:
`plans/08a-STRUCTURE-AND-FLOWS.md` §6.4, §6.5, §6.18.

**When to run it.** The app UI can open a second session only once P4 (the tab bar) lands, so
this checklist runs on the P4 build, on every OS, before P4 is called done. Until then the
paths below are unverified on hardware.

What **is** covered on every OS by `cargo test`:
* the ActiveGate's rule (shown = active ∧ ready ∧ ¬trouble ∧ ¬overlay ∧ ¬ended) and the
  switch order (old hidden before new shown) — `crates/engine/src/session/active.rs`;
* two live sessions against two hosts, 20 switches: only the active one is visible and
  presented, the hidden one stops presenting; a copy reaches only the active tab's host and
  a copy made while another tab was active is never sent on activation —
  `crates/engine/tests/loopback.rs` (`one_viewer_holds_two_sessions_and_ends_them_separately`);
* the clipboard re-baseline while gated off — `crates/engine/src/session/clipboard.rs`;
* the GTK top-inset layout math — `crates/platform-linux/src/surface/gtk_host.rs`.

## Before you start

* One viewer machine (the OS under test) and **two** host machines (any OS), each running
  the build under test, in approve mode with clipboard sync on.
* On the viewer, a text editor to copy from and paste into.

## Steps (per viewer OS: Windows, macOS, Linux X11, Linux Wayland — GNOME and sway)

1. Connect to host A, then to host B. B's tab is on screen. Only B's video shows; A's is
   not visible anywhere (not behind, not flickering at the edges).
2. Switch tabs 20 times quickly (click and Ctrl+Alt+PageUp/Down). After each switch exactly
   one video is visible, and it is the selected tab's. The tab bar stays visible and
   clickable above the video at all times (the top inset).
3. On the active tab, type into a remote text field without clicking the video first: the
   keys arrive (the shown video takes focus). Switch tabs and type again: the keys go only to
   the newly active host.
4. Copy text on the viewer while A is active: it pastes on host A. Switch to B: host B's
   clipboard does **not** receive that text. Copy new text: it pastes on host B only.
5. Shortcut capture: with control on, Alt+Tab (Cmd+Tab on macOS; Super/compositor shortcuts
   on Wayland) acts on the active host only. Switch tabs: it now acts on the other host. On
   Wayland GNOME, the permission prompt appears at most once per session.
6. Drag a file onto the video: it lands on the active tab's host only.
7. Open Settings (and a confirm dialog) while a session is on screen: the dialog is fully
   visible and typeable, no video over it; close it: the same tab's video returns and
   repaints even if the remote screen is static.
8. Close the hidden tab, then the active one. Windows: Spy++ (or `Get-Process` window count)
   shows no leftover `ShareCtrlVideoWindow` child. macOS / Linux: no stray video area remains;
   reopening and closing ten times does not grow the process's window/handle count.
9. Pull the network of host A while B is active: A's tab reports trouble; B keeps playing
   and B's video is never hidden by A's trouble.

## Record

| OS | Build | Steps passed | Notes |
|---|---|---|---|
| Windows | | | |
| macOS | | | |
| Linux X11 | | | |
| Linux Wayland (GNOME) | | | |
| Linux Wayland (sway) | | | |

## P6 — background tabs pause (same build)

10. Open three tabs to three hosts, leave one on screen. On each background host the log
    shows `viewer paused the main stream (background tab)` and its upload drops to about
    0 kbps for video (Task Manager / `nethogs`); the active host streams normally.
11. Switch to a background tab: a picture appears within about a second
    (`viewer resumed the main stream` in that host's log), and the tab you left pauses.
12. With a host on an older version (no `vpause`): switching still works and its video keeps
    flowing while in the background (the tab decodes but does not draw it).
13. Background a tab while its host screen is idle (nothing moving), wait a minute, switch
    back: the picture is right at once, not black or stale.
