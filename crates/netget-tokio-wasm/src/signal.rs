//! `tokio::signal` with no signals: a page has no Ctrl+C, so `ctrl_c` never completes.

use std::io;

pub async fn ctrl_c() -> io::Result<()> {
    futures::future::pending::<()>().await;
    Ok(())
}
