//! Bounded gh process protocol. No shell, no ambient environment, no raw diagnostics.
use super::super::transport::RESPONSE_LIMIT;
use std::{
    io::{Read, Write},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub(super) enum Failure {
    NotStarted,
    Timeout,
    TooLarge,
    Io,
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.0.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn drain(pipe: impl Read + Send + 'static) -> mpsc::Receiver<Result<Vec<u8>, Failure>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = match pipe.take(RESPONSE_LIMIT + 1).read_to_end(&mut bytes) {
            Err(_) => Err(Failure::Io),
            Ok(_) if bytes.len() as u64 > RESPONSE_LIMIT => Err(Failure::TooLarge),
            Ok(_) => Ok(bytes),
        };
        let _ = tx.send(result);
    });
    rx
}
fn collect<T>(rx: mpsc::Receiver<Result<T, Failure>>, deadline: Instant) -> Result<T, Failure> {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| Failure::Timeout)?
}
pub(super) fn run(cmd: &mut Command, input: Option<&[u8]>) -> Result<Output, Failure> {
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = OwnedChild(cmd.spawn().map_err(|_| Failure::NotStarted)?);
    let deadline = Instant::now() + Duration::from_secs(15);
    let stdout = drain(child.0.stdout.take().ok_or(Failure::Io)?);
    let stderr = drain(child.0.stderr.take().ok_or(Failure::Io)?);
    let writer = match (input, child.0.stdin.take()) {
        (Some(input), Some(mut pipe)) => {
            let input = input.to_vec();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(pipe.write_all(&input).map_err(|_| Failure::Io));
            });
            Some(rx)
        }
        _ => None,
    };
    let status = loop {
        if Instant::now() >= deadline {
            return Err(Failure::Timeout);
        }
        if let Some(status) = child.0.try_wait().map_err(|_| Failure::Io)? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = collect(stdout, deadline)?;
    let stderr = collect(stderr, deadline)?;
    // A rejected request may close stdin immediately. Its exit status is then
    // the definitive rejection; a successful mutation needs a complete write.
    if let Some(writer) = writer {
        let written = collect(writer, deadline);
        if status.success() {
            written?;
        }
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
