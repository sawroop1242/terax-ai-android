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
    // यह सुनिश्चित करें कि प्रूट बाइनरी और अल्पाइन का मुख्य शेल दोनों मौजूद हैं
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

    // ── 1. Extract proot binary using memory bytes ───────────────────────────
    if !proot_dest.exists() {
        log::info!("bootstrap_android: Extracting proot binary bytes...");
        // resolve_resource का उपयोग APK के अंदर से सीधे बाइट्स पढ़ने के लिए करें
    

                // Tauri v2 में .path().resource() का उपयोग करके वास्तविक पाथ निकालें
        let proot_resource_path = app
            .path()
            .resource("assets/android/proot-aarch64")
            .map_err(|e| format!("resolve proot asset path failed: {e}"))?;

        // पाथ मिलने के बाद std::fs::read का उपयोग करके फ़ाइल के बाइट्स पढ़ें
        let proot_bytes = std::fs::read(&proot_resource_path)
            .map_err(|e| format!("failed to read proot asset bytes from disk: {e}"))?;

        fs::write(&proot_dest, proot_bytes)
            .map_err(|e| format!("write proot binary: {e}"))?;
        

        

        // Executable परमिशन (0o755) सेट करें ताकि Permission Denied (Error 13) न आए
        fs::set_permissions(&proot_dest, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod proot: {e}"))?;

        log::info!("bootstrap_android: proot binary installed at {}", proot_dest.display());
    }

    // ── 2. Extract Alpine rootfs tarball using intermediate tmp file ─────────
    let alpine_check = rootfs_dest.join("bin/sh");
    if !alpine_check.exists() {
        log::info!("bootstrap_android: Extracting alpine tarball bytes...");
        fs::create_dir_all(&rootfs_dest).map_err(|e| format!("create rootfs dir: {e}"))?;

        

                // ठीक इसी तरह Alpine tarball का भी पाथ निकालें
        let tarball_resource_path = app
            .path()
            .resource("assets/android/alpine-rootfs.tar.gz")
            .map_err(|e| format!("resolve rootfs asset path failed: {e}"))?;

        // std::fs::read का उपयोग करके .tar.gz के बाइट्स लोड करें
        let tarball_bytes = std::fs::read(&tarball_resource_path)
            .map_err(|e| format!("failed to read rootfs asset bytes from disk: {e}"))?;

        // इसके बाद का आपका बाकी कोड (tmp_tarball_path में राइट करना और tar चलाना) वैसा ही रहेगा
        let tmp_tarball_path = base.join("alpine-tmp.tar.gz");
        fs::write(&tmp_tarball_path, tarball_bytes)
            .map_err(|e| format!("write intermediate tarball: {e}"))?;
        

        log::info!("bootstrap_android: Running system tar extraction...");
        let status = std::process::Command::new("tar")
            .args([
                "-xzf",
                tmp_tarball_path.to_str().unwrap(),
                "-C",
                rootfs_dest.to_str().unwrap(),
            ])
            .status()
            .map_err(|e| format!("spawn tar: {e}"))?;

        // काम पूरा होने के बाद अस्थायी फ़ाइल को तुरंत हटा दें
        let _ = fs::remove_file(tmp_tarball_path);

        if !status.success() {
            return Err(format!(
                "tar extraction failed with exit code: {:?}",
                status.code()
            ));
        }

        log::info!("bootstrap_android: rootfs extracted to {}", rootfs_dest.display());
    }

    // ── 3. Write /etc/resolv.conf for DNS inside proot ────────────────────────
    let resolv_path = rootfs_dest.join("etc/resolv.conf");
    if let Some(parent) = resolv_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut resolv = fs::File::create(&resolv_path)
        .map_err(|e| format!("create resolv.conf: {e}"))?;
    writeln!(resolv, "nameserver 8.8.8.8").map_err(|e| e.to_string())?;
    writeln!(resolv, "nameserver 1.1.1.1").map_err(|e| e.to_string())?;

    // ── 4. Write /etc/profile.d/terax.sh to set useful env vars ─────────────
    let profile_d = rootfs_dest.join("etc/profile.d");
    let _ = fs::create_dir_all(&profile_d);
    let terax_profile = profile_d.join("terax.sh");
    let mut pf = fs::File::create(&terax_profile)
        .map_err(|e| format!("create terax profile: {e}"))?;
    writeln!(pf, "export TERM=xterm-256color").map_err(|e| e.to_string())?;
    writeln!(pf, "export COLORTERM=truecolor").map_err(|e| e.to_string())?;
    writeln!(pf, "export TERAX_TERMINAL=1").map_err(|e| e.to_string())?;
    writeln!(pf, "export LANG=C.UTF-8").map_err(|e| e.to_string())?;

    log::info!("bootstrap_android: complete");
    Ok("bootstrapped".into())
}
