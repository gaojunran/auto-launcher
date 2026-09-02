use crate::{AutoLaunch, Error, Result, WindowsEnableMode};
use std::io;
use std::path::PathBuf;
use windows_registry::{Key, CURRENT_USER, LOCAL_MACHINE};
use windows_result::HRESULT;

const AL_REGKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
const AL_MARKER_REGKEY: &str = r"SOFTWARE\auto-launcher";
// Marker value at {AL_MARKER_REGKEY}\{app_name}: marks a registration as
// written by this library. Lives outside the Run key because every value in
// the Run key is executed at login; this subtree is never executed.
const AL_MARKER_VALUE: &str = "managed";
const TASK_MANAGER_OVERRIDE_REGKEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const TASK_MANAGER_OVERRIDE_ENABLED_VALUE: [u8; 12] = [
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const E_ACCESSDENIED: HRESULT = HRESULT::from_win32(0x80070005_u32);
const E_FILENOTFOUND: HRESULT = HRESULT::from_win32(0x80070002_u32);

/// Windows implement
impl AutoLaunch {
    /// Create a new AutoLaunch instance
    /// - `app_name`: application name
    /// - `app_path`: application path
    /// - `enable_mode`: behavior of the enable feature
    /// - `args`: startup args passed to the binary
    ///
    /// ## Notes
    ///
    /// The parameters of `AutoLaunch::new` are different on each platform.
    pub fn new(
        app_name: &str,
        app_path: &str,
        enable_mode: WindowsEnableMode,
        args: &[impl AsRef<str>],
    ) -> AutoLaunch {
        AutoLaunch {
            app_name: app_name.into(),
            app_path: app_path.into(),
            enable_mode,
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
    /// - [`crate::Error::RegistrationNotOwned`]: a manual Run registration
    ///   exists without the library marker
    /// - failed to open the registry key
    /// - failed to set value
    pub fn enable(&self) -> Result<()> {
        self.enable_with_force(false)
    }

    /// Like [`Self::enable`], but unconditionally overwrites any existing
    /// registration, including manually managed ones.
    pub fn enable_force(&self) -> Result<()> {
        self.enable_with_force(true)
    }

    /// Whether an existing registration was created by this library.
    ///
    /// Checks for the marker value at `SOFTWARE\auto-launcher\{app_name}`
    /// under HKLM or HKCU. Registrations without the marker, e.g. manually
    /// added Run entries, return `false`.
    pub fn is_registration_owned(&self) -> Result<bool> {
        Ok([LOCAL_MACHINE, CURRENT_USER]
            .iter()
            .any(|root| self.marker_exists(root)))
    }

    fn enable_with_force(&self, force: bool) -> Result<()> {
        if !force {
            for (root_name, root_key) in [("HKLM", LOCAL_MACHINE), ("HKCU", CURRENT_USER)] {
                if let Ok(key) = root_key.open(AL_REGKEY) {
                    if key.get_string(&self.app_name).is_ok() && !self.marker_exists(root_key) {
                        return Err(Error::RegistrationNotOwned(PathBuf::from(format!(
                            r"{root_name}\{AL_REGKEY}"
                        ))));
                    }
                }
            }
        }
        match self.enable_mode {
            WindowsEnableMode::Dynamic => self
                .enable_as_admin()
                .or_else(|e| {
                    if e.code() == E_ACCESSDENIED {
                        self.enable_as_current_user()
                    } else {
                        Err(e)
                    }
                })
                .map_err(std::io::Error::from)?,
            WindowsEnableMode::CurrentUser => self
                .enable_as_current_user()
                .map_err(std::io::Error::from)?,
            WindowsEnableMode::System => self.enable_as_admin().map_err(std::io::Error::from)?,
        }
        Ok(())
    }

    /// Whether the library marker exists under the given root key.
    fn marker_exists(&self, root_key: &Key) -> bool {
        root_key
            .open(&self.marker_regkey())
            .and_then(|key| key.get_string(AL_MARKER_VALUE))
            .is_ok()
    }

    /// Registry path of the library marker for this app.
    fn marker_regkey(&self) -> String {
        format!(r"{AL_MARKER_REGKEY}\{}", self.managed_name())
    }

    fn enable_as_admin(&self) -> windows_registry::Result<()> {
        self.enable_with_root_key(LOCAL_MACHINE)
    }

    fn enable_as_current_user(&self) -> windows_registry::Result<()> {
        self.enable_with_root_key(CURRENT_USER)
    }

    fn enable_with_root_key(&self, root_key: &Key) -> windows_registry::Result<()> {
        root_key.create(AL_REGKEY)?.set_string(
            &self.app_name,
            format!("{} {}", self.app_path, self.args.join(" ")),
        )?;
        root_key
            .create(&self.marker_regkey())?
            .set_string(AL_MARKER_VALUE, "1")?;

        match root_key
            .options()
            .write()
            .open(TASK_MANAGER_OVERRIDE_REGKEY)
        {
            Ok(key) => key.set_bytes(
                &self.app_name,
                windows_registry::Type::Bytes,
                &TASK_MANAGER_OVERRIDE_ENABLED_VALUE,
            )?,
            Err(error) if error.code() == E_FILENOTFOUND => {
                return Ok(());
            }
            Err(error) => {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Disable the AutoLaunch setting
    ///
    /// ## Errors
    ///
    /// - failed to open the registry key
    /// - failed to delete value
    pub fn disable(&self) -> Result<()> {
        // try to delete both admin and current user registry values
        if let Err(error) = self.disable_as_admin() {
            if error.code() == E_ACCESSDENIED {
                // Fail if our enable mode is system but we don't have the access
                if self.enable_mode == WindowsEnableMode::System {
                    return Err(std::io::Error::from(error).into());
                }
                // Otherwise ignore this error
            } else {
                return Err(std::io::Error::from(error).into());
            }
        }
        self.disable_as_current_user()
            .map_err(std::io::Error::from)?;
        Ok(())
    }

    fn disable_as_admin(&self) -> windows_registry::Result<()> {
        self.disable_with_root_key(LOCAL_MACHINE)
    }

    fn disable_as_current_user(&self) -> windows_registry::Result<()> {
        self.disable_with_root_key(CURRENT_USER)
    }

    fn disable_with_root_key(&self, root_key: &Key) -> windows_registry::Result<()> {
        // Best-effort removal of the library marker; a leftover empty key is
        // harmless and gets overwritten on the next enable.
        if let Ok(key) = root_key.open(&self.marker_regkey()) {
            let _ = key.remove_value(AL_MARKER_VALUE);
        }
        match root_key
            .options()
            .write()
            .open(AL_REGKEY)
            .and_then(|key| key.remove_value(&self.app_name))
        {
            Ok(_) => Ok(()),
            Err(error) if error.code() == E_FILENOTFOUND => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Read the registered `app_path` from the registry.
    ///
    /// Returns `Ok(None)` when no registration exists in either HKLM or HKCU.
    /// The registry stores `"<app_path> <args...>"` as a single string; this
    /// method extracts the first whitespace-delimited token (the binary path).
    pub fn get_registered_app_path(&self) -> Result<Option<String>> {
        for root_key in [LOCAL_MACHINE, CURRENT_USER] {
            if let Ok(value) = root_key
                .open(AL_REGKEY)
                .and_then(|key| key.get_string(&self.app_name))
            {
                let path = value.split_whitespace().next().map(|s| s.to_string());
                return Ok(path);
            }
        }
        Ok(None)
    }

    /// Check whether the AutoLaunch setting is enabled
    pub fn is_enabled(&self) -> Result<bool> {
        let is_registered =
            self.is_registered(LOCAL_MACHINE)? || self.is_registered(CURRENT_USER)?;
        if !is_registered {
            return Ok(false);
        }
        let is_task_manager_enabled = self.is_task_manager_enabled(LOCAL_MACHINE)?
            && self.is_task_manager_enabled(CURRENT_USER)?;
        Ok(is_task_manager_enabled)
    }

    fn is_registered(&self, root_key: &Key) -> io::Result<bool> {
        let registered = match root_key
            .open(AL_REGKEY)
            .and_then(|key| key.get_string(&self.app_name))
        {
            Ok(_) => true,
            Err(error) if error.code() == E_FILENOTFOUND => false,
            Err(error) => {
                return Err(error.into());
            }
        };
        Ok(registered)
    }

    fn is_task_manager_enabled(&self, root_key: &Key) -> io::Result<bool> {
        let task_manager_enabled = match root_key
            .open(TASK_MANAGER_OVERRIDE_REGKEY)
            .and_then(|key| key.get_value(&self.app_name))
        {
            Ok(value) => last_eight_bytes_all_zeros(&value).unwrap_or(true),
            Err(error) if error.code() == E_FILENOTFOUND => true,
            Err(error) => {
                return Err(error.into());
            }
        };
        Ok(task_manager_enabled)
    }
}

fn last_eight_bytes_all_zeros(bytes: &[u8]) -> std::result::Result<bool, &str> {
    if bytes.len() < 8 {
        Err("Bytes too short")
    } else {
        Ok(bytes.iter().rev().take(8).all(|v| *v == 0u8))
    }
}
