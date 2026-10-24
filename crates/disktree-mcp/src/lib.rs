//! `disktree mcp`: disktree as a Model Context Protocol server over stdio.
//!
//! An agent runs `disktree mcp` as a subprocess, the same way it would run
//! any other tool, and talks to it over stdin and stdout. There is no port,
//! no token and no sign-in: the agent already runs as this user, and this
//! process can do no more than the user's shell can. It scans with
//! `disktree-core` itself, so no window is needed; when a window is open it
//! follows along over [`disktree_link`], and a window can be opened for the
//! review screen.
//!
//! The agent marks; the person removes. There is no tool that deletes.
//! [`Session`] holds what one connected agent knows; [`server`] is the
//! protocol around it.

pub mod report;
pub mod server;
pub mod session;

pub use session::Session;

/// The free-space procedure, served as a prompt and a resource so the
/// agent can read how to use the tools well before it starts.
pub const SKILL: &str =
    include_str!("../../../skills/disktree-free-space/SKILL.md");

/// Serve MCP on stdin and stdout until the client hangs up.
pub fn serve() -> anyhow::Result<()> {
    use rmcp::ServiceExt as _;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = server::Server::new(Session::new());
        let running = server.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    })
}
