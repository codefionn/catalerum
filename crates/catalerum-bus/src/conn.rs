//! Dedicated connections for **blocking** stream reads (XREAD/XREADGROUP BLOCK).
//!
//! redis 1.x gives every async connection a default **500 ms response
//! timeout** (`AsyncConnectionConfig::default`). A `BLOCK <ms>` read occupies
//! its connection at the server for up to `<ms>`, so with any block window
//! past 500 ms a default-config connection kills its own read mid-block
//! ("timed out") — the forwarder then backs off and the stream degrades to
//! ~1 s polling with warn spam. Every blocking read must therefore bound its
//! response timeout *past its block window*, never inherit the default.

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::AsyncConnectionConfig;

/// Slack past the server-side block window before the client gives up:
/// covers RTT + a slow reply without masking a dead server for long.
const BLOCK_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);

/// Open a dedicated connection whose response timeout outlives a
/// `BLOCK block_ms` read. Dedicated (not the shared manager) so a blocked
/// read can't head-of-line block ordinary commands.
pub(crate) async fn blocking_read_connection(
    client: &redis::Client,
    block_ms: u64,
) -> redis::RedisResult<MultiplexedConnection> {
    let config = AsyncConnectionConfig::new()
        .set_response_timeout(Some(Duration::from_millis(block_ms) + BLOCK_TIMEOUT_MARGIN));
    client
        .get_multiplexed_async_connection_with_config(&config)
        .await
}
