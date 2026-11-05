//! Crash report: a panic in a windowed release build is invisible (no console), so the program would
//! just vanish. The hook writes the message, the place and a backtrace to `crash.log` next to the
//! settings (`%APPDATA%\PanoWeave\crash.log`) and, in the window program, tells the person where it is.

use std::path::PathBuf;

/// Where the report goes.
pub fn log_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PanoWeave").join("crash.log"))
}

/// One report as text.
pub fn format_report(version: &str, unix_secs: u64, thread: &str, message: &str, location: &str, backtrace: &str) -> String {
    format!("--- PanoWeave {version} crashed (unix time {unix_secs}) ---\nthread: {thread}\nmessage: {message}\nat: {location}\nbacktrace:\n{backtrace}\n\n")
}

/// Installs the hook. `show_dialog`: also show a message box (only for the window program).
pub fn install(show_dialog: bool) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => info.payload().downcast_ref::<String>().cloned().unwrap_or_else(|| "(no message)".to_string()),
        };
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let report = format_report(env!("CARGO_PKG_VERSION"), secs, &thread, &message, &location, &std::backtrace::Backtrace::force_capture().to_string());
        let path = log_path();
        if let Some(p) = &path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = f.write_all(report.as_bytes());
            }
        }
        default(info);
        // a panic in a worker thread is reported by the app itself; only the main thread ends the program
        if show_dialog && thread == "main" {
            let where_ = path.map(|p| p.display().to_string()).unwrap_or_else(|| "(no folder)".into());
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("PanoWeave has stopped")
                .set_description(format!("PanoWeave ran into an error and has to close.\n\n{message}\n\nA report was saved to:\n{where_}\n\nPlease attach it when you report the problem."))
                .show();
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_version_thread_message_and_place() {
        let r = format_report("0.1.0", 42, "main", "boom", "src/x.rs:7", "frames");
        assert!(r.contains("PanoWeave 0.1.0 crashed"));
        assert!(r.contains("thread: main") && r.contains("message: boom") && r.contains("at: src/x.rs:7") && r.contains("frames"));
    }

    #[test]
    fn the_log_lives_next_to_the_settings() {
        if let Some(p) = log_path() {
            assert_eq!(p.file_name().unwrap(), "crash.log");
            assert_eq!(p.parent().unwrap().file_name().unwrap(), "PanoWeave");
        }
    }
}
