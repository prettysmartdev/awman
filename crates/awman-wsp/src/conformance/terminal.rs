//! Replacement terminal sockets retain the same lease and fencing epoch.
use super::{heartbeat, result, work_item, ConformanceAdmin};
use crate::{terminal::LeaseTerminalFrame, terminal::TerminalFrame, Client, Error};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;

/// How long the superseded socket may take to close.
const CLOSE_DEADLINE: Duration = Duration::from_secs(2);
/// How long the replacement must stay open once the old one closed.
const STAYS_OPEN: Duration = Duration::from_millis(300);

fn step(ok: bool, name: &'static str) -> Result<(), Error> {
    if ok {
        Ok(())
    } else {
        Err(Error::ConformanceCheck(name))
    }
}

/// Accept a second socket at the same epoch and close the superseded socket.
/// The replacement stays open, a stale-epoch handshake is refused with
/// `409 lease_lost`, and the lease stays usable for heartbeat and result.
pub async fn reconnect(
    item_id: &str,
    client: &Client,
    admin: &dyn ConformanceAdmin,
) -> Result<(), Error> {
    admin.enqueue(work_item(item_id)).await;
    admin.enable_terminal(item_id).await;
    let lease = client
        .claim(Some(item_id), 0)
        .await?
        .ok_or(Error::ConformanceCheck(
            "terminal_reconnect: claim returned no lease",
        ))?;
    step(
        lease.terminal,
        "terminal_reconnect: lease does not offer a terminal",
    )?;
    let client = client.with_lease_epoch(&lease.id, lease.epoch);
    let mut first = client.open_terminal(&lease.id).await?;
    let mut replacement = client.open_terminal(&lease.id).await?;
    // No protocol acknowledgement exists for terminal output. Replacement is
    // evidenced by its accepted handshake and closure of the superseded
    // stream; the server may still send ordinary frames (such as `detached`)
    // on the old socket before closing it.
    let closed = tokio::time::timeout(CLOSE_DEADLINE, async {
        loop {
            match first.next().await {
                Some(Ok(_)) => continue,
                None | Some(Err(Error::TerminalTransport(_))) => return true,
                Some(Err(_)) => return false,
            }
        }
    })
    .await;
    step(
        closed == Ok(true),
        "terminal_reconnect: the superseded socket was not closed",
    )?;
    // The replacement must still be open: nothing, or ordinary frames, but no
    // close or transport failure.
    let open = tokio::time::timeout(STAYS_OPEN, async {
        loop {
            match replacement.next().await {
                Some(Ok(_)) => continue,
                _ => return,
            }
        }
    })
    .await
    .is_err();
    step(
        open,
        "terminal_reconnect: the replacement socket was closed",
    )?;
    replacement
        .send(LeaseTerminalFrame {
            terminal_id: "term_11111111111111111111111111111111".into(),
            frame: TerminalFrame::Snapshot {
                has_output: false,
                cols: 80,
                rows: 24,
                screen: String::new(),
            },
        })
        .await?;
    // A handshake at an older epoch is fenced like any other lease call.
    if lease.epoch > 0 {
        let stale = client.with_lease_epoch(&lease.id, lease.epoch - 1);
        step(
            stale
                .open_terminal(&lease.id)
                .await
                .is_err_and(|error| error.is_lease_lost()),
            "terminal_reconnect: a stale-epoch handshake was not refused with 409 lease_lost",
        )?;
    }
    client.heartbeat(&lease.id, &heartbeat()).await?;
    client.result(&lease.id, &result()).await?;
    Ok(())
}
