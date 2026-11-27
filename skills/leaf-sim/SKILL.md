---
name: leaf-sim
description: Drive a booted iOS Simulator from the command line with leaf: read the screen as a short list of elements with refs (@e3), tap, type, scroll, wait and launch apps, without screenshots. Use when asked to open, use, test, click through or check an iPhone app or screen in a simulator, to reproduce a bug on iOS, or to fill in a form or walk a flow on an iPhone simulator. macOS with Xcode only.
---

# leaf-sim: drive an iPhone simulator

`leaf sim` reads the simulator's screen through iOS accessibility, the same tree VoiceOver uses. It prints one line per element, and each line has a ref you act on. A snapshot is a few hundred tokens and takes about a second. A screenshot costs thousands of tokens and you'd still have to guess coordinates, so take screenshots only when you need to see pixels.

It drives any booted iPhone simulator: one open in a Leaf pane (the person sees a ring where you touch), Simulator.app's, or one booted with `xcrun simctl boot`. With more than one booted, add `--device <udid>` to every command (`leaf sim list` shows the ids).

## The loop

1. `leaf sim snapshot -i`: see what you can act on.
2. Act on a ref: `leaf sim tap @e3`, `leaf sim fill @e7 "text"`.
3. `leaf sim snapshot -i --diff`: see only what changed.

Repeat. Don't sleep between steps. A snapshot waits for the screen to stop moving, and after an action it waits for the screen to react (up to 1.5 s).

```bash
leaf sim launch Settings          # by name or bundle id; returns once it's in front
leaf sim snapshot -i
# Settings 1206×2622px
# @e1 button "General"
# @e2 button "Accessibility"
# @e3 textfield = "Search"
leaf sim tap @e1
leaf sim snapshot -i --diff
# - @e2 button "Accessibility"
# + @e4 button "Settings"         ← the back button
# + @e5 button "About"
```

## Reading a snapshot

```
Settings 1206×2622px                     the app in front (blank on the home screen), screen size in pixels
@e1 heading "Settings"
@e2 button "General"
  @e9 text "Off"                         indented: inside the element above
@e3 textfield "Email" = "me@x.com"       = its current value
@e4 checkbox "Wi-Fi" = "1"               switches show as checkbox: 1 on, 0 off
@e5 button #save-button (disabled)       #id when there's no label
```

- `-i` keeps only what you can act on (buttons, fields, checkboxes, sliders…) and drops text, headings, images and groups. Leave it off to read the screen's words.
- `--diff` prints `-` and `+` lines against the last snapshot, `no changes` when nothing moved, or the whole snapshot when that's shorter (a new screen).
- A ref stays the same while its element is on screen, across snapshots, scrolling and typing. A ref from an element that left the screen fails with a message, and leaf never reuses its number, so take a new snapshot.
- Only what's on screen is listed. Scroll to find more.

## Acting

```bash
leaf sim tap @e3                  # middle of the element's visible part
leaf sim tap 600 1400             # or a point, in screenshot pixels
leaf sim fill @e3 "hi@example.com"   # tap, select all, delete, type
leaf sim type @e3 "more"          # tap, then type (keeps what's there)
leaf sim type "text"              # into whatever has focus
leaf sim key enter                # enter, escape, backspace, tab, space, up, down, left, right…
leaf sim key cmd+a                # chords: cmd, shift, ctrl, alt
leaf sim scroll down              # the whole screen; or inside one element: scroll down @e8
leaf sim swipe 600 1400 600 1400 1   # x1 y1 x2 y2 seconds; the same point held = long press
leaf sim button home              # home, lock, side, siri
leaf sim wait "Welcome" 15        # until an element shows the text (default 10 s); prints its line and ref
leaf sim apps                     # installed apps: yours first, then iOS's
leaf sim launch com.example.App   # or a name: launch Maps
leaf sim open-url myapp://item/42 # deep links and web pages
```

`type` uses a US keyboard. For other characters, `xcrun simctl pbcopy <udid>` the text, then long-press the field and tap Paste.

## When the tree isn't enough

- **Games, maps, canvases, web views without labels**: elements may be missing or unlabeled. Take `leaf sim screenshot`, read the PNG, and tap by pixel. Snapshot sizes and screenshot pixels are the same coordinates.
- **System alerts** (permission prompts) belong to iOS, not the app, and may not be in the snapshot. If the screen seems stuck, take a screenshot. You can grant permissions up front with `xcrun simctl privacy <udid> grant <service> <bundle id>`.
- **An app you just terminated** sometimes launches in the background. `launch` says so; tap its icon on the home screen instead.
- **The iPhone Duo** has two screens, and only its Leaf pane knows which one faces front, so taps and swipes need it open in a pane. Everything else works without one.
- `leaf sim info` prints the device, its udid and its screen size.

## Your own app

Build and install as usual, then drive it:

```bash
xcodebuild -scheme MyApp -destination "id=<udid>" -derivedDataPath build build
xcrun simctl install <udid> build/Build/Products/Debug-iphonesimulator/MyApp.app
leaf sim launch MyApp
```

Give controls accessibility identifiers (`.accessibilityIdentifier("save")` in SwiftUI). They show up as `#save` and keep refs stable when labels change. Missing labels in a snapshot are also missing for VoiceOver users, so they're worth reporting.

To show the person the simulator on a 3D iPhone in a Rex pane, see the `leaf` skill (`leaf sim open`).
