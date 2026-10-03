use std::path::Path;
use std::process::Command;

const SCRIPT: &str = include_str!("../../../plugins/opencode/runtime/dashboard-acl.ps1");

pub fn validate_private_path(path: &Path) -> std::io::Result<()> {
    let root = std::env::var_os("SystemRoot")
        .ok_or_else(|| std::io::Error::other("Windows SystemRoot unavailable"))?;
    let executable = Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !Path::new(&root).is_absolute() || !path.is_absolute() {
        return Err(std::io::Error::other("Windows ACL path must be absolute"));
    }
    let status = Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env_clear()
        .env("SystemRoot", root)
        .env("RTRT_ACL_PATH", path)
        .env("RTRT_ACL_ACTION", "private-check")
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(
            "dashboard Windows ACL is not private",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_state_accepts_owner_only_and_rejects_inherited_acl() {
        // Given: a PowerShell 7 module path inherited by the Rust test process.
        if std::env::var_os("RTRT_TEST_WINDOWS_ACL_CHILD").is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows_acl::tests::private_state_accepts_owner_only_and_rejects_inherited_acl",
                    "--nocapture",
                ])
                .env("PSModulePath", r"C:\Program Files\PowerShell\7\Modules")
                .env("RTRT_TEST_WINDOWS_ACL_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .contains("test result: ok. 1 passed"),
                "Windows ACL fixture failed under PS7 module path: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // Given: an isolated private fixture, never an operator profile.
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.rtrt/tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let fixture = parent.join(format!(
            "dashboard-acl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&fixture).unwrap();
        let root = std::env::var_os("SystemRoot").unwrap();
        let powershell = Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let create = Command::new(&powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .env_clear()
            .env("SystemRoot", &root)
            .env("RTRT_ACL_PATH", &fixture)
            .env("RTRT_ACL_ACTION", "private-create")
            .status()
            .unwrap();
        assert!(create.success());
        let token = fixture.join("dashboard.env");
        std::fs::File::create(&token).unwrap();
        let protect_token = Command::new(&powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .env_clear()
            .env("SystemRoot", &root)
            .env("RTRT_ACL_PATH", &token)
            .env("RTRT_ACL_ACTION", "private-create")
            .status()
            .unwrap();
        assert!(protect_token.success());
        // When: startup independently checks the directory ACL.
        assert!(validate_private_path(&fixture).is_ok());
        assert!(validate_private_path(&token).is_ok());
        let inherited = Command::new(&powershell)
            .args([
                "-NoProfile", "-NonInteractive", "-Command",
                "$a=Get-Acl -LiteralPath $env:RTRT_ACL_PATH; $a.SetAccessRuleProtection($false,$true); Set-Acl -LiteralPath $env:RTRT_ACL_PATH -AclObject $a",
            ])
            .env_clear()
            .env("SystemRoot", &root)
            .env("RTRT_ACL_PATH", &token)
            .status()
            .unwrap();
        assert!(inherited.success());
        // Then: permissive inheritance is rejected before content can be read.
        assert!(validate_private_path(&token).is_err());
        std::fs::remove_file(&token).unwrap();
        std::fs::remove_dir(&fixture).unwrap();
    }
}
