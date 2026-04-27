//! F-608 step 2: `forged-agent` binary entry point.
//!
//! Usage: `forged-agent --socket <path> --instance-id <id>`.
//!
//! Connects to the daemon-bound UDS at `--socket`, sends `Hello`, awaits
//! `HelloAck`, then enters the dispatch loop. See
//! [`forge_agent_host`] for the full lifecycle contract.

use anyhow::Result;
use forge_agent_host::{init_tracing, run, AgentArgs};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = AgentArgs::parse_from(std::env::args())?;
    run(args).await
}
