# Kadr

Screenshots and screen recording at up to 120 fps for Windows 11, as a replacement for the Snipping
Tool. Written in Rust: one small exe with no runtime dependencies.

- A double `Print` opens the capture, so an accidental press does nothing
- Area, window or whole screen, as a picture or a video with system sound and microphone
- An editor that outlines the object under the mouse with one click
- Text from the screen, color picker, delayed capture
- A player that trims videos and exports GIFs

The interface is in Russian. Kadr is an independent project, not affiliated with Microsoft.

## Install

Download `Kadr-Setup-*.exe` from [Releases](https://github.com/devatiywork/kadr/releases) and run it.
It installs for the current user without administrator rights, adds Kadr to the Start menu, turns on
autostart and starts the app. Running a newer setup updates the app and keeps your settings.

The setup is not signed, so Windows SmartScreen may warn about it: choose **More info**, then
**Run anyway**. You can also [build it yourself](#build).

To remove Kadr, use **Settings > Apps > Installed apps**. Screenshots, recordings and the settings in
`%APPDATA%\Kadr` stay on the computer.

Kadr does not remove the Snipping Tool. While Kadr runs, it takes over `Print` and `Win+Shift+S`.

## Usage

| Action | Result |
| --- | --- |
| `Print` once | Nothing, so an accidental press does not interrupt you |
| `Print` twice | Opens the capture overlay (also `Win+Shift+S` or a click on the tray icon) |
| Drag in the overlay | Selects an area on any monitor, whichever shape is chosen |
| Whole screen or Window button | A picture is taken at once: the monitor with the toolbar or the active window |
| In video mode | A drag, Window or Whole screen only selects what to record; a click picks another window or monitor. The Start button or `Enter` starts the recording |
| `Print` once more, or `Enter`, in the overlay | Captures the whole monitor under the cursor |
| `Esc` or right click in the overlay | Cancel |
| `Print` twice while recording | Stops the recording |

`Print` pressed twice is the default hotkey; both the key and the number of presses can be changed.
While Kadr runs, plain `Print` never opens the capture tool built into Windows, whichever hotkey
is chosen. `Alt+Print` and `Win+Print` keep working as usual.

After a capture Windows shows a notification. A screenshot is already on the clipboard and
saved to the system Screenshots folder; clicking the notification opens the editor. A recording
is saved to `Videos\Записи экрана` and put on the clipboard as a file, ready to paste into a folder
or a chat; clicking the notification opens it in the player.

The overlay also has these modes:

- **Text:** select an area, and its text goes to the clipboard. Works with the recognition languages
  installed in Windows.
- **Color picker:** click anywhere to copy that color as HEX, RGB or HSL.
- **Delay:** the clock button waits 3, 5 or 10 seconds before freezing the screen, to catch menus
  and tooltips. Clicking the small countdown cancels it.

### Editor

The default **Frame** tool (`R`) is the quick way to mark something: hover to preview, click to
outline the object under the mouse, or drag to draw a frame by hand.

Other tools: square `Q`, oval `E`, line `L`, arrow `A` (Shift gives equal sides and 45 degree steps),
marker `H`, pen `P`, eraser `X`, text `T`, mosaic `B`, crop `C`, move and delete `V`
(`Del` removes the selected item). The toolbar also erases all annotations and copies the text of
the picture.

`Ctrl+Z` / `Ctrl+Y` undo and redo, `Ctrl+C` copy, `Ctrl+S` save as, `Ctrl+wheel` zoom,
`Ctrl+0` fit to window. Every edit is copied to the clipboard right away.

### Recording

H.264 at a constant frame rate with AAC audio, encoded on the GPU. System sound and microphone
are switched in the overlay toolbar. The control panel has a microphone button that works at any
moment, even if the recording began without one; a switched off microphone is released, so Windows
no longer shows it as in use. The frame and control panel do not appear in the video.
A video covers one monitor.

### Player

Drag the two grips on the timeline to choose a piece, or press `I` and `O` at the current position.
Save it as a new video, or as a GIF of up to 30 seconds in high quality (25 frames per second, up to
1280 pixels wide) or as a small file (12 frames per second, up to 640 pixels wide). The original stays
untouched. `Space` plays and pauses, the arrows step by a second (5 with `Shift`), `M` mutes.

### Settings

Open them from the tray menu or the gear in the overlay toolbar. Changes apply immediately.

| Group | Options |
| --- | --- |
| Video | Quality, 30/60/120 fps, cursor on video, countdown, folder |
| Sound | System sound and microphone on or off, the device for each, microphone level check |
| Screenshots | Autosave, folder, open the editor right after a capture, color format of the picker |
| General | Hotkey, one or two presses, start with Windows |

A chosen sound device that is no longer connected falls back to the default one in Windows.
Keys used for typing can be a hotkey only together with Ctrl, Alt or Win.

## Build

Needs Rust 1.85 or newer on Windows.

```bash
cargo build --release
```

The result is `target\release\kadr.exe`, which runs as it is. To make the setup, copy it under a name
that starts with `Kadr-Setup`, for example `Kadr-Setup-0.1.0.exe`: under such a name the same program
installs itself into `%LOCALAPPDATA%\Programs\Kadr`, and `kadr.exe --uninstall` removes it again.

To update an installed copy while developing, without any questions:

```bash
powershell -ExecutionPolicy Bypass -File install.ps1
```

Settings and the error log are in `%APPDATA%\Kadr`.

## Tests

```bash
cargo test
```

```bash
cargo test records_ -- --ignored --nocapture
```

The second command records the real screen for 3 seconds. `settings_matrix`, `lists_audio_devices`
and `microphone_level` are further ignored tests that use real hardware.

## Layout

| File | Purpose |
| --- | --- |
| `app.rs` | Tray icon, key press routing, what happens after a capture |
| `hook.rs` | Global keyboard hook and double press detection |
| `overlay.rs` | Full-screen selection layer |
| `editor.rs` | Editor window and annotations |
| `smart.rs` | Finding the object under the mouse in screenshot pixels |
| `record.rs` | Windows.Graphics.Capture + WASAPI + Media Foundation pipeline |
| `recui.rs` | Countdown, region frame and control panel |
| `player.rs` | Player window with the timeline |
| `videoedit.rs` | Saving a piece of a video and GIF export |
| `ocr.rs` | Text recognition |
| `delay.rs` | Delayed capture |
| `color.rs` | Text forms of a color |
| `prefs.rs` | Settings window |
| `setup.rs` | Installing and removing the app |
| `notify.rs` | Windows notifications and `kadr:` links |
| `capture.rs`, `clipboard.rs`, `image.rs`, `gfx.rs`, `geom.rs`, `win.rs`, `paths.rs`, `settings.rs` | Helpers |

`kadr:` links can be opened by any website, so they carry only a bare file name that is looked
up in Kadr's own folders, never a path.

## License

[MIT](LICENSE)
