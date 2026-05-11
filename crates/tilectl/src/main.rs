//! `tilectl` — talk to the daemon over `\\.\pipe\tilemanager.sock`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tile_core::ipc::{Request, Response, PIPE_NAME};
use tile_core::workspace::WorkspaceId;
use tile_core::Direction;

#[derive(Parser)]
#[command(name = "tilectl", about = "Control TileManager")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Health-check the daemon.
    Ping,
    /// Print the current state as JSON.
    Dump,
    /// Reload the config file.
    Reload,
    /// Move focus.
    Focus  { dir: DirArg },
    /// Swap focused window with neighbour.
    Swap   { dir: DirArg },
    /// Resize: nudges the parent split.
    Resize { dir: DirArg, #[arg(default_value_t = 0.05)] delta: f32 },
    /// Toggle floating on the focused window.
    Float,
    /// Switch to a workspace (1..=9).
    Workspace { id: u16 },
    /// Move the focused window to a workspace.
    MoveTo    { id: u16 },
    /// Pull the focused window out of its tab group.
    Untab,
    /// Cycle to the next tab (or previous with --back).
    CycleTab {
        #[arg(long)]
        back: bool,
    },
    /// Tell the daemon to exit.
    Quit,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum DirArg { Left, Right, Up, Down }

impl From<DirArg> for Direction {
    fn from(d: DirArg) -> Self {
        match d { DirArg::Left=>Direction::Left, DirArg::Right=>Direction::Right, DirArg::Up=>Direction::Up, DirArg::Down=>Direction::Down }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let req = match cli.cmd {
        Cmd::Ping              => Request::Ping,
        Cmd::Dump              => Request::Dump,
        Cmd::Reload            => Request::ReloadConfig,
        Cmd::Focus  { dir }    => Request::FocusDirection  { dir: dir.into() },
        Cmd::Swap   { dir }    => Request::SwapDirection   { dir: dir.into() },
        Cmd::Resize { dir, delta } => Request::ResizeDirection { dir: dir.into(), delta },
        Cmd::Float             => Request::ToggleFloat,
        Cmd::Workspace { id }  => Request::SwitchWorkspace { id: WorkspaceId(id) },
        Cmd::MoveTo    { id }  => Request::MoveToWorkspace { id: WorkspaceId(id) },
        Cmd::Untab             => Request::UntabWindow,
        Cmd::CycleTab { back } => Request::CycleTab { forward: !back },
        Cmd::Quit              => Request::Quit,
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(send(req))
}

#[cfg(windows)]
async fn send(req: Request) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut client = ClientOptions::new()
        .open(PIPE_NAME)
        .with_context(|| format!("open {PIPE_NAME} (is the daemon running?)"))?;

    let payload = serde_json::to_string(&req)?;
    client.write_all(payload.as_bytes()).await?;
    client.write_all(b"\n").await?;

    let (read, _write) = tokio::io::split(client);
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await.context("read response")?;

    let resp: Response = serde_json::from_str(line.trim()).context("parse response")?;
    match resp {
        Response::Ok                          => println!("ok"),
        Response::Pong { version }            => println!("pong v{version}"),
        Response::State { json }              => println!("{json}"),
        Response::Error { message }           => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
async fn send(_req: Request) -> Result<()> {
    anyhow::bail!("tilectl only runs on Windows");
}
