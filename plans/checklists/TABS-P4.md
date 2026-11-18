# TABS-P4 — the tab strip: tester checklist

What this proves: several remote computers open as tabs in one window, each with its own
connection, password prompt and name; the strip survives a restart and follows the account;
closing the app disconnects everything. Plan: `plans/08-MULTI-SESSION-TABS.md` P4, structure
`plans/08a-STRUCTURE-AND-FLOWS.md` §4–§6. Run it together with `TABS-P3.md` (several surfaces in
one window) and its P6 steps, on every OS.

What the unit and loopback tests cover: the strip's order keys (`protocol/src/sync/order.rs`),
opening, closing, reordering and the active tab in the workspace
(`protocol/src/config/workspace.rs`), the tab-switch chords taken out of the key stream
(`engine/src/modifiers.rs`), and the engine's multi-session behaviour
(`engine/tests/loopback.rs`). The TabManager and the web UI need the app.

## Steps

1. Connect to host A from the connect page: a tab "A" appears and shows A. Press **+**, connect
   to B, then to C. Three tabs; the strip stays visible above the video.
2. Double-click a tab's name, rename it, Enter: the tab, the recents chip and the connect box
   show the new name. Drag a tab before another: the order changes.
3. Switch by clicking and by Ctrl+Alt+PageDown / PageUp (Cmd+Option on macOS) — inside the
   video and on a disconnected tab's card. The chord never reaches the remote.
4. Disconnect one tab (session bar → Disconnect): its card says it is not connected, with
   Connect and Close tab. Close another with ×: its neighbour is shown.
5. Quit the app (window close; ⌘Q on macOS): every host's screen shows the session ended at
   once (not after a timeout). Relaunch: the open tabs return, disconnected, with their names
   and order, and the last one shown is selected. Nothing connects by itself.
6. **Connect all** brings the disconnected tabs up one by one.
7. Open a 9th connected tab: a confirm asks first; Cancel leaves 8.
8. Sign in to account B: none of the local or account A tabs show. Back to A: its tabs return.
9. Eight tabs to eight password-protected hosts: each prompt shows inside its own tab; type each
   once with "Remember". Restart, **Connect all**: all eight come up without a prompt, and each
   card shows "Signs in automatically" with **Forget saved password**.
10. Pop-outs: open a pop-out on tab A, switch to tab B and open one there: both stay open; closing
    tab A closes only A's.
11. While being viewed by another computer, connecting a tab is refused with the reason.
12. **P5, two computers on one account.** On laptop A open 3 named tabs; sign in on desktop B:
    the same 3 tabs appear, disconnected. Rename one on B: A shows it after its next sync. Close
    one on A: it disappears on B — unless it is connected on B, where it stays until it is
    disconnected, then goes. Take B offline for a week (or set its clock back): the closed tab
    does not come back.
13. Hold Ctrl+Alt+PageDown (Cmd+Option on macOS) inside a video: the tabs keep stepping and no
    host ever receives a PageDown. One press steps exactly one tab (the menu and the page do not
    step it twice). On a keyboard with AltGr (Windows), note whether AltGr+PageDown switches
    tabs.
14. Quit with ⌘Q (macOS) / the window's close (all OSes) while connected: each host sees the
    session end at once.

## Record

| OS | Build | Steps passed | Notes |
|---|---|---|---|
| Windows | | | |
| macOS | | | |
| Linux X11 | | | |
| Linux Wayland | | | |
