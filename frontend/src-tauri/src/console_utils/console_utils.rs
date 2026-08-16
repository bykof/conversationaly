#[cfg(target_os = "windows")]
use std::ptr;
#[cfg(target_os = "macos")]
use std::process::Command;

use tauri::{AppHandle, Manager};

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn AllocConsole() -> i32;
    #[allow(dead_code)]
    fn FreeConsole() -> i32;
    fn GetConsoleWindow() -> *mut std::ffi::c_void;
    fn ShowWindow(hwnd: *mut std::ffi::c_void, n_cmd_show: i32) -> i32;
}

#[cfg(target_os = "windows")]
const SW_HIDE: i32 = 0;
#[cfg(target_os = "windows")]
const SW_SHOW: i32 = 5;

#[tauri::command]
pub fn show_console() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    unsafe {
        let console_window = GetConsoleWindow();
        if console_window == ptr::null_mut() {
            // If no console exists, allocate one
            if AllocConsole() == 0 {
                return Err("Failed to allocate console".to_string());
            }
            // No logger init here. This used to re-run `env_logger::init()`,
            // which would have panicked (a logger was already installed in
            // main) had anyone reached it. tauri-plugin-log's Stdout target
            // writes through GetStdHandle, which AllocConsole has just
            // repointed at the new console, so output arrives on its own.
        } else {
            // Show existing console window
            ShowWindow(console_window, SW_SHOW);
        }
        Ok("Console shown".to_string())
    }
    
    #[cfg(target_os = "macos")]
    {
        // On macOS, we'll open Terminal.app with our app's logs
        // First, get the app name from the bundle
        match Command::new("osascript")
            .arg("-e")
            .arg(r#"
                tell application "Terminal"
                    activate
                    do script "log stream --process conversationaly --level info --style compact"
                end tell
            "#)
            .spawn()
        {
            Ok(_) => Ok("Console opened in Terminal".to_string()),
            Err(e) => Err(format!("Failed to open console: {}", e)),
        }
    }
    
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Ok("Console control is only available on Windows and macOS".to_string())
    }
}

#[tauri::command]
pub fn hide_console() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    unsafe {
        let console_window = GetConsoleWindow();
        if console_window != ptr::null_mut() {
            ShowWindow(console_window, SW_HIDE);
            Ok("Console hidden".to_string())
        } else {
            Err("No console window found".to_string())
        }
    }
    
    #[cfg(target_os = "macos")]
    {
        // On macOS, we'll close the Terminal window that's showing our logs
        match Command::new("osascript")
            .arg("-e")
            .arg(r#"
                tell application "Terminal"
                    set windowList to windows
                    repeat with aWindow in windowList
                        if contents of selected tab of aWindow contains "log stream --process conversationaly" then
                            close aWindow
                        end if
                    end repeat
                end tell
            "#)
            .spawn()
        {
            Ok(_) => Ok("Console closed".to_string()),
            Err(e) => Err(format!("Failed to close console: {}", e)),
        }
    }
    
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Ok("Console control is only available on Windows and macOS".to_string())
    }
}

/// Basename of the rotating log file, without extension.
///
/// `lib.rs::log_plugin()` hands this same constant to tauri-plugin-log as
/// `TargetKind::LogDir { file_name }`; the plugin appends `.log` and writes it
/// into `app_log_dir()`. Shared rather than spelled twice so the path this
/// module reports cannot drift from the file the logger actually writes.
pub const LOG_FILE_STEM: &str = "conversationaly";

/// Absolute path of the log file tauri-plugin-log is writing.
///
/// Reported whether or not the file exists yet: a missing path is still the
/// right thing to tell a user who is being asked to send their logs.
#[tauri::command]
pub fn get_log_file_path(app: AppHandle) -> Result<String, String> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|e| format!("Failed to resolve app log dir: {}", e))?;

    Ok(dir
        .join(LOG_FILE_STEM)
        .with_extension("log")
        .to_string_lossy()
        .to_string())
}

/// Show the log file in the OS file manager, selected rather than opened — a
/// 4 MB log opened in the default text-file handler is not what anyone wants.
///
/// Falls back to revealing the containing directory when the file has not been
/// created yet (first launch before the first flush).
#[tauri::command]
pub fn reveal_log_file(app: AppHandle) -> Result<(), String> {
    let path = std::path::PathBuf::from(get_log_file_path(app)?);
    let exists = path.exists();
    let dir = path
        .parent()
        .ok_or_else(|| "Log path has no parent directory".to_string())?
        .to_path_buf();

    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create log directory: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("explorer");
        if exists {
            // No space after the comma: explorer treats "/select, path" as two
            // arguments and silently opens the user's Documents folder instead.
            command.arg(format!("/select,{}", path.display()));
        } else {
            command.arg(dir.as_os_str());
        }
        command
            .spawn()
            .map_err(|e| format!("Failed to reveal log file: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        if exists {
            command.arg("-R");
            command.arg(path.as_os_str());
        } else {
            command.arg(dir.as_os_str());
        }
        command
            .spawn()
            .map_err(|e| format!("Failed to reveal log file: {}", e))?;
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // xdg-open has no reveal-and-select verb; the directory is the closest
        // portable equivalent.
        let _ = exists;
        std::process::Command::new("xdg-open")
            .arg(dir.as_os_str())
            .spawn()
            .map_err(|e| format!("Failed to open log directory: {}", e))?;
    }

    log::info!("Revealed log file: {}", path.display());
    Ok(())
}

#[tauri::command]
pub fn toggle_console() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    unsafe {
        let console_window = GetConsoleWindow();
        if console_window == ptr::null_mut() {
            show_console()
        } else {
            // Check if window is visible (this is a simplified approach)
            // In a real implementation, you might want to use GetWindowLong to check visibility
            hide_console()
        }
    }
    
    #[cfg(target_os = "macos")]
    {
        // On macOS, check if Terminal is running with our log stream
        let check_result = Command::new("osascript")
            .arg("-e")
            .arg(r#"
                tell application "Terminal"
                    set windowList to windows
                    repeat with aWindow in windowList
                        if contents of selected tab of aWindow contains "log stream --process conversationaly" then
                            return "found"
                        end if
                    end repeat
                    return "not found"
                end tell
            "#)
            .output();
            
        match check_result {
            Ok(output) => {
                let output_str = String::from_utf8_lossy(&output.stdout);
                if output_str.trim() == "found" {
                    hide_console()
                } else {
                    show_console()
                }
            }
            Err(_) => show_console()
        }
    }
    
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Ok("Console control is only available on Windows and macOS".to_string())
    }
}