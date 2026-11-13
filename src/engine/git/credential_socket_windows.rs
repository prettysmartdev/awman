//! Owner-only Windows named pipes, using the binary's existing ACL/identity shims.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) fn serve(
    path: &Path,
    _mode: u32,
    reply: impl Fn(&[u8]) -> Option<String> + Send + 'static,
    _same_uid: bool,
    stop: Arc<AtomicBool>,
) -> Result<Serving> {
    let path = pipe_path(path)?;
    let active = Arc::new(AtomicBool::new(false));
    let serving = active.clone();
    let (ready, started) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("awman-git-credential".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    let _ = ready.send(Err(cfg_err("could not start credential pipe runtime")));
                    return;
                }
            };
            runtime.block_on(async {
                let mut pipe = match crate::engine::remote::local::owner_pipe(&path, true) {
                    Ok(pipe) => pipe,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while !stop.load(Ordering::SeqCst) {
                    tokio::select! {
                        result = pipe.connect() => { if result.is_err() { break; } }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
                    }
                    if crate::engine::remote::local::verify_pipe_client(&pipe) {
                        serving.store(true, Ordering::SeqCst);
                        let exchange = async {
                            let len = pipe.read_u32_le().await? as u64;
                            if len > MAX_REQUEST_BYTES {
                                return Ok::<(), std::io::Error>(());
                            }
                            let mut request = vec![0; len as usize];
                            pipe.read_exact(&mut request).await?;
                            if !stop.load(Ordering::SeqCst) {
                                let answer = reply(&request).unwrap_or_default();
                                pipe.write_u32_le(answer.len() as u32).await?;
                                pipe.write_all(answer.as_bytes()).await?;
                                // Disconnect discards unread bytes on Windows. Wait until
                                // the client has consumed the complete response.
                                pipe.read_u8().await?;
                            }
                            Ok(())
                        };
                        let stopped = async {
                            while !stop.load(Ordering::SeqCst) {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        };
                        tokio::select! {
                            _ = tokio::time::timeout(IO_TIMEOUT, exchange) => {},
                            _ = stopped => {},
                        }
                        serving.store(false, Ordering::SeqCst);
                    }
                    if pipe.disconnect().is_err() {
                        break;
                    }
                }
            });
        })
        .map_err(|_| cfg_err("could not start credential pipe thread"))?;
    match started.recv() {
        Ok(Ok(())) => Ok(Serving { thread, active }),
        result => {
            let _ = thread.join();
            Err(result
                .ok()
                .and_then(std::result::Result::err)
                .unwrap_or_else(|| cfg_err("credential pipe failed to start")))
        }
    }
}

pub(super) fn exchange(socket: &Path, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() as u64 > MAX_REQUEST_BYTES {
        return Err(cfg_err("credential request too large"));
    }
    let socket = pipe_path(socket)?;
    // The helper is synchronous and can also be embedded in a Tokio caller.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| cfg_err("could not start credential pipe client"))?;
                runtime.block_on(async {
                    tokio::time::timeout(IO_TIMEOUT, async {
                        let mut pipe = loop {
                            match tokio::net::windows::named_pipe::ClientOptions::new()
                                .open(&socket)
                            {
                                Ok(pipe) => break pipe,
                                Err(error) if error.raw_os_error() == Some(231) => {
                                    tokio::time::sleep(Duration::from_millis(10)).await
                                }
                                Err(_) => return Err(cfg_err("could not open credential pipe")),
                            }
                        };
                        let io = |_| cfg_err("credential pipe exchange failed");
                        pipe.write_u32_le(body.len() as u32).await.map_err(io)?;
                        pipe.write_all(body).await.map_err(io)?;
                        let len = pipe.read_u32_le().await.map_err(io)? as u64;
                        if len > MAX_REQUEST_BYTES {
                            return Err(cfg_err("credential reply too large"));
                        }
                        let mut response = vec![0; len as usize];
                        pipe.read_exact(&mut response).await.map_err(io)?;
                        pipe.write_u8(1).await.map_err(io)?;
                        Ok(response)
                    })
                    .await
                    .map_err(|_| cfg_err("credential pipe timed out"))?
                })
            })
            .join()
            .map_err(|_| cfg_err("credential pipe client failed"))?
    })
}

fn pipe_path(endpoint: &Path) -> Result<PathBuf> {
    let name = endpoint
        .to_str()
        .ok_or_else(|| cfg_err("invalid credential pipe name"))?;
    let nonce = name
        .strip_prefix("awman-git-")
        .ok_or_else(|| cfg_err("invalid credential pipe name"))?;
    if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(cfg_err("invalid credential pipe name"));
    }
    Ok(PathBuf::from(format!(r"\\.\pipe\{name}")))
}
