# Mestream

[![CI](https://github.com/mestrovicfilip/mestream/actions/workflows/ci.yml/badge.svg)](https://github.com/mestrovicfilip/mestream/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/mestrovicfilip/mestream)](https://github.com/mestrovicfilip/mestream/releases/latest)

A small, fast Xbox remote play client for Windows handhelds. It streams your
own Xbox console to your device over your home network, with the least delay
the hardware allows.

Built for and tested on the **ASUS ROG Xbox Ally X**. It should run on the
other ROG Ally models and on any Windows 11 PC with an Xbox-compatible
controller, but those have not been tested.

> **Unofficial.** Mestream is not made by, endorsed by, or affiliated with
> Microsoft, Xbox or ASUS. It talks to the same undocumented services the
> official remote play clients use; they can change or stop working at any
> time. Use it with your own account and console, at your own risk.

## What it does

- 720p or 1080p at 60 fps, hardware-decoded, typically 2–3 ms from the moment
  a picture arrives to the moment it is on screen
- its own scaling and sharpening, with sharpness, saturation and contrast
  adjustable while you play
- controller input sent the moment it happens, vibration included
- sound that waits only as long as the network requires, typically about 20 ms
- reconnects by itself after the stream drops or the device wakes from sleep
- everything on screen works by touch, controller, keyboard and mouse

## Before you start

On the console:

1. **Settings > Devices & connections > Remote features**: turn on *Enable
   remote features*.
2. **Settings > General > Power options**: choose *Sleep*, so the console can
   be woken up remotely.
3. Sign in on the console with the Microsoft account you will use in Mestream.

On the device: Windows 11 (64-bit; Windows 10 may work but is untested) and a
controller Windows sees as an Xbox controller. The built-in controls of the ROG Ally family qualify.

## Install

Download the latest zip from the
[Releases page](https://github.com/mestrovicfilip/mestream/releases/latest), unpack it
anywhere, and run
`mestream.exe`. Windows SmartScreen will warn about an unknown publisher,
because the program is not code-signed: choose *More info > Run anyway*, or
build it yourself (below).

## First run

Mestream opens full screen and walks you through it:

1. **Sign in.** It shows a code and a QR code. Open `microsoft.com/link` on
   your phone or computer, enter the code and approve. Mestream never sees
   your password; it stores only a sign-in token, encrypted for your Windows
   user.
2. **Choose a console**, if the account has more than one.
3. It connects, and you are playing.

Next time it goes straight to the stream.

## Controls

| Action | Touch / mouse | Controller | Keyboard |
| --- | --- | --- | --- |
| Open the settings menu | tap the picture | hold both sticks down | Esc |
| Move in the menu | tap | d-pad or left stick | arrow keys |
| Switch tab | tap the tab | LB / RB | Tab, Page Up / Down |
| Choose | tap | A | Enter |
| Close the menu | tap ✕ or the picture | B | Esc |
| Xbox button | – | View + Menu together | – |

Windows keeps the real Xbox button for itself, which is why View + Menu
stands in for it.

## Settings

| Tab | Setting | Notes |
| --- | --- | --- |
| Picture | Sharpness, saturation, contrast | applied at once |
| Stream | Resolution | 720p (default) or 1080p; reconnects when the menu closes |
| | Bitrate limit | 2–40 Mbit/s; reconnects when the menu closes |
| | Display sync | V-Sync (no tearing) or Low latency (may tear without VRR) |
| | Smoothing | the most delay Mestream may add to even out network jitter |
| Device | Volume, vibration, statistics | statistics show fps, bitrate and delay |
| Console | Switch console, restart console, sign out | |

Settings are kept in `%USERPROFILE%\.mestream\settings.json`.

## If something goes wrong

The failure screen says what happened and offers *Try again*, *Copy error*
(a report you can paste into an issue) and, when it would help, *Restart
console*.

- **"The Xbox service cannot reach the console"**: the console is fully shut
  down, offline, or its link to the Xbox service is stuck. Restart the console.
- **No console found**: remote features are off, or the console is signed in
  to a different account.
- **Stutter**: lower the bitrate, use a wired connection for the console, and
  turn on *Statistics* to see what the network is doing.

From a terminal, `mestream diagnose` prints what the Xbox service knows about
your consoles, and `mestream help` lists every command.

## Build it yourself

You need [Rust](https://rustup.rs) with the MSVC toolchain (the right version
is picked up from `rust-toolchain.toml`), and
[CMake](https://cmake.org/download/) on your `PATH` (it builds the Opus audio
decoder). The Visual Studio Build Tools include CMake as the optional
"C++ CMake tools" component; `assets\package.ps1` finds that copy by itself.

```powershell
cargo build --release
```

```powershell
cargo test
```

The program is `target\release\mestream.exe`. `assets\package.ps1` builds it
and makes the release zip; `assets\make-shortcut.ps1` puts a shortcut on the
desktop.

## Translating

All on-screen text lives in `src/text/en.rs`. To add a language, copy that
file, translate the right-hand sides and register it in `src/text/mod.rs`.

## What is stored and sent

- `%USERPROFILE%\.mestream\auth.bin`: the Microsoft sign-in token, encrypted
  with Windows DPAPI so only your Windows user on this machine can read it.
- `settings.json` and, while a stream is open, `session.json` in the same folder.
- Mestream talks only to Microsoft and Xbox services and to your console.
  There is no telemetry.

`mestream logout`, or *Sign out* in the menu, removes the sign-in.

## Author

Made by Filip Meštrović of [MestApps](https://www.mestapps.com).

## Thanks

The remote play protocol was understood with the help of open-source projects
that came first, in particular
[Greenlight](https://github.com/unknownskl/greenlight),
[xal-node](https://github.com/unknownskl/xal-node) and
[XStreaming](https://github.com/Geocld/XStreaming).

## Licence

Copyright © 2026 Filip Meštrović, [MestApps](https://www.mestapps.com).

Mestream is free software under the GNU Affero General Public License,
version 3: you may use, change and share it, and anything you distribute that
is built on it must be shared under the same licence. See [LICENSE](LICENSE).
It comes with no warranty.
