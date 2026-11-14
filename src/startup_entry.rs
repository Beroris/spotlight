//! Lists Spotlight under Startup apps in Task Manager, where it can be switched on to
//! start with Windows. On Linux a switch in Settings does it, through the desktop's
//! autostart folder.
//!
//! Task Manager lists what's in the Run key and keeps its on/off switch separately, in
//! StartupApproved. So correcting the path after a move never overrides the owner's
//! choice. The entry starts switched off: adding it shouldn't change what happens at
//! the next sign-in by itself.

// Most of this is only called on Windows.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const VALUE_NAME: &str = "Spotlight";
/// Passed by the entry, so Spotlight knows Windows started it as the user signed in.
pub const AT_SIGN_IN_ARG: &str = "--at-sign-in";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// Seconds from the start of Windows' clock (1601) to the start of Unix's (1970).
const WINDOWS_TO_UNIX_SECONDS: u64 = 11_644_473_600;

/// A build run from the source tree must not take over the installed app's entry.
fn is_dev_build(exe: &Path) -> bool {
    exe.components().any(|part| part.as_os_str().eq_ignore_ascii_case("target"))
}

fn command_for(exe: &Path) -> String {
    format!("\"{}\" {AT_SIGN_IN_ARG}", exe.display())
}

/// Task Manager's own record of a disabled entry: 3, then when it was disabled.
fn switched_off_record(now: SystemTime) -> [u8; 12] {
    let since_unix = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let ticks = (since_unix.as_secs() + WINDOWS_TO_UNIX_SECONDS) * 10_000_000
        + u64::from(since_unix.subsec_nanos()) / 100;
    let mut record = [0u8; 12];
    record[0] = 3;
    record[4..].copy_from_slice(&ticks.to_le_bytes());
    record
}

#[cfg(target_os = "windows")]
pub fn register() {
    use windows_sys::Win32::{
        Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS},
        System::Registry::{
            RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_BINARY, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
        },
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn read_command(name: &[u16]) -> Option<String> {
        let mut buffer = [0u16; 1024];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if result != ERROR_SUCCESS {
            return None;
        }
        let length = (size as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buffer[..length]))
    }

    fn has_a_switch(name: &[u16]) -> bool {
        let mut record = [0u8; 12];
        let mut size = record.len() as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide(APPROVED_KEY).as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_BINARY,
                std::ptr::null_mut(),
                record.as_mut_ptr().cast(),
                &mut size,
            )
        };
        result == ERROR_SUCCESS || result == ERROR_MORE_DATA
    }

    let Ok(exe) = std::env::current_exe() else { return };
    if is_dev_build(&exe) {
        return;
    }
    let name = wide(VALUE_NAME);
    let command = command_for(&exe);
    let existing = read_command(&name);
    if existing.as_deref() == Some(command.as_str()) {
        return;
    }
    if existing.is_none() && !has_a_switch(&name) {
        // Before the Run value, or it would count as switched on, even if only for a moment.
        let record = switched_off_record(SystemTime::now());
        let result = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                wide(APPROVED_KEY).as_ptr(),
                name.as_ptr(),
                REG_BINARY,
                record.as_ptr().cast(),
                record.len() as u32,
            )
        };
        if result != ERROR_SUCCESS {
            return;
        }
    }
    let data = wide(&command);
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            name.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        );
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn register() {}

/// On Linux the desktop opens whatever has an entry in the autostart folder as the user signs
/// in. Spotlight's is only there while the switch in Settings is on, so all there is to do
/// here is correct its path after the app has moved.
#[cfg(target_os = "linux")]
pub fn register() {
    let Ok(exe) = std::env::current_exe() else { return };
    if !is_dev_build(&exe) && opens_at_sign_in() {
        let _ = open_at_sign_in(true);
    }
}

#[cfg(target_os = "linux")]
fn autostart_file() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("autostart").join("com.beroris.spotlight.desktop"))
}

/// The autostart entry that opens this copy. Its path is quoted the way such entries ask: a
/// backslash before each of these four inside the quotes, and every backslash written twice
/// because the file reads `\\` as one.
#[cfg(any(target_os = "linux", test))]
fn autostart_entry(exe: &Path) -> String {
    let mut path = String::new();
    for letter in exe.to_string_lossy().chars() {
        match letter {
            '"' | '`' | '$' => path.push_str("\\\\"),
            '\\' => path.push_str("\\\\\\"),
            _ => {}
        }
        path.push(letter);
    }
    format!("[Desktop Entry]\nType=Application\nName=Spotlight\nExec=\"{path}\" {AT_SIGN_IN_ARG}\nIcon=com.beroris.spotlight\n")
}

/// Whether Spotlight opens as the user signs in.
#[cfg(target_os = "linux")]
pub fn opens_at_sign_in() -> bool {
    autostart_file().is_some_and(|file| file.exists())
}

/// Switches opening at sign-in on or off.
#[cfg(target_os = "linux")]
pub fn open_at_sign_in(on: bool) -> std::io::Result<()> {
    let file = autostart_file().ok_or(std::io::ErrorKind::NotFound)?;
    if !on {
        return match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let entry = autostart_entry(&std::env::current_exe()?);
    if std::fs::read_to_string(&file).is_ok_and(|existing| existing == entry) {
        return Ok(());
    }
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    std::fs::write(&file, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, time::Duration};

    // Windows paths, which only split into folders on Windows.
    #[cfg(target_os = "windows")]
    #[test]
    fn only_builds_from_the_source_tree_stay_out_of_startup() {
        assert!(is_dev_build(&PathBuf::from(r"C:\Code\spotlight\target\release\Spotlight.exe")));
        assert!(!is_dev_build(&PathBuf::from(r"C:\Users\Me\AppData\Local\Programs\Spotlight\Spotlight.exe")));
        // Only a whole folder name counts.
        assert!(!is_dev_build(&PathBuf::from(r"C:\targets\Spotlight.exe")));
    }

    #[test]
    fn the_command_survives_spaces_in_the_path() {
        assert_eq!(
            command_for(&PathBuf::from(r"C:\Program Files\Spotlight\Spotlight.exe")),
            r#""C:\Program Files\Spotlight\Spotlight.exe" --at-sign-in"#
        );
    }

    #[test]
    fn the_autostart_entry_survives_spaces_and_quotes_in_the_path() {
        let entry = autostart_entry(&PathBuf::from("/home/me/My \"apps\"/Spotlight"));
        assert!(entry.contains(r#"Exec="/home/me/My \\"apps\\"/Spotlight" --at-sign-in"#), "{entry}");
        let entry = autostart_entry(&PathBuf::from(r"/home/me/a\b$c/Spotlight"));
        assert!(entry.contains(r#"Exec="/home/me/a\\\\b\\$c/Spotlight" --at-sign-in"#), "{entry}");
        assert!(entry.starts_with("[Desktop Entry]\nType=Application\n"));
    }

    #[test]
    fn the_switched_off_record_matches_what_task_manager_writes() {
        let record = switched_off_record(UNIX_EPOCH + Duration::from_secs(1));
        assert_eq!(record[..4], [3, 0, 0, 0]);
        // 1970 plus one second, in 100 ns ticks since 1601.
        assert_eq!(u64::from_le_bytes(record[4..].try_into().unwrap()), 116_444_736_010_000_000);
    }
}
