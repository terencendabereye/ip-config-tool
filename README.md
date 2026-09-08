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

## Requirements

- Windows 10/11.
- Administrator rights — the exe requests elevation (UAC prompt) on launch,
  since changing adapter IP settings requires it.

## Building

```
cargo build --release
```

The result is a single, self-contained `target\release\ip-config-tool.exe`
(no runtime to install — Rust compiles to a native binary). Copy that one
file anywhere to run it on another PC.

For machines where even the standard VC++ runtime/UCRT (present on
essentially all Windows 10/11 installs) shouldn't be assumed, build with the
CRT statically linked instead, for a fully dependency-free exe:

```
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release
```

## Testing

`cargo test` runs the full unit test suite (parsing of real captured
`netsh`/`arp` output, IP/mask/gateway validation, backup/restore JSON
round-trips, the auto-detect decision logic) — **no admin rights and no
network changes required**, safe to run anytime.

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
