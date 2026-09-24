---
name: shared-monitor
description: Show something from this computer on the Dell monitor it shares with other computers, using the delldisplay tools (display_state, display_arrange, display_restore). Use when the user asks to put something on screen, show it on the monitor, split the screen, or switch the monitor to this computer, and when work running in the background has a result worth showing the user on the shared monitor.
---

# Shared monitor

This computer shares a Dell monitor with other computers. The user may be
looking at it right now while working on another one, so a change here
interrupts them. Decide whether a result is worth that the way a colleague
would before tapping them on the shoulder.

## Tools

- `display_state`: read-only. The layout, what each pane shows, which input
  is this computer, the layouts and inputs available, and whether this
  computer has a change to put back.
- `display_arrange`: set a layout and what goes in each pane. In `panes`,
  `self` is this computer, `current` is whatever the main pane shows now,
  anything else is an input name from `display_state`. `dry_run: true`
  checks without writing.
- `display_restore`: put back what this computer changed. Refuses if anyone
  has changed the monitor since.

## Etiquette

1. Call `display_state` first, every time. Other computers and the
   monitor's buttons change it without telling you.
2. Never assume which computer the user is on. `display_state` says which
   input is this computer; the main pane is not necessarily the user's.
3. Prefer adding this computer beside what's showing (`side-by-side` with
   `{"right": "self"}`, or picture-in-picture) over replacing it. The server
   may refuse a takeover anyway.
4. Write `reason` for the user: it appears as a notification. Say what they
   will see ("Showing the test results you asked for"), not what you did.
5. Once this computer is on screen, say in your reply what to look at.
6. Call `display_restore` when the user is done with it, or when they tell
   you to put it back. If it refuses, someone changed the monitor; leave it.
7. A refusal or cooldown is an answer, not a transient error. Don't retry in
   a loop, wait out the cooldown in a busy loop, or work around the rules.
   Read `display_state` again, then tell the user what happened.

## Background runs

Permission prompts show on this computer, which the user may not be
looking at. A background agent needs the tools allowed up front:
`mcp__plugin_delldisplay_delldisplay__*` in `permissions.allow`. If a call
is denied, say so in the result rather than asking again.

The server's own rules (takeover, cooldown, notifications) are set in
`~/.config/delldisplay/mcp.toml`. Leave that file to the user.
