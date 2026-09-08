//! Helper for spawning console helper processes (`netsh`, `arp`) without a
//! flashing console window. This app has no console of its own
//! (`#![windows_subsystem = "windows"]`), so by default Windows pops up a
//! brand new console window for every child console process we spawn — with
//! `netsh` called repeatedly (auto-detect probes six candidate subnets) that
//! looked like a rapid series of flickering popup windows. `CREATE_NO_WINDOW`
//! suppresses that.

use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
