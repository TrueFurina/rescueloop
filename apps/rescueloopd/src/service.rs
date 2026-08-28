use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::{fs, process::Command};

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.rescueloop.agent";

pub async fn install(incident_dir: &Path) -> Result<()> {
    install_using(incident_dir, None).await
}

pub async fn install_using(incident_dir: &Path, executable: Option<&Path>) -> Result<()> {
    let executable = match executable {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe().context("cannot resolve RescueLoop executable")?,
    };
    let incident_dir = absolute(incident_dir)?;
    #[cfg(target_os = "macos")]
    return install_macos(&executable, &incident_dir).await;
    #[cfg(target_os = "windows")]
    return install_windows(&executable, &incident_dir).await;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("background service installation currently supports macOS and Windows")
}

pub async fn install_to_path() -> Result<PathBuf> {
    let source = std::env::current_exe().context("cannot resolve RescueLoop executable")?;
    #[cfg(target_os = "macos")]
    {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?);
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).await?;
        let destination = bin.join("rescueloop");
        if source != destination {
            fs::copy(&source, &destination).await?;
        }
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755)).await?;
        ensure_unix_path(&home.join(".zprofile"), &bin).await?;
        ensure_unix_path(&home.join(".bash_profile"), &bin).await?;
        Ok(destination)
    }
    #[cfg(target_os = "windows")]
    {
        let local =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?);
        let bin = local.join("RescueLoop").join("bin");
        fs::create_dir_all(&bin).await?;
        let destination = bin.join("rescueloop.exe");
        if source != destination {
            fs::copy(&source, &destination).await?;
        }
        let current = std::env::var("PATH").unwrap_or_default();
        if !std::env::split_paths(&current).any(|path| path == bin) {
            let updated = if current.is_empty() {
                bin.to_string_lossy().into_owned()
            } else {
                format!("{};{}", bin.display(), current)
            };
            let status = Command::new("setx")
                .args(["PATH", &updated])
                .status()
                .await?;
            if !status.success() {
                bail!("could not add RescueLoop to the Windows user PATH")
            }
        }
        Ok(destination)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("PATH installation currently supports macOS and Windows")
}

#[cfg(target_os = "macos")]
async fn ensure_unix_path(profile: &Path, bin: &Path) -> Result<()> {
    let marker = "# RescueLoop PATH";
    let existing = fs::read_to_string(profile).await.unwrap_or_default();
    if existing.contains(marker) {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let addition = format!(
        "{separator}{marker}\nexport PATH=\"{}:$PATH\"\n",
        bin.display()
    );
    let mut updated = existing;
    updated.push_str(&addition);
    fs::write(profile, updated).await?;
    Ok(())
}

pub async fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    return uninstall_macos().await;
    #[cfg(target_os = "windows")]
    return uninstall_windows().await;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("background service installation currently supports macOS and Windows")
}

#[allow(clippy::needless_return)]
pub async fn restart_if_installed() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let plist = macos_plist()?;
        if !plist.exists() {
            return Ok(false);
        }
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let _ = Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(&plist)
            .status()
            .await;
        let status = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist)
            .status()
            .await?;
        return Ok(status.success());
    }
    #[cfg(target_os = "windows")]
    {
        let exists = Command::new("schtasks")
            .args(["/Query", "/TN", "RescueLoop"])
            .output()
            .await?
            .status
            .success();
        if !exists {
            return Ok(false);
        }
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", "RescueLoop"])
            .status()
            .await;
        return Ok(Command::new("schtasks")
            .args(["/Run", "/TN", "RescueLoop"])
            .status()
            .await?
            .success());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Ok(false)
}

#[allow(clippy::needless_return)]
pub async fn install_system(incident_dir: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::geteuid() } != 0 {
            bail!("system installation requires root; rerun this exact command with sudo")
        }
        let executable = std::env::current_exe()?;
        let incident_dir = absolute(incident_dir)?;
        let plist = PathBuf::from("/Library/LaunchDaemons").join(format!("{LABEL}.plist"));
        let path = std::env::var("PATH").unwrap_or_else(|_| {
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()
        });
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LABEL}</string>
<key>ProgramArguments</key><array><string>{}</string><string>--incident-dir</string><string>{}</string><string>watch</string></array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ProcessType</key><string>Background</string>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>
<key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
            xml(&executable),
            xml(&incident_dir),
            escape_xml_text(&path),
            "/dev/null",
            "/dev/null"
        );
        fs::write(&plist, content).await?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644)).await?;
        let _ = Command::new("launchctl")
            .args(["bootout", "system"])
            .arg(&plist)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        let status = Command::new("launchctl")
            .args(["bootstrap", "system"])
            .arg(&plist)
            .status()
            .await?;
        if !status.success() {
            bail!("launchctl could not start the system RescueLoop daemon")
        }
        println!(
            "System watcher installed with Unified Log access: {}",
            plist.display()
        );
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    return install(incident_dir).await;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("system installation supports macOS and Windows")
}

#[allow(clippy::needless_return)]
pub async fn uninstall_system() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::geteuid() } != 0 {
            bail!("system uninstall requires root")
        }
        let plist = PathBuf::from("/Library/LaunchDaemons").join(format!("{LABEL}.plist"));
        if plist.exists() {
            let _ = Command::new("launchctl")
                .args(["bootout", "system"])
                .arg(&plist)
                .status()
                .await;
            fs::remove_file(&plist).await?;
        }
        println!("System watcher uninstalled.");
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    return uninstall().await;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("system uninstall supports macOS and Windows")
}

#[allow(clippy::needless_return)]
pub async fn status() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let path = macos_plist()?;
        let system_path = PathBuf::from("/Library/LaunchDaemons").join(format!("{LABEL}.plist"));
        println!(
            "User watcher: {}\nUser definition: {}\nSystem watcher: {}\nSystem definition: {}",
            if path.exists() {
                "installed"
            } else {
                "not installed"
            },
            path.display(),
            if system_path.exists() {
                "installed"
            } else {
                "not installed"
            },
            system_path.display()
        );
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let status = Command::new("schtasks")
            .args(["/Query", "/TN", "RescueLoop"])
            .status()
            .await?;
        println!(
            "Background watcher: {}",
            if status.success() {
                "installed"
            } else {
                "not installed"
            }
        );
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("background service installation currently supports macOS and Windows")
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(target_os = "macos")]
fn macos_plist() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is unavailable")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
async fn install_macos(executable: &Path, incident_dir: &Path) -> Result<()> {
    let plist = macos_plist()?;
    let parent = plist.parent().context("LaunchAgents path has no parent")?;
    fs::create_dir_all(parent).await?;
    let path = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into());
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LABEL}</string>
<key>ProgramArguments</key><array>
<string>{}</string><string>--incident-dir</string><string>{}</string><string>watch</string>
</array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
<key>ProcessType</key><string>Background</string>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>
<key>StandardOutPath</key><string>{}</string>
<key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        xml(executable),
        xml(incident_dir),
        path.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
        "/dev/null",
        "/dev/null",
    );
    fs::write(&plist, content).await?;
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &domain])
        .arg(&plist)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist)
        .status()
        .await?;
    if !status.success() {
        bail!("launchctl could not start RescueLoop")
    }
    println!(
        "Background watcher installed and started: {}",
        plist.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
async fn uninstall_macos() -> Result<()> {
    let plist = macos_plist()?;
    if plist.exists() {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let _ = Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(&plist)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        fs::remove_file(&plist).await?;
    }
    println!("Background watcher uninstalled.");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn install_windows(executable: &Path, incident_dir: &Path) -> Result<()> {
    let task = format!(
        "\"{}\" --incident-dir \"{}\" watch",
        executable.display(),
        incident_dir.display()
    );
    let status = Command::new("schtasks")
        .args([
            "/Create",
            "/F",
            "/SC",
            "ONLOGON",
            "/TN",
            "RescueLoop",
            "/TR",
            &task,
        ])
        .status()
        .await?;
    if !status.success() {
        bail!("Windows Task Scheduler could not install RescueLoop")
    }
    println!("Background watcher installed for the current Windows user.");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn uninstall_windows() -> Result<()> {
    let status = Command::new("schtasks")
        .args(["/Delete", "/F", "/TN", "RescueLoop"])
        .status()
        .await?;
    if !status.success() {
        bail!("Windows Task Scheduler could not remove RescueLoop")
    }
    println!("Background watcher uninstalled.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml(path: &Path) -> String {
    escape_xml_text(&path.to_string_lossy())
}

#[cfg(target_os = "macos")]
fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
