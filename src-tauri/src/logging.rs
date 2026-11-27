// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

use chrono::Utc;
use parking_lot::Mutex;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB per log file

pub struct LogSink {
    log_dir: PathBuf,
    file: Mutex<Option<File>>,
}

impl LogSink {
    pub fn new(user_data_dir: &Path) -> Self {
        let log_dir = user_data_dir.join("logs");
        let _ = fs::create_dir_all(&log_dir);
        let file = open_log_file(&log_dir).ok();
        Self {
            log_dir,
            file: Mutex::new(file),
        }
    }

    pub fn write_line(&self, level: &str, scope: &str, message: &str) {
        let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let line = format!("[{ts}] [{level}] [{scope}] {message}\n");

        let mut guard = self.file.lock();
        if let Some(ref mut f) = *guard {
            if let Ok(meta) = f.metadata() {
                if meta.len() > MAX_LOG_BYTES {
                    let _ = rotate_log(&self.log_dir);
                    *guard = open_log_file(&self.log_dir).ok();
                }
            }
        }
        if let Some(ref mut f) = *guard {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn delete_all_logs(&self) -> std::io::Result<()> {
        let mut guard = self.file.lock();
        *guard = None;
        if self.log_dir.exists() {
            for entry in fs::read_dir(&self.log_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        *guard = open_log_file(&self.log_dir).ok();
        Ok(())
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }
}

fn open_log_file(log_dir: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("main.log"))
}

fn rotate_log(log_dir: &Path) -> std::io::Result<()> {
    let current = log_dir.join("main.log");
    let prev = log_dir.join("main.old.log");
    if current.exists() {
        let _ = fs::rename(&current, &prev);
    }
    Ok(())
}

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,signal_desktop_rs_lib=debug"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}
