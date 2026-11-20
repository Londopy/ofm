# Changelog

All notable changes to ofm are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- `ofm` (or `ofm run`) starts the modem with a VARA FM compatible host interface: commands on
  TCP port 8300 and data on 8301, so Pat connects to it as `varafm://`. `--port`, `--profile`,
  `--input` and `--output` pick the port, speed profile and audio devices.
- Narrow and Wide speed profiles, for the mic/speaker or 1200-baud data jack and for the flat
  9600 discriminator port.
- A KISS-over-TCP port for connectionless frames, 8310 by default (`--kiss-port`).
- The full waveform, framing and link protocol in `docs/waveform.md`, and error rate and
  throughput for every speed level in the channel simulator in `docs/curves.md`.
- `ofm devices` lists the audio devices, and `ofm calibrate` sends a steady tone and shows the
  receive level, for setting deviation.
- PTT from the client (Pat with rigctld) or VOX, or keyed by ofm itself through serial RTS or
  DTR, a CM108 GPIO, or rigctld (`method` under `[ptt]`). Ctrl-C releases PTT before ofm
  exits.
- `ofm update` checks GitHub Releases for a newer version. After you type y it downloads it,
  checks its SHA-256 against the release's `SHA256SUMS.txt` and replaces itself. The modem also
  says when a newer version is out as it starts (`check_on_start` under `[updater]`).
- Settings in `%APPDATA%\ofm\config.toml` (`~/.config/ofm/config.toml` on Linux), or next to
  the executable when a file named `portable` sits beside it.
- Windows downloads: a per-user setup.exe that puts ofm on your PATH, MSIs for x64, x86 and
  arm64, portable exes and a portable zip, `install.ps1` and `install.cmd`, and a torrent for
  each. Plus a Linux x86_64 build.

[Unreleased]: https://github.com/Londopy/ofm/compare/v0.1.0...HEAD
