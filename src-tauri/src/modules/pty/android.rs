// src-tauri/src/modules/pty/android.rs
//
// Android PTY backend: opens /dev/ptmx via POSIX posix_openpt(),
// spawns proot + Alpine Linux rootfs, streams output to xterm.js
// through a Tauri Channel<Response>.
//
// NOTE: openpty() is NOT used here because Android's Bionic libc does
// not ship libutil. Instead we use the standard POSIX sequence:
//   posix_openpt → grantpt → unlockpt → ptsname → open(slave)

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use tauri::ipc::{Channel, Response};
use tauri::Manager;

// How often the flusher thread drains the pending buffer and sends to JS
const FLUSH_INTERVAL: Duration = Duration::from_millis(4);
const READ_BUF: usize = 16 * 1024;
const MAX_PENDING: usize = 4 * 1024 * 1024;
const OVERFLOW_NOTICE: &[u8] =
    b"\x1bc\x1b[2m[terax: dropped output due to backpressure]\x1b[0m\r\n";

// ── Paths ─────────────────────────────────────────────────────────────────────

/// Returns the directory where bootstrap extracted proot + Alpine.
/// On Android this is always /data/data/<package>/files.
pub fn android_files_dir() -> PathBuf {
    PathBuf::from("/data/data/app.crynta.terax/files")
}

pub fn proot_bin() -> PathBuf {
    android_files_dir().join("proot")
}

pub fn rootfs_dir() -> PathBuf {
    android_files_dir().join("rootfs")
}

// ── PTY helpers ───────────────────────────────────────────────────────────────

/// Open a PTY master/slave pair using the standard POSIX API that
/// Android's Bionic libc actually provides (no openpty / libutil needed).
fn open_pty_pair(cols: u16, rows: u16) -> Result<(RawFd, RawFd), String> {
    // 1. Open the master side
    let master_fd: RawFd =
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        return Err(format!(
            "posix_openpt failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // 2. Grant access to the slave device
    if unsafe { libc::grantpt(master_fd) } < 0 {
        unsafe { libc::close(master_fd) };
        return Err(format!(
            "grantpt failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // 3. Unlock the slave device
    if unsafe { libc::unlockpt(master_fd) } < 0 {
        unsafe { libc::close(master_fd) };
        return Err(format!(
            "unlockpt failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // 4. Get slave device name (e.g. /dev/pts/3)
    let slave_name = unsafe {
        let ptr = libc::ptsname(master_fd);
        if ptr.is_null() {
            unsafe { libc::close(master_fd) };
            return Err("ptsname returned null".to_string());
        }
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().to_string()
    };

    // 5. Open the slave side
    let slave_cstr =
        CString::new(slave_name).map_err(|e| format!("ptsname CString: {e}"))?;
    let slave_fd: RawFd =
        unsafe { libc::open(slave_cstr.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave_fd < 0 {
        unsafe { libc::close(master_fd) };
        return Err(format!(
            "open slave pty failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // 6. Set initial window size on the master
    let ws = libc::winsize {
        ws_col: cols,
        ws_row: rows,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws) };

    Ok((master_fd, slave_fd))
}

// ── Session ───────────────────────────────────────────────────────────────────

struct Session {
    master_fd: RawFd,
    write_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// PID of the proot child process for kill-on-close.
    child_pid: nix::unistd::Pid,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort kill. The process may already be dead.
        let _ = nix::sys::signal::kill(
            self.child_pid,
            nix::sys::signal::Signal::SIGKILL,
        );
        unsafe { libc::close(self.master_fd) };
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

pub struct PtyState {
    sessions: RwLock<HashMap<u32, Arc<Mutex<Session>>>>,
    next_id: AtomicU32,
}

impl Default for PtyState {
    fn default() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            next_id: AtomicU32::new(1),
        }
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn pty_open(
    state: tauri::State<PtyState>,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    // workspace is accepted but ignored on Android (no WSL)
    workspace: Option<crate::modules::workspace::WorkspaceEnv>,
    on_data: Channel<Response>,
    on_exit: Channel<i32>,
) -> Result<u32, String> {
    let _ = workspace;

    let proot = proot_bin();
    let rootfs = rootfs_dir();

    if !proot.exists() {
        return Err(
            "Bootstrap not complete. Call bootstrap_android from the frontend first.".into(),
        );
    }
    if !rootfs.exists() {
        return Err("Rootfs not found. Bootstrap may have failed.".into());
    }

    // ── Open PTY pair ────────────────────────────────────────────────────────
    let (master_fd, slave_fd) = open_pty_pair(cols, rows)?;

    // Working directory inside the rootfs
    let inner_cwd = cwd
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "/root".to_string());

    // ── Spawn proot ──────────────────────────────────────────────────────────
    // -0              → pretend to be root (uid=0) inside proot
    // -r <rootfs>     → use Alpine rootfs
    // -b /dev         → bind host /dev so /dev/ptmx works inside
    // -b /proc        → bind /proc for ps, top, etc.
    // -b /sys         → bind /sys
    // -b /data:/data  → expose host /data so we can read/write app files
    // -w <cwd>        → initial working directory
    let child = unsafe {
        std::process::Command::new(proot.to_str().unwrap())
            .args([
                "-0",
                "-r",
                rootfs.to_str().unwrap(),
                "-b", "/dev",
                "-b", "/proc",
                "-b", "/sys",
                "-b", "/data:/data",
                "-w", &inner_cwd,
                // Alpine's default shell
                "/bin/sh",
                "-l",
            ])
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERAX_TERMINAL", "1")
            .env("HOME", "/root")
            .env("USER", "root")
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .stdin(std::process::Stdio::from_raw_fd(slave_fd))
            .stdout(std::process::Stdio::from_raw_fd(slave_fd))
            .stderr(std::process::Stdio::from_raw_fd(slave_fd))
            .spawn()
    }
    .map_err(|e| format!("Failed to spawn proot: {e}"))?;

    // The child owns all three fd copies of slave. Close the parent's copy
    // so EOF propagates correctly when the child exits.
    unsafe { libc::close(slave_fd) };

    let child_pid = nix::unistd::Pid::from_raw(child.id() as i32);

    // ── Write channel: mpsc → master_fd ─────────────────────────────────────
    let (write_tx, write_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
    {
        // dup master_fd so the write thread owns its own fd copy.
        let write_master = unsafe { libc::dup(master_fd) };
        thread::Builder::new()
            .name("terax-pty-writer-android".into())
            .spawn(move || {
                let mut file = unsafe { std::fs::File::from_raw_fd(write_master) };
                for data in write_rx {
                    if file.write_all(&data).is_err() {
                        break;
                    }
                }
                // file drop closes write_master
            })
            .expect("spawn pty writer thread");
    }

    // ── Pending buffer shared between reader and flusher ────────────────────
    let pending: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(READ_BUF)));
    let done = Arc::new(AtomicBool::new(false));

    // ── Read thread: master_fd → pending buffer ──────────────────────────────
    {
        let pending_r = pending.clone();
        let read_master = unsafe { libc::dup(master_fd) };
        thread::Builder::new()
            .name("terax-pty-reader-android".into())
            .spawn(move || {
                let mut buf = [0u8; READ_BUF];
                let mut file = unsafe { std::fs::File::from_raw_fd(read_master) };
                let spawn_at = Instant::now();
                let mut logged_first = false;
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if !logged_first {
                                logged_first = true;
                                log::info!(
                                    "pty android: first byte after {}ms",
                                    spawn_at.elapsed().as_millis()
                                );
                            }
                            let mut g = pending_r.lock().unwrap();
                            if g.len() + n > MAX_PENDING {
                                g.clear();
                                g.extend_from_slice(OVERFLOW_NOTICE);
                            }
                            g.extend_from_slice(&buf[..n]);
                        }
                    }
                }
                // file drop closes read_master
            })
            .expect("spawn pty reader thread");
    }

    // ── Flush thread: pending buffer → Channel ───────────────────────────────
    {
        let pending_f = pending.clone();
        let done_f = done.clone();
        let on_data_flush = on_data.clone();
        thread::Builder::new()
            .name("terax-pty-flusher-android".into())
            .spawn(move || loop {
                thread::sleep(FLUSH_INTERVAL);
                let chunk = {
                    let mut g = pending_f.lock().unwrap();
                    if g.is_empty() {
                        if done_f.load(Ordering::Acquire) {
                            break;
                        }
                        continue;
                    }
                    std::mem::take(&mut *g)
                };
                if let Err(e) = on_data_flush.send(Response::new(chunk)) {
                    log::debug!("pty android flusher exiting: {e}");
                    break;
                }
            })
            .expect("spawn pty flusher thread");
    }

    // ── Waiter thread: wait for child exit → emit on_exit ───────────────────
    {
        let pending_e = pending;
        let done_e = done;
        let mut child_owned = child;
        thread::Builder::new()
            .name("terax-pty-waiter-android".into())
            .spawn(move || {
                let code = match child_owned.wait() {
                    Ok(status) => status.code().unwrap_or(-1),
                    Err(e) => {
                        log::warn!("pty android child wait failed: {e}");
                        -1
                    }
                };
                // Give the reader ~50ms to drain remaining output
                thread::sleep(Duration::from_millis(50));
                let tail = std::mem::take(&mut *pending_e.lock().unwrap());
                if !tail.is_empty() {
                    let _ = on_data.send(Response::new(tail));
                }
                done_e.store(true, Ordering::Release);
                let _ = on_exit.send(code);
            })
            .expect("spawn pty waiter thread");
    }

    let session = Session {
        master_fd,
        write_tx,
        child_pid,
    };

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    state
        .sessions
        .write()
        .unwrap()
        .insert(id, Arc::new(Mutex::new(session)));

    log::info!("pty android opened id={id} cols={cols} rows={rows}");
    Ok(id)
}

#[tauri::command]
pub fn pty_write(
    state: tauri::State<PtyState>,
    id: u32,
    data: String,
) -> Result<(), String> {
    let tx = {
        let sessions = state.sessions.read().unwrap();
        let session = sessions.get(&id).ok_or_else(|| {
            log::warn!("pty_write android: unknown id={id}");
            "no session".to_string()
        })?;
        let x = session.lock().unwrap().write_tx.clone(); x
    }; // sessions lock dropped here
    tx.send(data.into_bytes()).map_err(|e| e.to_string())
}





#[tauri::command]
pub fn pty_resize(
    state: tauri::State<PtyState>,
    id: u32,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let (master_fd, child_pid) = {
        let sessions = state.sessions.read().unwrap();
        let session = sessions.get(&id).ok_or_else(|| {
            log::warn!("pty_resize android: unknown id={id}");
            "no session".to_string()
        })?;
        let s = session.lock().unwrap();
        (s.master_fd, s.child_pid)
    }; // sessions lock dropped here

    let ws = libc::winsize {
        ws_col: cols,
        ws_row: rows,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let ret = unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws) };
    if ret != 0 {
        return Err(format!(
            "TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGWINCH);
    Ok(())
}


#[tauri::command]
pub fn pty_close(state: tauri::State<PtyState>, id: u32) -> Result<(), String> {
    let session = state.sessions.write().unwrap().remove(&id);
    if session.is_some() {
        // Drop fires Session::drop which kills the process and closes master_fd
        log::info!("pty android closed id={id}");
    } else {
        log::debug!("pty_close android: unknown id={id}");
    }
    Ok(())
    }
