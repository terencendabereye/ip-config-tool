# IP Config Tool

A small Windows utility for quickly connecting to Ethernet devices with an
unknown IP address (PLCs, RTUs, energy meters, etc.):

1. Pick the network adapter that's wired to the device.
2. Either type in the IP/subnet/gateway you want to use, or click
   **Auto-detect subnet** to have it probe common industrial subnets
   (192.168.1.0/24, 192.168.0.0/24, 10.0.0.0/24, ...) for a live device.
3. **Scan for devices** to ARP-sweep the current /24 and see what's alive
   (the same mechanism Windows itself uses for `arp -a` — faster and more
   reliable than pinging, since it works even when a device's firewall
   blocks ICMP).
4. **Apply** the static IP to the adapter.
5. Do your work in the PLC/RTU/meter's own software.
6. Click **Revert to original settings** (or just wait — settings
   auto-revert after 3 minutes unless you click **Keep**) to restore the
   adapter to whatever it was before, no need to open Windows network
   settings at all. Closing the app also reverts everything automatically,
   with no confirmation prompt needed.

## Identifying and working with a discovered device

Selecting a row in the scan results resolves (in the background, one device
at a time — the bulk scan itself never slows down for this) its:

- **Vendor**, from the MAC's OUI, looked up entirely offline against an
  embedded copy of IEEE's public registry (~40,000 entries) — works even on
  an isolated plant network with no internet access.
- **Hostname**, via reverse DNS, falling back to a NetBIOS name query
  (`nbtstat`) when there's no DNS server on the segment, which is common on
  OT networks.
- **Open ports**, a quick check of ports relevant to identifying the device
  (FTP, SSH, Telnet, HTTP/S, SMB, RDP) and common industrial protocols
  (Modbus TCP, S7comm, DNP3, EtherNet/IP).

Once a row is selected, the action buttons below the list operate on it:
**Ping** and **Traceroute** (continuous, in a visible console window —
Ctrl+C to stop), **SSH** (opens `ssh.exe` in a console window — requires
Windows' OpenSSH client, an optional Windows feature, to be installed),
**Open web UI** (launches the device's `http(s)://` page in your default
browser), **Wake-on-LAN**, and **Copy IP** / **Copy MAC**.

> Ping/Traceroute/SSH intentionally open in a separate console window for
> now rather than an in-app terminal pane — see the "embedded terminal"
> issue on the repo for the planned follow-up (a proper ConPTY-backed
> terminal, needed for full interactive fidelity in SSH sessions).

## When IPv4 is a dead end: IPv6 link-local

Every "Scan for devices" click also reads the IPv6 neighbor table
(`netsh interface ipv6 show neighbors`) for the selected adapter, alongside
the ARP sweep. This matters because **IPv6 link-local addresses
(`fe80::/10`) self-assign on any live Ethernet link with zero configuration
on either end** — no DHCP, no subnet match, nothing that a misconfigured or
recently-changed IPv4 network can break. It's the fallback that works when
a site's switch/routing config has been changed without your knowledge and
IPv4 auto-detect has nothing to try.

Any link-local neighbor found shows up as another row (its zoned address,
e.g. `fe80::...%12`), and works with the same identify/action flow as an
IPv4 row — vendor lookup, port scan, Ping/Traceroute/SSH, Open web UI, Copy.
(Reverse DNS and NetBIOS name lookup are skipped for these, since neither
applies to a link-local address.) If the adapter has no DHCP gateway at
all, the scan-complete status also flags that it may be a direct cable
connection — most common workflow here: Ethernet straight from your laptop
into a PLC, no switch in between.

Most PLCs/RTUs are IPv4-only, so don't expect this to find the device
itself in the common case — its proven value is reaching *something* (a
managed switch's management interface, another PC) when IPv4 has otherwise
completely failed.

## Requirements

To **run** the built exe: Windows 10/11 and administrator rights (the exe
requests elevation — a UAC prompt on launch — since changing adapter IP
settings requires it). Nothing else; see "Building" below for why.

To **build from source**:
- Windows 10/11.
- The Rust toolchain, MSVC edition (`x86_64-pc-windows-msvc`) — install via
  [rustup.rs](https://rustup.rs). This is the default target rustup picks on
  Windows, so a plain `rustup-init.exe` run is normally enough.
- The MSVC linker, from either **Visual Studio** (Desktop development with
  C++ workload) or the smaller **[Build Tools for Visual
  Studio](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022)**
  (just the "Desktop development with C++" component, no full IDE needed).
  This is a Windows/MSVC-toolchain requirement, not specific to this
  project — `rustup` will tell you if it's missing when you try to build.
- No other dependencies. `data/oui.csv` (the embedded offline vendor
  database, ~1.2MB) is checked into the repo, so a plain `git clone` +
  `cargo build` needs no separate download/generation step — verified by
  building from a fresh clone in an empty directory.

## Building

```
git clone https://github.com/terencendabereye/ip-config-tool.git
cd ip-config-tool
cargo build --release
```

The result is a single, self-contained `target\release\ip-config-tool.exe`
(no runtime to install — Rust compiles to a native binary; all dependencies
in `Cargo.toml`/`Cargo.lock` are pulled from crates.io automatically by
`cargo build`, no manual dependency installation). Copy that one file
anywhere to run it on another PC.

For machines where even the standard VC++ runtime/UCRT (present on
essentially all Windows 10/11 installs) shouldn't be assumed, build with the
CRT statically linked instead, for a fully dependency-free exe:

```
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release
```

## Testing

`cargo test` runs the full unit test suite (parsing of real captured
`netsh`/`arp`/`nbtstat`/IPv6-neighbor-table output, IP/mask/gateway
validation, backup/restore JSON round-trips, the auto-detect decision
logic, OUI vendor lookups, and Wake-on-LAN packet construction) — **no
admin rights and no network changes required**, safe to run anytime.

Test binaries deliberately do **not** get the `requireAdministrator`
manifest (only the `[[bin]]` GUI target does — see `build.rs` and the
`test = false` / separate `[lib]` setup in `Cargo.toml`), so `cargo test`
never triggers a UAC prompt.

See the project plan for the full manual/integration test matrix — in
particular: **always test Apply/Revert against a throwaway adapter first**
(a USB Ethernet dongle or a virtual switch adapter), never your primary or
remote-access NIC, in case a bug leaves it unreachable.

## Safety features

- **Backup before every change**: the adapter's original config (DHCP or
  static, IP/mask/gateway/DNS) is saved to `%TEMP%` before anything is
  touched, and only deleted after a successful revert.
- **Revert on close, automatically**: closing the app reverts every adapter
  that still has an applied static config, with no confirmation prompt.
- **Orphaned-backup recovery**: if the app was force-closed or crashed
  with a backup still on disk (so the close-time auto-revert never ran),
  the next launch silently reverts it before you do anything else.
- **Auto-revert timer**: after Apply, a 3-minute countdown reverts
  automatically unless you click Keep — protects against being locked out
  by a bad static config.
- **Input validation** (valid IP/mask, gateway inside the resulting subnet)
  before any `netsh` call is made. A dialog only ever appears for a genuine
  error (invalid input, a failed `netsh` call) — routine confirmations were
  deliberately removed to avoid popup spam.

## Data: the embedded vendor (OUI) table

`data/oui.csv` is a trimmed copy of IEEE's public MA-L registry
(https://standards-oui.ieee.org/oui/oui.csv), reduced to `prefix,vendor`
and sorted, embedded into the binary at compile time (`src/oui.rs`) so
vendor lookup works fully offline. To refresh it against IEEE's current
registry: download `oui.csv` from that URL into `data/oui_raw.csv`, then run
`python data/trim_oui.py` from the `data/` directory.

## Icon

`assets/app.ico` (an RJ45 port silhouette, viewed head-on) is embedded as
the exe's file icon (`build.rs`, via the `winres` crate). The running
window's icon (`src/gui.rs`) is read back out of that same compiled-in
resource via `nwg::EmbedResource` rather than decoded from raw bytes at
runtime — `Icon::from_bin` needs nwg's `"image-decoder"` feature, which
isn't enabled, and panics instantly (with no visible error, since this is a
console-less app) without it. Regenerate the `.ico` with
`python assets/build_icon.py` after editing the glyph in
`assets/build_icon.py` itself (the source of truth — there's no separate
vector file to keep in sync).

## A note on `lto` in `Cargo.toml`

`profile.release` deliberately sets `lto = false`. Fat LTO was tried for
extra size reduction and found to silently strip reachable application code
in a clean build — confirmed by grepping the compiled exe for literal
strings from `App::build_ui` and everything downstream of it (all present
with LTO off, all missing with it on). If re-enabling LTO in the future,
verify with the same check (`python -c "print(b'IP Config Tool' in open('target/release/ip-config-tool.exe','rb').read())"`
after a **clean** `cargo build --release` — a stale `target/` can mask this)
before trusting the build.
