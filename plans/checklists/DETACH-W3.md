# DETACH-W3 — detached tab windows: tester checklist

What this proves: a tab can leave the strip for a window of its own and come back, live,
without reconnecting; several such windows stream at the same time; the window you are in has
the keyboard, the clipboard and shortcut capture; a minimised window pauses; X on a window
ends that session and closes its tab; the windows come back where they were at the next
launch. Plan: `plans/09-DETACHABLE-TABS.md` W3, structure: `plans/09a-STRUCTURE-AND-FLOWS.md`
§4.1, §5.1–§5.7.

What **is** covered on every OS by `cargo test`:
* several sessions on screen, one focused; hooks only for the focused, on-screen, uncovered
  one — `crates/engine/src/session/{active.rs,mod.rs}`, loopback
  `several_sessions_on_screen_one_focused`;
* a live session moved between surfaces 20 times with no reconnect, a failed move, a move
  while connecting, a move with no video — loopback `a_live_session_moves_between_surfaces`,
  `a_failed_move_stays_and_an_early_move_goes_live_there`, `a_session_without_video_still_moves`;
* the synced schema, its merge and the placement maths — `crates/protocol/src/sync/{tabs.rs,
  placement.rs}`, `config/workspace.rs`, the shared vectors (Rust and `server/`);
* Windows: minimise / restore / destroy reports of the window watch —
  `crates/platform-windows/src/winwatch.rs`;
* which sessions are on screen and focused, from the windows' states — `app/src/tabs.rs`
  (`the_screen_follows_the_windows`).

**Not covered without hardware:** everything below. The macOS window watch is compiled only by
CI; macOS and Linux minimise / activation events, Windows DPI moves, and the move's cost on real
GPUs are unverified until this passes.

## Before you start

* One viewer (the OS under test) and two hosts (any OS), each on the build under test, in
  approve mode, clipboard sync and shortcut capture on.
* Two monitors on the viewer for steps 9–10, if available.

## Steps (per viewer OS: Windows, macOS, Linux X11, Linux Wayland — GNOME and sway)

1. Connect to both hosts (two tabs). On tab A press **Detach** (session bar) or ⧉ (strip).
   Expect: A opens in its own window, live, with no reconnect (the host shows no new prompt);
   the strip now shows only B, which becomes the tab on screen and streams.
2. Move the mouse over both videos: both update live at the same time.
3. Click into A's window and type: keys reach host A only. Click into the main window's video
   and type: keys reach host B only. Copy text on the viewer after each click: it reaches only
   the host whose window you were in.
4. With A's window focused, press Alt+Tab (Cmd+Tab): it goes to host A. Click the main window:
   Alt+Tab goes to host B.
5. Click another app, copy text there: it reaches the host of the window you used last.
6. Minimise A's window: host A's video pauses (its CPU drops). Restore it: the picture is back
   at once (no black frame), and live. *Wayland: minimise is not reported; A keeps streaming.*
7. Minimise the main window: B pauses; A keeps streaming.
8. In A's window press **Dock**: A goes back to the end of the strip as the tab on screen, live
   without reconnecting; B pauses; A's window closes.
9. Detach A again, move its window to the second monitor, make it about half that screen, wait
   a second. Quit the app and start it again: A's window opens on that monitor, the same share
   of it, disconnected, with **Connect**. Maximise it, restart: it opens maximised.
10. Sign in to the same account on a second computer with a different screen size: A's window
    opens there too, scaled to that screen. *(Sync of placement needs W4 and the server.)*
11. In A's window press X: session A ends (host A sees it end), A's tab is gone from the strip,
    and on the second computer it is closed too after a sync.
12. Detach a tab that is still **connecting** or asking for a **password**: the card moves into
    the window; finishing the connection there goes live in that window, not the main one.
13. Settings from A's window: only A's video hides under the dialog; B keeps showing.
14. Close the main window (or Quit): every window closes and the sessions end. *Windows service
    host:* closing hides the main window and A's window to the tray; Open brings both back.
15. Windows only: move A's window between monitors with different scaling: the picture stays
    sharp, input lands where you click.

Record OS, build and pass / fail per step in the PR.
