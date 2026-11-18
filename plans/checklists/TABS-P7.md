# TABS-P7 — Ctrl+Tab and its neighbours reach the remote: tester checklist

What this proves: while a tab's session is on screen with control, the remote's own tab
switching works — Ctrl+Tab, Ctrl+Shift+Tab, Ctrl+PageUp / PageDown go to the remote (its browser
or editor switches tabs), not to this app — and Alt+Tab still does what it did. This app's own tab
switch (Ctrl+Alt+PageUp / PageDown, Cmd+Option on macOS) stays with this app.

Root cause found in the code (Windows): only the video window forwards keys, and the web view
took these chords whenever it had the keyboard (after a click on the tab strip, the session bar or
a dialog). The keyboard hook now takes them too, with their modifiers, while the app is in front
and capture is on; and a tab takes the keyboard when it is shown (P3). Unit tests:
`crates/input/src/keyhook.rs` (which chords are taken, left/right modifiers). On macOS and Linux
the video takes every key while it has the keyboard (the macOS key monitor, the GTK toplevel's
key handler), so these chords are forwarded there already; they are on this list to confirm it.

## Before you start

* A viewer on the OS under test, a host (any OS) with a browser open on three tabs.
* Settings → "Capture system shortcuts" on, for steps 1–4; off for step 5.

## Steps (every viewer OS)

1. Connect with control. Click into the video, press Ctrl+Tab and Ctrl+Shift+Tab: the host's
   browser switches tabs forward and back. Ctrl+PageDown / PageUp do the same.
2. Click this app's tab strip (the web page now has the keyboard) — without clicking back into
   the video — and repeat step 1 (Windows: this is the case that failed before P7). The keys
   still reach the host.
3. Alt+Tab (Cmd+Tab on macOS) still switches the host's windows; Win/Super still opens the host's
   start menu; release everything — no modifier is left held on the host (type a letter: it is
   not upper-case or a shortcut).
4. Ctrl+Alt+PageDown / PageUp (Cmd+Option on macOS) switches this app's tabs and never reaches
   the host.
5. Capture off: with the video holding the keyboard (click into it), Ctrl+Tab still reaches the
   host.

## Record

| OS | Build | Steps passed | Notes |
|---|---|---|---|
| Windows | | | |
| macOS | | | |
| Linux X11 | | | |
| Linux Wayland | | | |
