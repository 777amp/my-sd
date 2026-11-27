// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    signal_desktop_rs_lib::run();
}
