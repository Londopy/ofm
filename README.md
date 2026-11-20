# ofm

[![CI](https://github.com/Londopy/ofm/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ofm/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ofm)](https://github.com/Londopy/ofm/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ofm/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**An open source sound card modem for FM radios that Pat can use in place of VARA FM.**

ofm is a modem for VHF/UHF FM radios. Toward Winlink clients it looks like VARA FM: the same
two TCP ports and the same text commands, so Pat connects to it as `varafm://` with no changes
on Pat's side. On the air it uses its own open OFDM waveform, so an ofm station talks only to
other ofm stations.

## Status

ofm is early work and has not been tested on the air yet. It has been tested in a channel
simulator and between two instances over a virtual audio cable. The waveform, framing and
link protocol are documented in full in [docs/waveform.md](docs/waveform.md), but the format
is not frozen yet, so run the same ofm version at both ends of a link.

## Install

### Windows

Every [release](https://github.com/Londopy/ofm/releases/latest) has the same set of downloads:

| Download | What it is |
|---|---|
| `ofm-<version>-setup-x64.exe` | Installer (recommended). Per-user, no admin prompt, and puts `ofm` on your PATH. |
| `ofm-<version>-x64.msi` (also `x86`, `arm64`) | MSI installer. Per-user by default; IT can deploy it machine-wide. |
| `ofm-v<version>-x64-portable.exe` (also `x86`, `arm64`) | The exe on its own, nothing to install. |
| `ofm-v<version>-portable.zip` | All three builds in one zip; settings stay next to the exe. |
| `install.ps1` / `install.cmd` | Script install that picks the right build and checks its SHA-256. |
| `<file>.torrent` and magnet links | Any of the above over BitTorrent, with GitHub as the web seed when nobody is seeding. |

PowerShell one-liner:

```powershell
irm https://github.com/Londopy/ofm/releases/latest/download/install.ps1 | iex
```

Options: `-Msi`, `-Portable` (add `-AddToPath` to put it on PATH), `-AllUsers`,
`-Version 1.2.3`. To pass them, run the script as a block:
`& ([scriptblock]::Create((irm https://github.com/Londopy/ofm/releases/latest/download/install.ps1))) -Portable -AddToPath`

Scoop:

```powershell
scoop bucket add londopy https://github.com/Londopy/scoop-bucket
scoop install londopy/ofm
```

MSI for every user (admin): `msiexec /i ofm-<version>-x64.msi ALLUSERS=2 MSIINSTALLPERUSER=""`.
Add `DESKTOPSHORTCUT=0` to skip the desktop shortcut or `ADDTOPATH=0` to leave PATH alone.

The Start menu and desktop shortcuts open a terminal running `ofm`, which stays open when ofm
exits so you can read any error.

### Linux

Each release has `ofm-v<version>-linux-x64.tar.gz` for x86_64 with glibc 2.35 or newer
(Ubuntu 22.04, Debian 12 and later):

```sh
tar -xzf ofm-v<version>-linux-x64.tar.gz
install -Dm755 ofm-v<version>-linux-x64/ofm ~/.local/bin/ofm
```

### Checking a download

The binaries are not code-signed, so Windows SmartScreen may warn the first time you run one
(**More info**, then **Run anyway**). Every release ships `SHA256SUMS.txt`; compare a download
with its line there:

```powershell
(Get-FileHash .\<file> -Algorithm SHA256).Hash.ToLower()
```

On Linux: `sha256sum -c SHA256SUMS.txt --ignore-missing`.

### Updating

`ofm update` checks GitHub Releases and tells you when there is a newer version. If you type
y, it downloads the build for your system, checks it against the release's `SHA256SUMS.txt`
and replaces itself. Nothing is installed without that y. The modem also checks once when it
starts and prints a line if a newer version is out; set `check_on_start = false` under
`[updater]` in the [config file](#configuration) to turn that off.

## Use it with Pat

1. Find your radio interface's sound card:

   ```
   ofm devices
   ```

2. Set your levels. `ofm calibrate` sends a steady tone so you can set the deviation without a
   service monitor, shows the receive level, and saves the transmit level you settle on.
   Over-deviation is the most common reason an FM data link fails.

3. Start the modem. It listens for Pat on TCP port 8300 (commands) and 8301 (data). Part of a
   device's name is enough:

   ```
   ofm run --profile narrow --input "<input device>" --output "<output device>"
   ```

   By default ofm leaves keying the radio to Pat (`ptt_ctrl` with rigctld) or to the radio's
   VOX. To have ofm key it, set `method` under `[ptt]` in the config file to `rts`, `dtr`,
   `cm108` or `rigctld`.

4. In Pat's configuration (`pat configure`), set `addr` in the `varafm` section to ofm's
   command port and leave the section's other settings as they are. Pat finds the data port by
   adding one.

   ```json
   "varafm": {
     "addr": "127.0.0.1:8300"
   }
   ```

5. Connect to a station that also runs ofm, from Pat's web interface (VARA FM) or the command
   line:

   ```
   pat connect varafm:///N0CALL-10
   ```

   To take incoming P2P connections, start Pat with `--listen varafm`, as you would for VARA FM.

All of this mirrors how Pat drives VARA FM, but none of it has been tried over the air yet.

## Commands

| Command | What it does |
|---|---|
| `ofm` or `ofm run` | Starts the modem. `--port 8300` sets the command port (data is the next port up), `--kiss-port 8310` the KISS port, `--profile narrow\|wide` the speed profile, and `--input`/`--output` the audio devices. |
| `ofm devices` | Lists the audio devices. |
| `ofm calibrate` | Sends a steady tone and shows the receive level. |
| `ofm update` | Checks GitHub Releases for a newer ofm and installs it after you type y. |

The KISS-over-TCP port sends frames without ARQ, for broadcast tools rather than Winlink.

## Profiles and speed

| Profile | Radio connection | Passband | Top level, raw | Measured over a virtual cable |
|---|---|---|---|---|
| Narrow | mic/speaker, or the 1200-baud data jack | about 560 to 2690 Hz | 8.1 kbps | 5.3 kbps |
| Wide | the flat 9600-baud discriminator port | about 310 to 4750 Hz | 17.2 kbps | 12.3 to 13.0 kbps |

ofm adapts its speed to the link across ten levels, from about 400 bps (Narrow) or 860 bps
(Wide) up to the top level. "Measured" is the net transfer rate of a two-way exchange between
two ofm instances in real time over VB-Audio Virtual Cable, with 200 ms TXDELAY and turnarounds
included. Real radios add noise, filtering and slower keyup, so expect less on the air; nothing
has been measured there yet. [docs/curves.md](docs/curves.md) has error rate and throughput
for every level in the channel simulator.

## Configuration

| | Path |
|---|---|
| Windows | `%APPDATA%\ofm\config.toml` |
| Linux | `$XDG_CONFIG_HOME/ofm/config.toml`, usually `~/.config/ofm/config.toml` |
| Portable | `config.toml` next to the executable, when a file named `portable` sits beside it (the portable zip includes one) |

ofm writes the file with its defaults the first time it starts. Flags such as `--port` and
`--input` override it for that run.

## What it does not do

- Talk to VARA. ofm copies VARA FM's host interface, not its waveform, so it can't reach
  VARA-only Winlink RMS gateways until their operators run ofm too. ofm is not affiliated with
  VARA or its author.
- HF. ardopcf and Mercury cover that.
- Digipeating, or `CONNECT` with `via`, in this version.
- Encryption, which amateur rules prohibit (47 CFR 97.113(a)(4) in the US).

## Building from source

```sh
git clone https://github.com/Londopy/ofm
cd ofm
cargo build --release
```

On Linux, install the ALSA and udev headers first
(`sudo apt install libasound2-dev libudev-dev pkg-config` on Debian and Ubuntu).

Installers: [Inno Setup 6](https://jrsoftware.org/isinfo.php) builds the setup.exe
(`ISCC.exe /DAppVersion=0.1.0 installer\ofm.iss`) and [WiX v7](https://wixtoolset.org) builds
the MSIs; `.github/workflows/release.yml` has the exact commands.

## Layout

```
src/              the ofm program
crates/ofm-core/  waveform, LDPC, framing and ARQ
installer/        Inno Setup (.iss) and WiX (.wxs) scripts, install.ps1/.cmd, Scoop manifest
scripts/          release_notes.py (release page, checksums, torrents)
```

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). A pull request
that changes the waveform, framing or ARQ also updates `docs/waveform.md`.

## License

MIT OR Apache-2.0, at your option: [LICENSE-MIT](LICENSE-MIT) or [LICENSE-APACHE](LICENSE-APACHE).
