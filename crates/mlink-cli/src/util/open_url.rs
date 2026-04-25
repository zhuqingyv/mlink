//! Spawn the platform's default URL handler. No `open` crate dependency —
//! three one-liners per OS is cheaper than another transitive tree. Errors
//! bubble up so the caller can print a manual-click fallback.

pub(crate) fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        // `cmd /C start "" <url>` is the only invocation that handles URLs
        // containing `&` correctly; the empty `""` is the window-title slot.
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no known browser-opener on this platform",
        ))
    }
}
