# JRiver Cast Bridge for Jellyfin

JRiver Cast Bridge makes **JRiver Media Center** on a Windows HTPC a **Jellyfin cast target**. You browse your Jellyfin library in any Jellyfin app and press play on "JRiver (HTPC)". JRiver Media Center then plays the original file straight from your file share, with everything JRiver does well: JRVR video processing, Dolby Vision (including FEL), and bitstreamed TrueHD Atmos or DTS:X to the receiver. Playback position, resume points and watched status go back to Jellyfin as if a Jellyfin player had played the film.

The bridge never streams and never transcodes: JRiver always opens the original file.

## Intended use

The main setup is a home-theatre PC running both JRiver Media Center and **Jellyfin Media Player** (JMP):

- JMP's 10-foot interface on the TV is used to browse the library, with a remote or mouse and keyboard.
- Playback is cast to the bridge, so JRiver plays the film full screen.
- The bridge starts JMP itself at logon, after it has registered with Jellyfin, so JMP finds the bridge and keeps it as its default cast target.
- When the film ends or is stopped, the bridge minimizes JRiver, so JMP is in front again.

Casting from the Jellyfin web app or the phone apps works the same way: pick "JRiver (HTPC)" in the cast menu.

## Requirements

- **Windows** with **JRiver Media Center 36** with Media Network turned on (Options → Media Network): the bridge drives Media Center through its MCWS web service.
- **Jellyfin 10.11** or later, and a Jellyfin user for the bridge.
- **Your media on a file share the HTPC can open** (for example `\\nas\movies`), the same files your Jellyfin server sees under its own paths (for example `/media/movies`).

## Install

1. Run `jriver-cast-bridge-setup-<version>.exe`. It installs for the current Windows user, without admin rights. It adds Start-menu entries for **JRiver Cast Bridge** and its **Settings**, and can start the bridge at logon.
2. The settings live in `%APPDATA%\jriver-cast-bridge\config.toml`, and the logs in `%APPDATA%\jriver-cast-bridge\logs` (one file a day, kept 14 days).

## Set up

Open **Settings** from the Start menu (or **Settings…** in the tray menu). Saving applies the changes at once; a playing film plays on.

| Setting | What to enter |
|---|---|
| **Jellyfin server** | Its address, e.g. `http://jellyfin.local:8096` |
| **Cast name** | The name shown in Jellyfin's cast menu (default "JRiver (HTPC)") |
| **Login profile** | Leave at `default` (only needed to keep a second, test login apart) |
| **Media Network URL** | `http://localhost:52199/MCWS/v1/` unless you changed MC's port |
| **Zone** | `0`, MC's local "Player" zone |
| **Media Center program** | `C:\Program Files\J River\Media Center 36\Media Center 36.exe`, so the bridge can start MC when a cast arrives and MC isn't running |
| **Authentication** | Only if Media Network authentication is on in MC: the user name and password. The password is kept in Windows Credential Manager, never in the config file |
| **Path mapping** | One rule per library: the path as **Jellyfin** reports it → the share path the **HTPC** opens. For example `/media/movies` → `\\nas\movies`, and `/media/tv` → `\\nas\tv`. The first matching rule wins. Use share (UNC) paths, not drive letters: MC adds every file it plays to its library, and the same file under two paths becomes two entries |
| **Minimize Media Center when a full-screen film ends** | On by default |
| **Play the audio and subtitle tracks picked in Jellyfin** | On by default, see [Audio and subtitle tracks](#audio-and-subtitle-tracks) |
| **Jellyfin Media Player** | Whether the bridge starts JMP, and where JMP is installed |
| **JRiver hang detection** | How long MC may be "Not responding" before the bridge restarts it (3 s, or 20 s while a file is opening), and the most restarts an hour (3; 0 turns restarts off) |
| **Log** | Log level and days kept |

### Log in

Start **JRiver Cast Bridge**. The first time, it opens its login window for the server in the settings: enter the Jellyfin user name and password. The bridge keeps only the access token, in Windows Credential Manager. **Log out** in the tray menu revokes it.

### Jellyfin Media Player

1. In JMP, cast to "JRiver (HTPC)" once, so JMP remembers it as its cast target.
2. Remove JMP's own autostart: open `shell:startup` in Explorer and delete the **Jellyfin Media Player** shortcut. While it's there, JMP may start before the bridge has registered, and then it won't pick the bridge. The Settings window warns while the shortcut is there.
3. Leave **Start Jellyfin Media Player** on in Settings. At logon the bridge starts JMP once it is registered with Jellyfin (or after 60 s if the server can't be reached). It never starts a second copy.

## Use

- **Cast** any film or episode to "JRiver (HTPC)". JRiver opens the file full screen and starts at your resume point.
- **The remote** in the Jellyfin app works: pause, resume, seek and stop reach JRiver within about a second. Stopping or pausing in JRiver itself shows up in Jellyfin too.
- **Resume points and watched status** are reported to Jellyfin as you watch, every 10 s and at the stop. Jellyfin's own rules decide when a film counts as watched.
- **A new cast** while a film plays stops the old film first.

### Audio and subtitle tracks

With **Play the audio and subtitle tracks picked in Jellyfin** on:

- **An item's page** in Jellyfin shows the audio and subtitle pickers while casting to the bridge. The tracks you pick there play in JRiver: the bridge selects them just after the film starts.
- **The full remote-control screen** (tap the now-playing bar) has audio and subtitle buttons to switch tracks during the film.

With the setting off, Media Center picks the tracks: its own defaults, or the tracks last used for that file. Jellyfin then offers no track choice when casting to the bridge.

After changing this setting, pick the bridge again in the Jellyfin app's cast menu. Jellyfin apps read a cast target's abilities only when it is selected.

### The tray menu

**Status** shows what the bridge is doing (connected, playing, a rejected login). The menu also has **Log in…** or **Log out**, **Settings…**, **Open log**, **About** (version and build) and **Quit**.

### When things go wrong

The bridge handles these by itself:

- **JRiver not running:** a cast starts it ("Starting JRiver…" shows on screen) and then plays.
- **JRiver hangs:** when Windows reports Media Center as "Not responding" and MC stops answering, the bridge closes MC, starts it again and resumes the film where it was. It does this at most 3 times an hour.
- **The bridge restarts** (a crash, or a new login while a film plays): it takes over the film that JRiver is still playing, so the remote and progress reports go on working.
- **Saving settings** keeps the connection to Jellyfin, so the Jellyfin app keeps the bridge selected. Changing the server, the cast name or the login profile does reconnect.

## Troubleshooting

- **Logs:** **Open log** in the tray menu, or `%APPDATA%\jriver-cast-bridge\logs`. Set the log level to `debug` in Settings to log every call to MC and Jellyfin.
- **The bridge isn't in the cast menu:**
  - Check the tray status.
  - "Jellyfin rejected the login" means the saved login was revoked or expired: choose **Log in…**.
  - Otherwise check the server address in Settings.
- **JMP doesn't select the bridge at logon:** make sure JMP's own Startup shortcut is gone (see [Jellyfin Media Player](#jellyfin-media-player)).
- **"Can't play" in the log:**
  - The path mapping didn't match, or the HTPC can't open the mapped file. The log shows both the server path and the mapped path.
  - Check the share and the mapping rules.
- **A track choice has no effect:**
  - Disc folders (Blu-ray) get no track commands.
  - For a film with a separate subtitle file next to it, only "subtitles off" is sent.
  - In both cases, switch tracks in JRiver.

## Limitations

- **One film at a time:** a queue sent from Jellyfin plays only the item you picked.
- **Files and Blu-ray folders only:** ISO images and DVDs aren't played.
- **No volume or mute control from Jellyfin:** use the receiver.
- **Media Center can't report which track it plays,** so Jellyfin shows the track that was asked for.

## Build from source

- **Requirements:** Rust (the version in `rust-toolchain.toml`) and [Inno Setup 6](https://jrsoftware.org/isinfo.php).
- **Installer:** `powershell -ExecutionPolicy Bypass -File installer\build.ps1` builds `target\installer\jriver-cast-bridge-setup-<version>.exe`.
- **Lint and test:** `cargo xtask rust clippy` and `cargo xtask test`; `cargo xtask check` also checks the formatting.
- **Command-line tool:** `bridge-cli`, for testing.
  - `run` runs the bridge in a console.
  - `quit` and `reload` control a running bridge.
  - `login`, `whoami` and `listen` test the Jellyfin side.
  - `item <id>` shows the file and tracks JRiver would play.
  - `play`, `pause`, `seek`, `stop`, `info` and `track` drive JRiver directly.

| Crate | What it is |
|---|---|
| `crates/jriver-cast-bridge` | The bridge: tray program, settings, login and About windows, the Jellyfin WebSocket (`socket` module), and `bridge-cli` |
| `crates/jriver-mcws` | The MCWS client for JRiver Media Center |
| `xtask` | Build and lint tasks |

`SPEC.md` describes how the bridge behaves.

## Credits

- The Jellyfin WebSocket code (`crates/jriver-cast-bridge/src/socket.rs`) comes from [JellyPilot](https://github.com/hewel/jellypilot) (formerly [JMSR](https://github.com/hewel/jmsr)) by hewel and the JellyPilot contributors, used under the MIT licence: see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
- [JRiver Media Center](https://jriver.com/), driven through its MCWS web service.
- [Jellyfin](https://jellyfin.org/).
