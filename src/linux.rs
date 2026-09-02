use crate::{AutoLaunch, Error, LinuxLaunchMode, Result};
use std::{fs, io::Write, path::PathBuf};

/// Linux implement
impl AutoLaunch {
    /// Create a new AutoLaunch instance
    /// - `app_name`: application name
    /// - `app_path`: application path
    /// - `launch_mode`: launch mode (XDG Autostart or systemd)
    /// - `args`: startup args passed to the binary
    ///
    /// ## Notes
    ///
    /// The parameters of `AutoLaunch::new` are different on each platform.
    pub fn new(
        app_name: &str,
        app_path: &str,
        launch_mode: LinuxLaunchMode,
        args: &[impl AsRef<str>],
    ) -> AutoLaunch {
        AutoLaunch {
            app_name: app_name.into(),
            app_path: app_path.into(),
            launch_mode,
            args: args.iter().map(|s| s.as_ref().to_string()).collect(),
        }
    }

    /// Enable the AutoLaunch setting
    ///
    /// Refuses to overwrite an existing registration that was not created by
    /// this library (see [`Self::is_registration_owned`]); use
    /// [`Self::enable_force`] to take over such a registration.
    ///
    /// ## Errors
    ///
    /// - [`crate::Error::RegistrationNotOwned`]: a non-library file exists at
    ///   the registration path
    /// - failed to create directory
    /// - failed to create file
    /// - failed to write bytes to the file
    /// - failed to enable systemd service (if using systemd mode)
    pub fn enable(&self) -> Result<()> {
        self.enable_with_force(false)
    }

    /// Like [`Self::enable`], but unconditionally overwrites any existing
    /// registration, including manually managed files.
    pub fn enable_force(&self) -> Result<()> {
        self.enable_with_force(true)
    }

    /// Whether the existing registration was created by this library.
    ///
    /// Checks for the `# Managed by ...` marker written by this library
    /// (marker method below). Files without the marker, e.g. hand-written
    /// units, are considered manually managed and return `false`. A missing
    /// file returns `false`.
    pub fn is_registration_owned(&self) -> Result<bool> {
        let file = match self.launch_mode {
            LinuxLaunchMode::XdgAutostart => self.get_xdg_desktop_file()?,
            LinuxLaunchMode::SystemdUser | LinuxLaunchMode::SystemdSystem => {
                self.get_systemd_service_file()?
            }
        };
        if !file.exists() {
            return Ok(false);
        }
        let content = fs::read_to_string(file)?;
        Ok(content_is_managed(&content, &self.managed_marker()))
    }

    fn enable_with_force(&self, force: bool) -> Result<()> {
        match self.launch_mode {
            LinuxLaunchMode::XdgAutostart => self.enable_xdg_autostart(force),
            LinuxLaunchMode::SystemdUser | LinuxLaunchMode::SystemdSystem => {
                self.enable_systemd(force)
            }
        }
    }

    /// Enable using XDG Autostart (.desktop file)
    fn enable_xdg_autostart(&self, force: bool) -> Result<()> {
        let file_path = self.get_xdg_desktop_file()?;
        if !force
            && file_path.exists()
            && !content_is_managed(&fs::read_to_string(&file_path)?, &self.managed_marker())
        {
            return Err(Error::RegistrationNotOwned(file_path));
        }
        let data = build_xdg_autostart_data(
            &self.app_name,
            &self.app_path,
            &self.args,
            &self.managed_marker(),
        );

        let dir = get_xdg_autostart_dir()?;
        if !dir.exists() {
            fs::create_dir_all(&dir).or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(file_path)?;
        file.write_all(data.as_bytes())?;
        Ok(())
    }

    /// Enable using systemd service
    fn enable_systemd(&self, force: bool) -> Result<()> {
        let service_file = self.get_systemd_service_file()?;
        if !force
            && service_file.exists()
            && !content_is_managed(&fs::read_to_string(&service_file)?, &self.managed_marker())
        {
            return Err(Error::RegistrationNotOwned(service_file));
        }
        // Create systemd service file content
        let data = build_systemd_service_data(
            &self.app_name,
            &self.app_path,
            &self.args,
            self.launch_mode,
            &self.managed_marker(),
        );

        // Create systemd directory
        let dir = get_systemd_dir(self.launch_mode)?;
        if !dir.exists() {
            fs::create_dir_all(&dir).or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
        }

        // Write service file
        let service_file = self.get_systemd_service_file()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&service_file)?;
        file.write_all(data.as_bytes())?;

        // Reload systemd daemon so it picks up the new service file
        let daemon_reload_args: &[&str] = match self.launch_mode {
            LinuxLaunchMode::SystemdUser => &["--user", "daemon-reload"],
            LinuxLaunchMode::SystemdSystem => &["daemon-reload"],
            LinuxLaunchMode::XdgAutostart => unreachable!("XDG mode does not use systemctl"),
        };
        let _ = std::process::Command::new("systemctl")
            .args(daemon_reload_args)
            .output();

        // Enable and start the service using systemctl
        self.systemctl_enable()?;

        Ok(())
    }

    /// Run systemctl enable command.
    fn systemctl_enable(&self) -> Result<()> {
        let service_name = format!("{}.service", self.app_name);
        let args: &[&str] = match self.launch_mode {
            LinuxLaunchMode::SystemdUser => &["--user", "enable", &service_name],
            LinuxLaunchMode::SystemdSystem => &["enable", &service_name],
            LinuxLaunchMode::XdgAutostart => unreachable!("XDG mode does not use systemctl"),
        };
        let output = std::process::Command::new("systemctl")
            .args(args)
            .output()?;

        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to enable systemd service: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }

        Ok(())
    }

    /// Disable the AutoLaunch setting
    ///
    /// ## Errors
    ///
    /// - failed to remove file
    /// - failed to disable systemd service (if using systemd mode)
    pub fn disable(&self) -> Result<()> {
        match self.launch_mode {
            LinuxLaunchMode::XdgAutostart => self.disable_xdg_autostart(),
            LinuxLaunchMode::SystemdUser | LinuxLaunchMode::SystemdSystem => self.disable_systemd(),
        }
    }

    /// Disable XDG Autostart
    fn disable_xdg_autostart(&self) -> Result<()> {
        let file = self.get_xdg_desktop_file()?;
        if file.exists() {
            fs::remove_file(file)?;
        }
        Ok(())
    }

    /// Disable systemd service
    fn disable_systemd(&self) -> Result<()> {
        // Disable the service
        self.systemctl_disable()?;

        // Remove service file
        let service_file = self.get_systemd_service_file()?;
        if service_file.exists() {
            fs::remove_file(service_file)?;
        }

        // Reload systemd daemon
        let daemon_reload_args: &[&str] = match self.launch_mode {
            LinuxLaunchMode::SystemdUser => &["--user", "daemon-reload"],
            LinuxLaunchMode::SystemdSystem => &["daemon-reload"],
            LinuxLaunchMode::XdgAutostart => unreachable!("XDG mode does not use systemctl"),
        };
        let _ = std::process::Command::new("systemctl")
            .args(daemon_reload_args)
            .output();

        Ok(())
    }

    /// Run systemctl disable command.
    fn systemctl_disable(&self) -> Result<()> {
        let service_name = format!("{}.service", self.app_name);
        let args: &[&str] = match self.launch_mode {
            LinuxLaunchMode::SystemdUser => &["--user", "disable", &service_name],
            LinuxLaunchMode::SystemdSystem => &["disable", &service_name],
            LinuxLaunchMode::XdgAutostart => unreachable!("XDG mode does not use systemctl"),
        };
        let output = std::process::Command::new("systemctl")
            .args(args)
            .output()?;

        // Don't fail if the service is not enabled
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("No such file or directory") && !stderr.contains("not loaded") {
                let err_msg = format!("Failed to disable systemd service: {}", stderr);
                return Err(std::io::Error::other(err_msg).into());
            }
        }

        Ok(())
    }

    /// Check whether the AutoLaunch setting is enabled
    pub fn is_enabled(&self) -> Result<bool> {
        match self.launch_mode {
            LinuxLaunchMode::XdgAutostart => Ok(self.get_xdg_desktop_file()?.exists()),
            LinuxLaunchMode::SystemdUser | LinuxLaunchMode::SystemdSystem => {
                self.is_systemd_enabled()
            }
        }
    }

    /// Check if systemd service is enabled
    fn is_systemd_enabled(&self) -> Result<bool> {
        let service_name = format!("{}.service", self.app_name);
        let args: &[&str] = match self.launch_mode {
            LinuxLaunchMode::SystemdUser => &["--user", "is-enabled", &service_name],
            LinuxLaunchMode::SystemdSystem => &["is-enabled", &service_name],
            LinuxLaunchMode::XdgAutostart => unreachable!("XDG mode does not use systemctl"),
        };
        let output = std::process::Command::new("systemctl")
            .args(args)
            .output()?;

        // systemctl is-enabled returns:
        // - "enabled" with exit code 0 if enabled
        // - "disabled" with exit code 1 if disabled
        // - other states or errors with other exit codes
        Ok(output.status.success())
    }

    /// Read the registered `app_path` from the on-disk service/desktop file.
    ///
    /// Returns `Ok(None)` when the registration does not exist.
    /// For systemd, extracts the binary from the `ExecStart=` line.
    /// For XDG autostart, extracts it from the `Exec=` line.
    pub fn get_registered_app_path(&self) -> Result<Option<String>> {
        let file = match self.launch_mode {
            LinuxLaunchMode::XdgAutostart => self.get_xdg_desktop_file()?,
            LinuxLaunchMode::SystemdUser | LinuxLaunchMode::SystemdSystem => {
                self.get_systemd_service_file()?
            }
        };
        if !file.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(file)?;
        let key = match self.launch_mode {
            LinuxLaunchMode::XdgAutostart => "Exec=",
            _ => "ExecStart=",
        };
        let path = content
            .lines()
            .find_map(|line| {
                let trimmed = line.trim();
                trimmed.strip_prefix(key).map(|rest| {
                    // ExecStart=/path/to/bin arg1 arg2
                    // Take the first whitespace-delimited token.
                    rest.split_whitespace().next().map(|s| s.to_string())
                })
            })
            .flatten();
        Ok(path)
    }

    /// Get the XDG desktop entry file path
    fn get_xdg_desktop_file(&self) -> Result<PathBuf> {
        Ok(get_xdg_autostart_dir()?.join(format!("{}.desktop", self.app_name)))
    }

    /// Get the systemd service file path
    fn get_systemd_service_file(&self) -> Result<PathBuf> {
        Ok(get_systemd_dir(self.launch_mode)?.join(format!("{}.service", self.app_name)))
    }
}

fn build_xdg_autostart_data(
    app_name: &str,
    app_path: &str,
    args: &[String],
    managed_marker: &str,
) -> String {
    format!(
        "# {}. Manual edits will be overwritten.\n\
        [Desktop Entry]\n\
        Type=Application\n\
        Version=1.0\n\
        Name={}\n\
        Comment={} startup script\n\
        Exec={} {}\n\
        StartupNotify=false\n\
        Terminal=false",
        managed_marker,
        app_name,
        app_name,
        app_path,
        args.join(" ")
    )
}

fn build_systemd_service_data(
    app_name: &str,
    app_path: &str,
    args: &[String],
    mode: LinuxLaunchMode,
    managed_marker: &str,
) -> String {
    let args_str = if args.is_empty() {
        String::new()
    } else {
        format!(" {}", args.join(" "))
    };

    // system services should target multi-user.target; user services use default.target
    let wanted_by = match mode {
        LinuxLaunchMode::SystemdSystem => "multi-user.target",
        _ => "default.target",
    };

    format!(
        "# {}. Manual edits will be overwritten.\n\
        [Unit]\n\
        Description={}\n\
        After={}\n\
        \n\
        [Service]\n\
        Type=simple\n\
        ExecStart={}{}\n\
        Restart=on-failure\n\
        RestartSec=10\n\
        \n\
        [Install]\n\
        WantedBy={}",
        managed_marker, app_name, wanted_by, app_path, args_str, wanted_by
    )
}

/// Whether the file content carries the library's managed marker, matched by
/// prefix so future marker extensions stay recognized.
fn content_is_managed(content: &str, managed_marker: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("# ") && line[2..].starts_with(managed_marker)
    })
}

/// Get the XDG autostart directory
fn get_xdg_autostart_dir() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Failed to find home directory",
        )
    })?;
    Ok(home_dir.join(".config").join("autostart"))
}

/// Get the systemd service directory.
fn get_systemd_dir(mode: LinuxLaunchMode) -> Result<PathBuf> {
    match mode {
        LinuxLaunchMode::SystemdSystem => Ok(PathBuf::from("/etc/systemd/system")),
        LinuxLaunchMode::SystemdUser => {
            let home_dir = dirs::home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Failed to find home directory",
                )
            })?;
            Ok(home_dir.join(".config").join("systemd").join("user"))
        }
        LinuxLaunchMode::XdgAutostart => unreachable!("XDG mode does not use systemd dir"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_xdg_autostart_data() {
        let data = build_xdg_autostart_data(
            "TestApp",
            "/opt/test-app",
            &["--flag".into(), "value".into()],
            "Managed by TestApp",
        );

        assert!(data.contains("Type=Application"));
        assert!(data.contains("Name=TestApp"));
        assert!(data.contains("Comment=TestApp startup script"));
        assert!(data.contains("Exec=/opt/test-app --flag value"));
        assert!(data.contains("StartupNotify=false"));
        assert!(data.contains("Terminal=false"));
        assert!(data.starts_with("# Managed by TestApp. Manual edits will be overwritten.\n"));
    }

    #[test]
    fn test_build_systemd_service_data() {
        let data = build_systemd_service_data(
            "TestApp",
            "/opt/test-app",
            &["--flag".into()],
            LinuxLaunchMode::SystemdUser,
            "Managed by TestApp",
        );

        assert!(data.contains("Description=TestApp"));
        assert!(data.contains("After=default.target"));
        assert!(data.contains("ExecStart=/opt/test-app --flag"));
        assert!(data.contains("Restart=on-failure"));
        assert!(data.contains("WantedBy=default.target"));
        assert!(data.starts_with("# Managed by TestApp. Manual edits will be overwritten.\n"));
    }

    #[test]
    fn test_build_systemd_service_data_system() {
        let data = build_systemd_service_data(
            "TestApp",
            "/opt/test-app",
            &["--flag".into()],
            LinuxLaunchMode::SystemdSystem,
            "Managed by TestApp",
        );

        assert!(data.contains("After=multi-user.target"));
        assert!(data.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn test_content_is_managed() {
        let marker = "Managed by TestApp";
        // library-generated content: owned
        assert!(content_is_managed(
            "# Managed by TestApp. Manual edits will be overwritten.\n[Unit]\n..",
            marker
        ));
        // legacy library content: owned
        assert!(content_is_managed(
            "[Unit]\n..\n# Managed by TestApp v2 extension\n",
            marker
        ));
        // hand-written unit: not owned
        assert!(!content_is_managed("[Unit]\nDescription=x\n", marker));
        // a different app's marker: not owned
        assert!(!content_is_managed(
            "# Managed by OtherApp. ...\n[Unit]\n",
            marker
        ));
        // leading whitespace before the marker is fine
        assert!(content_is_managed("  # Managed by TestApp. ...\n", marker));
        // empty file
        assert!(!content_is_managed("", marker));
    }
}
