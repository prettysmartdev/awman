//! Driver-local deadline waits: bins cannot use dev-dependencies.
use std::time::Duration;

pub async fn until<T>(
    limit: Duration,
    mut condition: impl FnMut() -> Option<T>,
    diagnostic: impl Fn() -> String,
) -> Result<T, String> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(value) = condition() {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(diagnostic());
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(20)).min(deadline),
        )
        .await;
    }
}
