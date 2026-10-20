//! launchd integration so a node comes up automatically after a reboot.

use std::path::PathBuf;

pub const LABEL: &str = "io.hive.agent";

pub fn launchd_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Library/LaunchAgents")
}

pub fn plist_path() -> PathBuf {
    launchd_dir().join(format!("{}.plist", LABEL))
}

pub fn plist_xml(binary: &str, config_path: &str, log_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
        <string>--config</string>
        <string>{config}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        binary = binary,
        config = config_path,
        log = log_path
    )
}

pub fn install(binary: &str, config_path: &str, log_path: &str, dry_run: bool) -> std::io::Result<()> {
    let xml = plist_xml(binary, config_path, log_path);
    if dry_run {
        print!("{}", xml);
        return Ok(());
    }
    let dir = launchd_dir();
    std::fs::create_dir_all(&dir)?;
    if let Some(parent) = std::path::Path::new(log_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = plist_path();
    std::fs::write(&path, xml)?;
    println!("wrote {}", path.display());
    Ok(())
}

pub fn uninstall(dry_run: bool) -> std::io::Result<()> {
    let path = plist_path();
    if dry_run {
        println!("would remove {}", path.display());
        return Ok(());
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("removed {}", path.display());
    }
    Ok(())
}
