# OctoWoW Updater for Benilla

Keeps a [Benilla](https://github.com/zmarotrix/Benilla-OctoWoW) install's copy of the **OctoWoW
game files** up to date, then starts the game. It updates the OctoWoW content only; new versions
of Benilla itself come from the
[Benilla releases](https://github.com/zmarotrix/Benilla-OctoWoW/releases).

## The Benilla for OctoWoW family

| Project | What it does |
|---|---|
| [Benilla-OctoWoW](https://github.com/zmarotrix/Benilla-OctoWoW) | The game client, for Windows and Linux, with every OctoWoW client mod built in |
| [Benilla-OctoWoW-Updater](https://github.com/zmarotrix/Benilla-OctoWoW-Updater) | Downloads the OctoWoW game files and keeps them current, then starts the game |
| [Benilla-OctoWoW-Transfer](https://github.com/zmarotrix/Benilla-OctoWoW-Transfer) | Moves your settings, key bindings, macros and addons between a normal 1.12 client and Benilla, or between two Benilla installs |

## Use it

Put `benilla-launcher` (`benilla-launcher.exe` on Windows) in your Benilla folder, beside
`benilla`, and run it:

```
Benilla/
  benilla-launcher(.exe)   this updater: double-click it
  benilla(.exe)            the game, from a Benilla release
  OctoWoW/                 the game files: Data plus WoW.exe and 4 DLLs for the version check
  benilla-config/          your settings, addons and saved data; never touched here
  launcher/                the updater's notes (last synced release, download resume)
```

On a first start it offers to copy the game files from an OctoWoW install you already have
(that install is only read, never changed), or to download them from the official OctoWoW
torrent's mirror (about 9 GB). Every later start downloads only what changed, checked piece by
piece against the torrent's hashes. **Repair game files** re-checks every file. **Play** starts
Benilla on those files.

It writes only inside its own folder, and refuses to run from a normal OctoWoW install.

Without a window (a terminal, or Linux over SSH):

```sh
benilla-launcher --update                      # update the game files
benilla-launcher --update --download           # first install: download them
benilla-launcher --update --import "<install>" # first install: copy them from an install
```

### Future OctoWoW updates

The updater follows whatever OctoWoW publishes: it reads the current release torrent each start,
fetches only the files that changed, and removes files a new release dropped. Nothing about a
particular OctoWoW version is built in.

## Build

Rust from <https://rustup.rs>, then:

```sh
cargo build --release
```

On Linux, the window needs the usual X11 or Wayland libraries, and the folder picker uses the
XDG desktop portal.

Licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option. Not affiliated
with Blizzard Entertainment; it downloads the game files from OctoWoW's own servers and never
hosts them.
