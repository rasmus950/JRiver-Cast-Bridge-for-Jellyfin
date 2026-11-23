# AGENTS.md

Guidelines for working on JRiver Cast Bridge for Jellyfin, for people and coding agents alike. README.md explains what the bridge is and how to use it. SPEC.md describes how it behaves. When code and SPEC.md disagree, fix one of them, and don't leave both standing.

## Project layout

| Path | What it is |
|---|---|
| `crates/jriver-cast-bridge` | The bridge. Library modules in `src/`: `controller` (the playback state machine, watchdog, takeover), `service` (wires Jellyfin, JRiver and the controller together; reloads), `jellyfin` (REST client and reports), `socket` (the Jellyfin WebSocket), `jriver` (the player over MCWS, launching and restarting MC), `tracks`, `path_map`, `resolve`, `record`, `config`, `settings`, `credentials`, `ipc`, `logging`. Binaries in `src/bin/`: `jriver_cast_bridge` (tray program, settings, login and About windows) and `bridge_cli` (testing tool) |
| `crates/jriver-mcws` | The MCWS client: every call MC accepts from the bridge, with its parameters, in `function.rs` and `client.rs` |
| `xtask` | `cargo xtask` build and lint tasks |
| `installer` | Inno Setup script and `build.ps1` |
| `crates/jriver-cast-bridge/config.example.toml` | Every config key, documented; the installer copies it as the first config |

## Build, lint, test

- `cargo xtask rust clippy`: clippy with `-D warnings` on every crate (`cargo xtask rust clippy bridge` for the bridge alone).
- `cargo xtask test`: every crate's tests. For quick runs: `cargo test -p jriver-cast-bridge --lib`.
- `cargo xtask check`: the format check, clippy and the xtask tests.
- `powershell -ExecutionPolicy Bypass -File installer\build.ps1`: release build and installer (needs Inno Setup 6).

Lint, tests and the format check pass before every commit.

## Coding guidelines

- **Rust** (version pinned in `rust-toolchain.toml`).
  - `rustfmt` with 2-space indentation.
  - `unsafe_code` is forbidden workspace-wide.
  - `clippy.toml` bans process-global environment changes and `std::process::exit`.
  - Errors are typed with `thiserror`; nothing panics on bad input from the network, MC or the config.
- **Tests come with every change.**
  - The controller is tested against a fake player and a fake Jellyfin (`controller.rs` tests), with Tokio's paused clock.
  - The HTTP clients are tested against small fake servers.
  - Don't test against your own Jellyfin account; use a separate test user and login profile.
- **Verify every API detail against the source:** MC's own MCWS documentation (`http://localhost:52199/MCWS/v1/`) and the Jellyfin server's OpenAPI (`/api-docs/swagger`). Don't rely on memory or forum posts.
- **Hard rules towards Media Center:**
  - never send volume or mute commands
  - never change MC settings, DSP, audio output or JRVR profiles
  - never ask MC (or Jellyfin) for a stream, a conversion or a transcode: the bridge always plays the original file, and reports `PlayMethod: DirectPlay`
- **New MC calls:** any other MCWS call or MCC command may be added. List it in SPEC.md's JRiver table, and send it through its own named function with a fixed command number; never add a way to send an arbitrary command.
- **Secrets** (the Jellyfin token, the Media Network password) live only in Windows Credential Manager: never in the config file, the logs, error messages or git. Redact tokens from anything logged.
- **The playing film is never left untracked:** every way a playback ends reports a stop to Jellyfin exactly once.
- **Comments say why,** and name the SPEC.md section a behaviour comes from (e.g. SPEC.md "JRiver hung").
- **Licences:** the project is MIT (`LICENSE`). It includes code from JellyPilot (MIT): keep its notice in `THIRD_PARTY_NOTICES.md` and in the header of `socket.rs`, and the credit in README.md and the About window. Code taken from other projects gets the same treatment.

## Commits

- Small commits, each with lint and tests passing.
- [Conventional Commits](https://www.conventionalcommits.org/) style: `feat(bridge): …`, `fix(jriver): …`, `docs(spec): …`, `refactor(…)`, `test(…)`, `chore: …`.
- Bump `version` in `crates/jriver-cast-bridge/Cargo.toml` for a release; the installer takes its version from there.
