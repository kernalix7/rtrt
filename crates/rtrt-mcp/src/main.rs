//! rtrt-mcp — MCP server exposing compress / memory / provider tools.
//!
//! Currently a stub that prints capabilities and exits. Real stdio loop lands when the MCP
//! crate is wired in (see roadmap).

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("rtrt=info").init();
    tracing::info!("rtrt-mcp starting");
    tracing::info!("tools: compress, memory.save, memory.recall, provider.chat");
    tracing::warn!("stdio transport not wired yet — exiting");
    Ok(())
}
