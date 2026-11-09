fn read_daemon_env() { std::env::vars_os(); }
fn read_daemon_cwd() { std::env::current_dir(); }
fn read_daemon_home() { dirs::home_dir(); }
fn recapture_daemon() { Env::from_process(); }
#[cfg(test)] mod tests { fn allowed_test_read() { std::env::var("TEST"); } }
