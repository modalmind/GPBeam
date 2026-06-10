use crate::error::Result;
use std::path::Path;

/// Seam for ejecting/unmounting a removable volume. Object-safe + Send+Sync so
/// it can live behind `Box<dyn Ejector>` and be shared across threads.
pub trait Ejector: Send + Sync {
    fn eject(&self, mount: &Path) -> Result<()>;
}

/// Real implementation: shells out to the platform's volume tool.
pub struct SystemEjector;

impl Ejector for SystemEjector {
    #[cfg(target_os = "macos")]
    fn eject(&self, mount: &Path) -> Result<()> {
        run_cmd(
            std::process::Command::new("diskutil")
                .arg("unmount")
                .arg(mount),
            mount,
        )
    }

    #[cfg(target_os = "windows")]
    fn eject(&self, mount: &Path) -> Result<()> {
        // `mount` is a drive root like "E:\\"; take the "E:" drive spec.
        let drive = mount.to_string_lossy();
        let drive = drive.trim_end_matches(['\\', '/']);
        let ps = format!(
            "(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{drive}').InvokeVerb('Eject')"
        );
        run_cmd(
            std::process::Command::new("powershell").args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &ps,
            ]),
            mount,
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn eject(&self, mount: &Path) -> Result<()> {
        run_cmd(
            std::process::Command::new("udisksctl")
                .arg("unmount")
                .arg("-b")
                .arg(mount),
            mount,
        )
    }
}

/// Run a Command and map non-success / spawn failure to a CoreError.
#[allow(dead_code)]
fn run_cmd(cmd: &mut std::process::Command, mount: &Path) -> Result<()> {
    let status = cmd
        .status()
        .map_err(crate::error::io_at(mount.to_path_buf()))?;
    if status.success() {
        Ok(())
    } else {
        Err(crate::error::CoreError::Config(format!(
            "eject of {} failed with status {:?}",
            mount.display(),
            status.code()
        )))
    }
}

/// The platform's default ejector.
pub fn default_ejector() -> Box<dyn Ejector> {
    Box::new(SystemEjector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Mock ejector: records every mount it was asked to eject; spawns nothing.
    struct MockEjector {
        calls: Mutex<Vec<PathBuf>>,
        result: Result<()>,
    }

    impl MockEjector {
        fn ok() -> Self {
            MockEjector {
                calls: Mutex::new(Vec::new()),
                result: Ok(()),
            }
        }
    }

    impl Ejector for MockEjector {
        fn eject(&self, mount: &Path) -> Result<()> {
            self.calls.lock().unwrap().push(mount.to_path_buf());
            // `Result<()>` is not Clone; reconstruct the same variant.
            match &self.result {
                Ok(()) => Ok(()),
                Err(_) => Err(crate::error::CoreError::Config("mock eject failed".into())),
            }
        }
    }

    #[test]
    fn mock_records_call_without_touching_hardware() {
        let m = MockEjector::ok();
        let mount = PathBuf::from("/Volumes/GOPRO");
        m.eject(&mount).unwrap();
        assert_eq!(m.calls.lock().unwrap().as_slice(), &[mount]);
    }

    #[test]
    fn default_ejector_is_constructible() {
        let _e: Box<dyn Ejector> = default_ejector();
    }
}
