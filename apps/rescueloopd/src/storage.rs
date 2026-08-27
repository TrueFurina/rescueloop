use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub fn prepare_state_store(incident_dir: &Path) -> Result<PathBuf> {
    let root = state_root(incident_dir);
    if root.parent().is_none() {
        bail!("a filesystem root cannot be used as RescueLoop state storage")
    }
    let existed = root.exists();
    std::fs::create_dir_all(&root).with_context(|| {
        format!(
            "cannot create RescueLoop state directory: {}",
            root.display()
        )
    })?;
    let managed_name = root.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().to_ascii_lowercase().as_str(),
            ".rescueloop" | "rescueloop"
        )
    });
    secure_directory(&root, !existed || managed_name)?;
    Ok(root)
}

pub fn prepare_mcp_store(incident_dir: &Path) -> Result<PathBuf> {
    if !incident_dir.is_absolute() {
        bail!("MCP requires an absolute --incident-dir path")
    }
    let root = prepare_state_store(incident_dir)?;
    std::fs::create_dir_all(incident_dir).with_context(|| {
        format!(
            "cannot create RescueLoop incident directory: {}",
            incident_dir.display()
        )
    })?;
    secure_directory(incident_dir, true)?;
    let canonical_root = root
        .canonicalize()
        .context("cannot resolve RescueLoop state root")?;
    let canonical_incidents = incident_dir
        .canonicalize()
        .context("cannot resolve RescueLoop incident directory")?;
    if !canonical_incidents.starts_with(&canonical_root) || canonical_incidents == canonical_root {
        bail!("incident directory must be a strict descendant of the RescueLoop state root")
    }
    Ok(canonical_incidents)
}

fn state_root(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(incident_dir)
        .to_path_buf()
}

#[cfg(unix)]
fn secure_directory(path: &Path, may_replace_permissions: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("security-sensitive state path must be a real directory")
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!("RescueLoop state directory is not owned by the current user")
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        if !may_replace_permissions {
            bail!(
                "custom RescueLoop state directory must already have owner-only (0700) permissions"
            )
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let verified = std::fs::symlink_metadata(path)?;
    if verified.permissions().mode() & 0o077 != 0 {
        bail!("RescueLoop state directory remains accessible to group or other users")
    }
    Ok(())
}

#[cfg(windows)]
fn secure_directory(path: &Path, may_replace_permissions: bool) -> Result<()> {
    use std::process::Command;

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("security-sensitive state path must be a real directory")
    }
    if !may_replace_permissions {
        bail!(
            "custom Windows state roots must be created by RescueLoop or named .rescueloop/RescueLoop"
        )
    }
    let whoami = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .context("cannot determine the current Windows security identifier")?;
    if !whoami.status.success() {
        bail!("whoami could not determine the current Windows security identifier")
    }
    let output = String::from_utf8(whoami.stdout).context("whoami returned invalid text")?;
    let sid = output
        .split(',')
        .nth(1)
        .map(|value| value.trim().trim_matches('"'))
        .filter(|value| value.starts_with("S-1-"))
        .context("cannot parse the current Windows security identifier")?;
    let principal = format!("*{sid}:(OI)(CI)F");
    let status = Command::new("icacls")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &principal,
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .status()
        .context("cannot apply a private Windows ACL to RescueLoop state")?;
    if !status.success() {
        bail!("Windows refused the private RescueLoop state ACL")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn state_store_is_private_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let base =
            std::env::temp_dir().join(format!("rescueloop-storage-{}", uuid::Uuid::new_v4()));
        let state = base.join(".rescueloop");
        let incidents = state.join("incidents");
        std::fs::create_dir_all(&incidents).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_state_store(&incidents).unwrap();
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let custom = base.join("custom");
        std::fs::create_dir(&custom).unwrap();
        std::fs::set_permissions(&custom, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_state_store(&custom.join("incidents")).is_err());
        assert_eq!(
            std::fs::metadata(&custom).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = base.join("linked-state");
        symlink(&target, &link).unwrap();
        assert!(prepare_state_store(&link.join("incidents")).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_store_has_private_acl_principals() {
        use std::process::Command;

        let base =
            std::env::temp_dir().join(format!("rescueloop-storage-{}", uuid::Uuid::new_v4()));
        let incidents = base.join(".rescueloop").join("incidents");
        prepare_state_store(&incidents).unwrap();
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$acl=Get-Acl -LiteralPath $env:RESCUELOOP_TEST_ACL; $acl.AreAccessRulesProtected; $acl.Access | ForEach-Object { $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value }",
            ])
            .env("RESCUELOOP_TEST_ACL", base.join(".rescueloop"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let acl = String::from_utf8_lossy(&output.stdout);
        assert!(acl.lines().next().is_some_and(|line| line.trim() == "True"));
        assert!(acl.contains("S-1-5-18"));
        assert!(acl.contains("S-1-5-32-544"));
        let whoami = Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        let identity = String::from_utf8(whoami.stdout).unwrap();
        let current_sid = identity.split(',').nth(1).unwrap().trim().trim_matches('"');
        assert!(acl.contains(current_sid));
        std::fs::remove_dir_all(base).unwrap();
    }
}
