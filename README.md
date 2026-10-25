# mosh-win

A native Windows client for [mosh](https://mosh.org) (the mobile shell), written in Rust.
No Cygwin, WSL or MSYS: it runs directly in Windows Terminal, PowerShell or cmd.

It is a port of the mosh 1.4 client and talks the same protocol, so it works with the
stock `mosh-server` on Linux/macOS/BSD:

- SSP state synchronization over UDP, AES-128-OCB encryption, roaming / port hopping
- the server-compatible terminal emulator and screen diffing
- adaptive predictive local echo (`--predict`)
- notification bar ("Last contact N seconds ago"), `Ctrl-^ .` to quit

> **Status: experimental.** Tested against mosh-server 1.4.0 on Linux (interactive
> shells, vim, top, Japanese text, resizing). Roaming across network changes and IME
> input have not been field-tested yet. Bug reports are welcome.
>
> This is an unofficial client and is not affiliated with the mosh project.

## Download

[mosh-win-0.1.0-x86_64-windows.zip](docs/mosh-win-0.1.0-x86_64-windows.zip) (Windows 10/11, x64;
a single `mosh-win.exe`, no installer). SHA-256 checksums are in
[docs/SHA256SUMS.txt](docs/SHA256SUMS.txt):

```
ebc2bdc327f1e50e5b5a7c5c2d14173f243e9dab27354516e56a6e33667612d0  mosh-win-0.1.0-x86_64-windows.zip
5ada4c8c7e3041092d55f13eb5a0ce0f0ee262221eb736c80713fe502b6b683e  mosh-win.exe
```

Verify with `Get-FileHash .\mosh-win-0.1.0-x86_64-windows.zip`.

## Usage

```
mosh-win [options] [user@]host [--] [command...]
```

The server is started over ssh (Windows OpenSSH by default), exactly like the `mosh` wrapper.
To use a particular ssh config or options:

```
mosh-win --ssh="ssh -F G:\path\to\config" myhost
# or once per shell / in your profile:
$env:MOSH_WIN_SSH = "ssh -F G:\path\to\config"
```

Run `mosh-win --help` for all options. The server needs UDP ports 60000-61000 reachable
(or pick one with `-p`).

## Build

Requires a recent stable Rust (edition 2024). Developed with the GNU toolchain
(`x86_64-pc-windows-gnu`); the MSVC toolchain should work too.

```
cargo build --release
# or install into ~/.cargo/bin:
cargo install --path . --locked
```

`cargo test` runs the unit tests (crypto test vectors, fragmenting, diff round-trips,
prediction). `examples/conpty_harness.rs` drives the real client inside a Windows
pseudo console for end-to-end tests.

## License

mosh-win is a port of [mosh](https://github.com/mobile-shell/mosh), Copyright 2012
Keith Winstein and the mosh authors (Anders Kaseorg, Quentin Smith, Richard Tibbetts,
Keegan McAllister, John Hood and many contributors).

Like mosh, it is free software under the GNU General Public License, version 3 or
(at your option) any later version. See [LICENSE](LICENSE).
mosh-win does not use OpenSSL, so mosh's OpenSSL linking exception is not needed here.
