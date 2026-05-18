// src-tauri/src/bootstrap.rs

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

fn files_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .expect("app_data_dir unavailable on Android")
}

#[tauri::command]
pub fn bootstrap_status(app: AppHandle) -> bool {
    let base = files_dir(&app);
    base.join("proot").exists() && base.join("rootfs/bin/sh").exists()
}

#[tauri::command]
pub async fn bootstrap_android(app: AppHandle) -> Result<String, String> {
    let base = files_dir(&app);
    let proot_dest = base.join("proot");
    let rootfs_dest = base.join("rootfs");

    if bootstrap_status(app.clone()) {
        log::info!("bootstrap_android: already complete, skipping");
        return Ok("already_bootstrapped".into());
    }

    log::info!("bootstrap_android: starting first-run bootstrap");
    fs::create_dir_all(&base).map_err(|e| format!("create files dir: {e}"))?;

    // ── 1. Copy proot binary from asset_resolver ───────────────────────────
    if !proot_dest.exists() {
        // Tauri v2 कोर एसेट रिसॉल्वर का उपयोग करके APK से सीधे बाइट्स पढ़ें
        let asset_resolver = app.asset_resolver();
        let proot_asset = asset_resolver
            .get("assets/android/proot-aarch64".to_string())
            .ok_or_else(|| "proot-aarch64 not found in app assets".to_string())?;

        // प्रूट बाइनरी को इंटरनल डिस्क पर लिखें
        fs::write(&proot_dest, &proot_asset.bytes)
            .map_err(|e| format!("write proot binary: {e}"))?;

        // Executable परमिशन (0o755) सेट करें (OS Error 13 से बचने के लिए)
        fs::set_permissions(&proot_dest, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod proot: {e}"))?;

        log::info!("bootstrap_android: proot binary installed");
    }

    // ── 2. Extract Alpine rootfs tarball via temporary file ──────────────────
    let alpine_check = rootfs_dest.join("bin/sh");
    if !alpine_check.exists() {
        fs::create_dir_all(&rootfs_dest).map_err(|e| format!("create rootfs dir: {e}"))?;

        let asset_resolver = app.asset_resolver();
        let tarball_asset = asset_resolver
            .get("assets/android/alpine-rootfs.tar.gz".to_string())
            .ok_or_else(|| "alpine-rootfs.tar.gz not found in app assets".to_string())?;

        // सिस्टम 'tar' को रीडेबल पाथ देने के लिए इसे टेम्परेरी फाइल में सहेजें
        let tmp_tarball_path = base.join("alpine-tmp.tar.gz");
        fs::write(&tmp_tarball_path, &tarball_asset.bytes)
            .map_err(|e| format!("write intermediate tarball: {e}"))?;

        // Android के सिस्टम 'tar' टूल का उपयोग करके अनपैक करें
        let status = std::process::Command::new("tar")
            .args([
                "xzf",
                tmp_tarball_path.to_str().unwrap(),
                "-C",
                rootfs_dest.to_str().unwrap(),
            ])
            .status()
            .map_err(|e| format!("spawn tar: {e}"))?;

        // काम होने के बाद अस्थायी फ़ाइल को तुरंत हटा दें
        let _ = fs::remove_file(tmp_tarball_path);

        if !status.success() {
            return Err(format!("tar extraction failed: {:?}", status.code()));
        }
        log::info!("bootstrap_android: rootfs extracted");
    }

    // ── 3. Write /etc/resolv.conf ──────────────────────────────────────────
    let resolv_path = rootfs_dest.join("etc/resolv.conf");
    if let Some(parent) = resolv_path.parent() { let _ = fs::create_dir_all(parent); }
    let mut resolv = fs::File::create(&resolv_path).map_err(|e| format!("create resolv.conf: {e}"))?;
    writeln!(resolv, "nameserver 8.8.8.8").map_err(|e| e.to_string())?;
    writeln!(resolv, "nameserver 1.1.1.1").map_err(|e| e.to_string())?;

    // ── 4. Write /etc/profile.d/terax.sh ───────────────────────────────────
    let profile_d = rootfs_dest.join("etc/profile.d");
    let _ = fs::create_dir_all(&profile_d);
    let mut pf = fs::File::create(profile_d.join("terax.sh")).map_err(|e| format!("create profile: {e}"))?;
    writeln!(pf, "export TERM=xterm-256color\nexport COLORTERM=truecolor\nexport TERAX_TERMINAL=1\nexport LANG=C.UTF-8").map_err(|e| e.to_string())?;

    log::info!("bootstrap_android: complete");
    Ok("bootstrapped".into())
}
