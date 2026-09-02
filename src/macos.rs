use crate::{AutoLaunch, Error, MacOSLaunchMode, Result};
use plist::{Dictionary, Value};
use smappservice_rs::{AppService, ServiceStatus, ServiceType};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// macOS implement
impl AutoLaunch {
    /// Create a new AutoLaunch instance
    /// - `app_name`: application name
    /// - `app_path`: application path
    /// - `launch_mode`: launch mode (Launch Agent, AppleScript, or SMAppService)
    /// - `args`: startup args passed to the binary
    /// - `bundle_identifiers`: bundle identifiers (only used for LaunchAgent modes)
    /// - `agent_extra_config`: extra config for Launch Agent / Launch Daemon (unused currently)
    ///
    /// ## Notes
    ///
    /// The parameters of `AutoLaunch::new` are different on each platform.
    ///
    /// The `app_name` should be same as the basename of the `app_path`
    ///     when using AppleScript mode, or it will be corrected automatically.
    ///
    /// The `app_path` should be the **absolute path** and **exists**,
    ///     otherwise it will cause an error when `enable`.
    ///
    /// In case using AppleScript,
    ///     only `"--hidden"` and `"--minimized"` in `args` are valid.
    ///
    /// In case using SMAppService (macOS 13+), `app_name` and `app_path` can be empty strings
    ///     as it registers the running application.
    pub fn new(
        app_name: &str,
        app_path: &str,
        launch_mode: MacOSLaunchMode,
        args: &[impl AsRef<str>],
        bundle_identifiers: &[impl AsRef<str>],
        agent_extra_config: &str,
    ) -> AutoLaunch {
        let mut name = app_name;
        if launch_mode == MacOSLaunchMode::AppleScript {
            // the app_name should be same as the executable's name
            // when using login item
            let end = if app_path.ends_with(".app") { 4 } else { 0 };
            let end = app_path.len() - end;
            let begin = match app_path.rfind('/') {
                Some(i) => i + 1,
                None => 0,
            };
            name = &app_path[begin..end];
        }

        AutoLaunch {
            app_name: name.into(),
            app_path: app_path.into(),
            launch_mode,
            args: args.iter().map(|s| s.as_ref().to_string()).collect(),
            bundle_identifiers: bundle_identifiers
                .iter()
                .map(|s| s.as_ref().to_string())
                .collect(),
            agent_extra_config: agent_extra_config.into(),
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
    /// - [`crate::Error::RegistrationNotOwned`]: a non-library plist file
    ///   exists at the registration path
    /// - `app_path` does not exist
    /// - `app_path` is not absolute
    ///
    /// #### LaunchAgent / LaunchDaemon
    ///
    /// - failed to create the plist directory
    /// - failed to serialize or write the plist file
    ///
    /// #### AppleScript
    ///
    /// - failed to execute the `osascript` command, check the exit status or stderr for details
    ///
    /// #### SMAppService
    ///
    /// - failed to register app with SMAppService API (macOS 13+)
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
    /// Checks the `ManagedBy` marker key written by this library into the
    /// plist (launchd ignores unknown keys). A missing file returns `false`.
    ///
    /// AppleScript and SMAppService modes have no on-disk registration
    /// a user could edit, so they always report `true`.
    pub fn is_registration_owned(&self) -> Result<bool> {
        match self.launch_mode {
            MacOSLaunchMode::LaunchAgentUser
            | MacOSLaunchMode::LaunchAgentSystem
            | MacOSLaunchMode::LaunchDaemonSystem => {
                let file = self.get_file()?;
                if !file.exists() {
                    return Ok(false);
                }
                let value: Value = plist::from_file(&file).map_err(std::io::Error::other)?;
                Ok(plist_managed_by(&value, &self.managed_marker()))
            }
            MacOSLaunchMode::AppleScript | MacOSLaunchMode::SMAppService => Ok(true),
        }
    }

    fn enable_with_force(&self, force: bool) -> Result<()> {
        if self.launch_mode == MacOSLaunchMode::SMAppService {
            let app_service = AppService::new(ServiceType::MainApp);
            match app_service.register() {
                Ok(()) => return Ok(()),
                Err(e) => return Err(Error::SMAppServiceRegistrationFailed(e.code())),
            }
        }

        let path = Path::new(&self.app_path);

        if !path.exists() {
            return Err(Error::AppPathDoesntExist(path.to_path_buf()));
        }

        if !path.is_absolute() {
            return Err(Error::AppPathIsNotAbsolute(path.to_path_buf()));
        }

        match self.launch_mode {
            MacOSLaunchMode::LaunchAgentUser | MacOSLaunchMode::LaunchAgentSystem => {
                if !force && self.is_plist_not_owned()? {
                    return Err(Error::RegistrationNotOwned(self.get_file()?));
                }
                self.write_plist(build_launch_agent_plist(
                    &self.app_name,
                    &self.app_path,
                    &self.args,
                    &self.bundle_identifiers,
                    &self.managed_marker(),
                ))
            }
            MacOSLaunchMode::LaunchDaemonSystem => {
                if !force && self.is_plist_not_owned()? {
                    return Err(Error::RegistrationNotOwned(self.get_file()?));
                }
                self.write_plist(build_launch_daemon_plist(
                    &self.app_name,
                    &self.app_path,
                    &self.args,
                    &self.managed_marker(),
                ))
            }
            MacOSLaunchMode::AppleScript => self.enable_applescript(),
            MacOSLaunchMode::SMAppService => unreachable!("SMAppService mode handled above"),
        }
    }

    /// Whether the registration file exists but was not created by this library.
    fn is_plist_not_owned(&self) -> Result<bool> {
        let file = self.get_file()?;
        if !file.exists() {
            return Ok(false);
        }
        Ok(!plist_managed_by(
            &plist::from_file(&file).map_err(std::io::Error::other)?,
            &self.managed_marker(),
        ))
    }

    /// Write a plist `Dictionary` to the appropriate file path.
    fn write_plist(&self, dict: Dictionary) -> Result<()> {
        let dir = get_dir(self.launch_mode)?;
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let file = self.get_file()?;
        let f = fs::File::create(file)?;
        plist::to_writer_xml(f, &Value::Dictionary(dict)).map_err(std::io::Error::other)?;
        Ok(())
    }

    /// Enable using AppleScript
    fn enable_applescript(&self) -> Result<()> {
        let hidden = self
            .args
            .iter()
            .find(|arg| *arg == "--hidden" || *arg == "--minimized");

        let props = format!(
            "{{name:\"{}\",path:\"{}\",hidden:{}}}",
            self.app_name,
            self.app_path,
            hidden.is_some()
        );
        let command = format!("make login item at end with properties {props}");
        let output = exec_apple_script(&command)?;
        if !output.status.success() {
            return Err(Error::AppleScriptFailed(output.status.code().unwrap_or(1)));
        }
        Ok(())
    }

    /// Disable the AutoLaunch setting
    ///
    /// ## Errors
    ///
    /// #### LaunchAgent / LaunchDaemon
    ///
    /// - failed to remove the plist file
    ///
    /// #### AppleScript
    ///
    /// - failed to execute the `osascript` command, check the exit status or stderr for details
    ///
    /// #### SMAppService
    ///
    /// - failed to unregister app with SMAppService API (macOS 13+)
    pub fn disable(&self) -> Result<()> {
        match self.launch_mode {
            MacOSLaunchMode::LaunchAgentUser
            | MacOSLaunchMode::LaunchAgentSystem
            | MacOSLaunchMode::LaunchDaemonSystem => self.disable_plist(),
            MacOSLaunchMode::AppleScript => self.disable_applescript(),
            MacOSLaunchMode::SMAppService => self.disable_smappservice(),
        }
    }

    /// Disable SMAppService
    fn disable_smappservice(&self) -> Result<()> {
        let app_service = AppService::new(ServiceType::MainApp);
        match app_service.unregister() {
            Ok(()) => Ok(()),
            Err(e) => Err(Error::SMAppServiceUnregistrationFailed(e.code())),
        }
    }

    /// Remove the plist file (used by both LaunchAgent and LaunchDaemon modes)
    fn disable_plist(&self) -> Result<()> {
        let file = self.get_file()?;
        if file.exists() {
            fs::remove_file(file)?;
        }
        Ok(())
    }

    /// Disable AppleScript login item
    fn disable_applescript(&self) -> Result<()> {
        let command = format!("delete login item \"{}\"", self.app_name);
        let output = exec_apple_script(&command)?;
        if !output.status.success() {
            return Err(Error::AppleScriptFailed(output.status.code().unwrap_or(1)));
        }
        Ok(())
    }

    /// Check whether the AutoLaunch setting is enabled
    pub fn is_enabled(&self) -> Result<bool> {
        match self.launch_mode {
            MacOSLaunchMode::LaunchAgentUser
            | MacOSLaunchMode::LaunchAgentSystem
            | MacOSLaunchMode::LaunchDaemonSystem => Ok(self.get_file()?.exists()),
            MacOSLaunchMode::AppleScript => self.is_applescript_enabled(),
            MacOSLaunchMode::SMAppService => self.is_smappservice_enabled(),
        }
    }

    /// Check if SMAppService is enabled
    fn is_smappservice_enabled(&self) -> Result<bool> {
        let app_service = AppService::new(ServiceType::MainApp);
        Ok(app_service.status() == ServiceStatus::Enabled)
    }

    /// Check if AppleScript login item is enabled
    fn is_applescript_enabled(&self) -> Result<bool> {
        let command = "get the name of every login item";
        let output = exec_apple_script(command)?;
        let enable = if output.status.success() {
            let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
            stdout
                .split(',')
                .map(|x| x.trim())
                .any(|x| x == self.app_name)
        } else {
            false
        };
        Ok(enable)
    }

    /// Read the registered `app_path` from the on-disk plist file.
    ///
    /// Returns `Ok(None)` when the registration does not exist.
    /// Returns `Ok(Some(path))` with the first element of `ProgramArguments`.
    ///
    /// AppleScript and SMAppService modes do not store a path on disk and
    /// always return `Ok(None)`.
    pub fn get_registered_app_path(&self) -> Result<Option<String>> {
        match self.launch_mode {
            MacOSLaunchMode::LaunchAgentUser
            | MacOSLaunchMode::LaunchAgentSystem
            | MacOSLaunchMode::LaunchDaemonSystem => {
                let file = self.get_file()?;
                if !file.exists() {
                    return Ok(None);
                }
                let value: Value = plist::from_file(&file).map_err(std::io::Error::other)?;
                let path = value
                    .as_dictionary()
                    .and_then(|d| d.get("ProgramArguments"))
                    .and_then(|v| v.as_array())
                    .and_then(|args| args.first())
                    .and_then(|v| v.as_string())
                    .map(|s| s.to_string());
                Ok(path)
            }
            MacOSLaunchMode::AppleScript | MacOSLaunchMode::SMAppService => Ok(None),
        }
    }

    /// Get the plist file path for the current launch mode
    fn get_file(&self) -> Result<PathBuf> {
        Ok(get_dir(self.launch_mode)?.join(format!("{}.plist", self.app_name)))
    }
}

/// Return the directory where the plist file should be placed.
fn get_dir(mode: MacOSLaunchMode) -> Result<PathBuf> {
    match mode {
        MacOSLaunchMode::LaunchAgentUser => {
            let home_dir = dirs::home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Failed to find home directory",
                )
            })?;
            Ok(home_dir.join("Library").join("LaunchAgents"))
        }
        MacOSLaunchMode::LaunchAgentSystem => Ok(PathBuf::from("/Library/LaunchAgents")),
        MacOSLaunchMode::LaunchDaemonSystem => Ok(PathBuf::from("/Library/LaunchDaemons")),
        MacOSLaunchMode::AppleScript | MacOSLaunchMode::SMAppService => {
            unreachable!("AppleScript/SMAppService do not use a plist directory")
        }
    }
}

/// Execute the specific AppleScript
fn exec_apple_script(cmd_suffix: &str) -> Result<Output> {
    let command = format!("tell application \"System Events\" to {cmd_suffix}");
    let output = Command::new("osascript")
        .args(vec!["-e", &command])
        .output()?;
    Ok(output)
}

/// Build a plist `Dictionary` for a **LaunchAgent** (user or system).
///
/// LaunchAgent-specific fields:
/// - `AssociatedBundleIdentifiers`: links this agent to an app bundle for display in
///   System Settings > General > Login Items. Not supported by LaunchDaemon.
///
/// The plist is written to `~/Library/LaunchAgents/` (user) or
/// `/Library/LaunchAgents/` (system). The process runs as the **logged-in user**.
fn build_launch_agent_plist(
    app_name: &str,
    app_path: &str,
    args: &[String],
    bundle_identifiers: &[String],
    managed_marker: &str,
) -> Dictionary {
    let mut program_args: Vec<Value> = vec![Value::String(app_path.into())];
    program_args.extend(args.iter().map(|a| Value::String(a.clone())));

    let mut dict = Dictionary::new();
    dict.insert("Label".into(), Value::String(app_name.into()));

    // ManagedBy: ownership marker used by is_registration_owned().
    // launchd ignores unknown keys, so this key is safe to include.
    dict.insert("ManagedBy".into(), Value::String(managed_marker.into()));

    // AssociatedBundleIdentifiers: LaunchAgent-only — links agent to an app bundle.
    if !bundle_identifiers.is_empty() {
        let ids: Vec<Value> = bundle_identifiers
            .iter()
            .map(|id| Value::String(id.clone()))
            .collect();
        dict.insert("AssociatedBundleIdentifiers".into(), Value::Array(ids));
    }

    dict.insert("ProgramArguments".into(), Value::Array(program_args));
    dict.insert("RunAtLoad".into(), Value::Boolean(true));
    dict
}

/// Build a plist `Dictionary` for a **LaunchDaemon** (system-level, runs as root).
///
/// Key differences from LaunchAgent:
/// - No `AssociatedBundleIdentifiers` (unsupported by launchd for daemons).
/// - `SessionCreate = true`: gives the daemon its own security session, required
///   for accessing system services (e.g. Keychain, audio) without a user session.
///
/// The plist is written to `/Library/LaunchDaemons/`. Writing that directory
/// and loading the daemon both require **root / sudo** privileges.
fn build_launch_daemon_plist(
    app_name: &str,
    app_path: &str,
    args: &[String],
    managed_marker: &str,
) -> Dictionary {
    let mut program_args: Vec<Value> = vec![Value::String(app_path.into())];
    program_args.extend(args.iter().map(|a| Value::String(a.clone())));

    let mut dict = Dictionary::new();
    dict.insert("Label".into(), Value::String(app_name.into()));
    // ManagedBy: ownership marker used by is_registration_owned().
    // launchd ignores unknown keys, so this key is safe to include.
    dict.insert("ManagedBy".into(), Value::String(managed_marker.into()));
    dict.insert("ProgramArguments".into(), Value::Array(program_args));
    dict.insert("RunAtLoad".into(), Value::Boolean(true));
    // SessionCreate: LaunchDaemon-specific — creates a security session for the daemon.
    dict.insert("SessionCreate".into(), Value::Boolean(true));
    dict
}

/// Whether the plist carries the library's managed marker, matched by prefix
/// so future marker extensions stay recognized.
fn plist_managed_by(value: &Value, managed_marker: &str) -> bool {
    value
        .as_dictionary()
        .and_then(|d| d.get("ManagedBy"))
        .and_then(|v| v.as_string())
        .map(|s| s.starts_with(managed_marker))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_launch_agent_plist() {
        let dict = build_launch_agent_plist(
            "TestApp",
            "/Applications/TestApp.app",
            &["--flag".into()],
            &["com.example.testapp".into()],
            "Managed by TestApp",
        );

        // Serialize to XML for assertion
        let mut buf = Vec::new();
        plist::to_writer_xml(&mut buf, &Value::Dictionary(dict)).unwrap();
        let xml = String::from_utf8(buf).unwrap();

        assert!(xml.contains("<string>TestApp</string>"));
        assert!(xml.contains("AssociatedBundleIdentifiers"));
        assert!(xml.contains("<string>com.example.testapp</string>"));
        assert!(xml.contains("<string>/Applications/TestApp.app</string>"));
        assert!(xml.contains("<string>--flag</string>"));
        assert!(xml.contains("ManagedBy"));
        assert!(xml.contains("<string>Managed by TestApp</string>"));
        assert!(xml.contains("RunAtLoad"));
        assert!(xml.contains("<true/>"));
        // Agent must NOT have SessionCreate
        assert!(!xml.contains("SessionCreate"));
    }

    #[test]
    fn test_build_launch_daemon_plist() {
        let dict = build_launch_daemon_plist(
            "TestDaemon",
            "/usr/local/bin/test-daemon",
            &["--flag".into()],
            "Managed by TestDaemon",
        );

        let mut buf = Vec::new();
        plist::to_writer_xml(&mut buf, &Value::Dictionary(dict)).unwrap();
        let xml = String::from_utf8(buf).unwrap();

        assert!(xml.contains("<string>TestDaemon</string>"));
        assert!(xml.contains("<string>/usr/local/bin/test-daemon</string>"));
        assert!(xml.contains("<string>--flag</string>"));
        assert!(xml.contains("ManagedBy"));
        assert!(xml.contains("RunAtLoad"));
        assert!(xml.contains("<true/>"));
        // Daemon must NOT have AssociatedBundleIdentifiers
        assert!(!xml.contains("AssociatedBundleIdentifiers"));
        // Daemon MUST have SessionCreate
        assert!(xml.contains("SessionCreate"));
    }

    #[test]
    fn test_plist_managed_by() {
        let marker = "Managed by TestApp";
        let mut dict = Dictionary::new();
        dict.insert("Label".into(), Value::String("TestApp".into()));
        dict.insert(
            "ManagedBy".into(),
            Value::String("Managed by TestApp".into()),
        );
        // library-generated plist: owned
        assert!(plist_managed_by(&Value::Dictionary(dict.clone()), marker));
        // future extension of the marker: still owned
        dict.insert(
            "ManagedBy".into(),
            Value::String("Managed by TestApp v2".into()),
        );
        assert!(plist_managed_by(&Value::Dictionary(dict), marker));

        // hand-written plist without the key: not owned
        let mut manual = Dictionary::new();
        manual.insert("Label".into(), Value::String("TestApp".into()));
        assert!(!plist_managed_by(&Value::Dictionary(manual), marker));

        // a different app's marker: not owned
        let mut other = Dictionary::new();
        other.insert(
            "ManagedBy".into(),
            Value::String("Managed by OtherApp".into()),
        );
        assert!(!plist_managed_by(&Value::Dictionary(other), marker));

        // non-dictionary value: not owned
        assert!(!plist_managed_by(&Value::String("x".into()), marker));
    }
}
