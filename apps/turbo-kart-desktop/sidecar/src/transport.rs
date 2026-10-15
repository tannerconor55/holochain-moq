//! The local endpoint main listens on. Linux/macOS: a Unix domain socket in a
//! private directory main created. Windows (E7.WINDOWS.1): a named pipe main
//! created just before spawning this process, `\\.\pipe\turbo-kart-<128-bit
//! random hex>`; main accepts one connection that proves the session key.

use tokio::io::{AsyncRead, AsyncWrite};

pub trait Stream: AsyncRead + AsyncWrite + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + 'static> Stream for T {}

/// The only pipe names this sidecar connects to (main's `windowsPipeName`).
pub const PIPE_PREFIX: &str = r"\\.\pipe\turbo-kart-";

/// How long the sidecar keeps retrying a busy pipe before giving up.
pub const PIPE_CONNECT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether `endpoint` is a pipe name main can have made: the fixed prefix and
/// exactly 32 lowercase hex digits. Anything else is refused before opening.
pub fn is_pipe_name(endpoint: &str) -> bool {
    endpoint
        .strip_prefix(PIPE_PREFIX)
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

#[cfg(unix)]
pub async fn connect(endpoint: &str) -> std::io::Result<impl Stream> {
    tokio::net::UnixStream::connect(endpoint).await
}

/// Opens main's pipe. Retries only `ERROR_PIPE_BUSY` (all instances in use for
/// a moment), within [`PIPE_CONNECT_DEADLINE`]. tokio's client defaults keep
/// `SECURITY_IDENTIFICATION`, so the pipe server cannot impersonate this process.
#[cfg(windows)]
pub async fn connect(endpoint: &str) -> std::io::Result<impl Stream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    if !is_pipe_name(endpoint) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a kart-net pipe name"));
    }
    let deadline = tokio::time::Instant::now() + PIPE_CONNECT_DEADLINE;
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(client),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tokio::time::Instant::now() < deadline => {}
            Err(e) => return Err(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_are_exactly_the_ones_main_makes() {
        assert!(is_pipe_name(r"\\.\pipe\turbo-kart-0123456789abcdef0123456789abcdef"));
        for bad in [
            r"\\.\pipe\turbo-kart-0123456789abcdef0123456789abcde",   // 31 digits
            r"\\.\pipe\turbo-kart-0123456789abcdef0123456789abcdef0", // 33 digits
            r"\\.\pipe\turbo-kart-0123456789ABCDEF0123456789abcdef",  // uppercase
            r"\\.\pipe\turbo-kart-0123456789abcdef0123456789abcdeg",  // not hex
            r"\\server\pipe\turbo-kart-0123456789abcdef0123456789abcdef",
            r"\\.\pipe\other-0123456789abcdef0123456789abcdef",
            "/run/user/1000/tk-x/kart-net.sock",
            "",
        ] {
            assert!(!is_pipe_name(bad), "{bad}");
        }
    }
}
