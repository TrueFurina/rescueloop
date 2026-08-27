use std::io::{self, Write};

pub fn emergency(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
    write_system_log(message);
}

#[cfg(unix)]
fn write_system_log(message: &str) {
    use std::ffi::CString;

    let sanitized = message.replace('\0', "�");
    let Ok(message) = CString::new(sanitized) else {
        return;
    };
    // syslog is the last-resort path when the structured log directory is unavailable.
    unsafe {
        libc::syslog(
            libc::LOG_ERR | libc::LOG_USER,
            c"%s".as_ptr(),
            message.as_ptr(),
        );
    }
}

#[cfg(not(unix))]
fn write_system_log(_message: &str) {}
