//! Manual overlay driver: inject figterm edit-buffer hooks so the running
//! desktop app shows the suggestion list without a real terminal.
//!
//! This is a live-app smoke tool, not a `cargo test`. It needs FasTab
//! running (`remote.sock` + `desktop.sock`). Buffers are sent one character
//! at a time so the overlay does not treat the change as a paste.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, ValueEnum};
use eyre::{Context, Result, bail};
use fastab_ipc::{BufferedReader, RecvMessage, SendMessage, socket_connect_timeout};
use fastab_proto::hooks::new_caret_position_hook;
use fastab_proto::local::ShellContext;
use fastab_proto::local::caret_position_hook::Origin;
use fastab_proto::remote::{Clientbound, Hostbound, clientbound, hostbound};
use fastab_proto::remote_hooks::{hook_to_message, new_edit_buffer_hook};
use fastab_util::directories;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const DEFAULT_BUFFER: &str = "git ch";
const BACKSPACE_CYCLE: &[&str] = &["g", "gu", "gut", "gu", "g", "gi", "git"];

/// Built-in keystroke pattern for [`DriveOverlayArgs`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OverlayDriveScenario {
    /// Type `--buffer` one character at a time.
    #[default]
    Type,
    /// `g` → `gu` → `gut` → backspace → `gi` → `git`.
    Backspace,
}

/// Inject edit-buffer hooks so the running overlay appears.
#[derive(Debug, PartialEq, Args)]
pub struct DriveOverlayArgs {
    /// Buffer to type one character at a time. Default: `git ch`.
    #[arg(long)]
    pub buffer: Option<String>,
    /// Built-in keystroke pattern.
    #[arg(long, value_enum, default_value_t = OverlayDriveScenario::Type)]
    pub scenario: OverlayDriveScenario,
    /// Working directory reported to the engine.
    #[arg(long)]
    pub cwd: Option<String>,
    /// Delay between injected keystrokes.
    #[arg(long, default_value_t = 80)]
    pub delay_ms: u64,
    /// Keep the last frame visible before hiding.
    #[arg(long, default_value_t = 2000)]
    pub hold_ms: u64,
    /// Repeat the pattern this many times.
    #[arg(long, default_value_t = 1)]
    pub cycles: u32,
    /// Quartz caret X (top-left, primary display).
    #[arg(long, default_value_t = 120.0)]
    pub x: f64,
    /// Quartz caret Y (top-left, primary display).
    #[arg(long, default_value_t = 700.0)]
    pub y: f64,
    /// Replay a T3.3 JSONL session (`{buffer, cwd}`) instead of `--buffer`.
    #[arg(long)]
    pub session: Option<PathBuf>,
}

pub async fn execute(args: &DriveOverlayArgs) -> Result<ExitCode> {
    if args.cycles == 0 {
        bail!("--cycles must be at least 1");
    }

    let remote_path = directories::remote_socket_path()?;
    let desktop_path = directories::desktop_socket_path()?;
    if !remote_path.exists() || !desktop_path.exists() {
        bail!("FasTab is not running (missing {remote_path:?} or {desktop_path:?})");
    }

    let session_frames = args.session.as_deref().map(load_session_frames).transpose()?;
    let cwd = args
        .cwd
        .clone()
        .or_else(|| session_frames.as_ref().and_then(|session| session.cwd.clone()))
        .unwrap_or_else(|| std::env::current_dir().map_or_else(|_err| "/".into(), |path| path.display().to_string()));
    let frames = match session_frames {
        Some(session) => session.buffers,
        None => frames_for(&args.scenario, args.buffer.as_deref()),
    };
    let delay = Duration::from_millis(args.delay_ms);
    let hold = Duration::from_millis(args.hold_ms);

    let session_id = Uuid::new_v4().to_string();
    let context = shell_context(&session_id, &cwd);

    let stream = socket_connect_timeout(&remote_path, Duration::from_secs(3))
        .await
        .with_context(|| format!("connecting to {}", remote_path.display()))?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufferedReader::new(reader);

    writer
        .send_message(Hostbound {
            packet: Some(hostbound::Packet::Handshake(hostbound::Handshake {
                id: session_id.clone(),
                secret: "overlay-drive".into(),
                parent_id: None,
            })),
        })
        .await
        .context("sending overlay-drive handshake")?;

    wait_for_handshake(&mut reader).await?;
    tokio::spawn(async move {
        let mut reader = reader;
        while reader.recv_message::<Clientbound>().await.ok().flatten().is_some() {}
    });

    send_caret(args.x, args.y).await?;

    eprintln!(
        "Driving overlay at Quartz ({:.0}, {:.0}); look near the lower-left of the primary display.",
        args.x, args.y
    );

    let mut nonce = 0_u64;
    for cycle in 0..args.cycles {
        if args.cycles > 1 {
            eprintln!("cycle {}/{}", cycle + 1, args.cycles);
        }
        for frame in &frames {
            nonce += 1;
            send_caret(args.x, args.y).await?;
            send_edit_buffer(&mut writer, nonce, context.clone(), frame).await?;
            tokio::time::sleep(delay).await;
        }
        tokio::time::sleep(hold).await;
        nonce += 1;
        send_edit_buffer(&mut writer, nonce, context.clone(), "").await?;
        fastab_ipc::local::send_hook_to_socket(fastab_proto::hooks::new_hide_hook())
            .await
            .ok();
    }

    writer.flush().await.ok();
    eprintln!("Done.");
    Ok(ExitCode::SUCCESS)
}

struct SessionFrames {
    buffers: Vec<String>,
    cwd: Option<String>,
}

fn load_session_frames(path: &std::path::Path) -> Result<SessionFrames> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buffers = Vec::new();
    let mut cwd = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), index + 1))?;
        let buffer = row
            .get("buffer")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| eyre::eyre!("{}:{} missing buffer", path.display(), index + 1))?;
        buffers.push(buffer.to_string());
        if cwd.is_none() {
            cwd = row
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
        }
    }
    if buffers.len() < 100 {
        bail!(
            "session {} has {} buffers; T3.3 requires ≥ 100",
            path.display(),
            buffers.len()
        );
    }
    Ok(SessionFrames { buffers, cwd })
}

fn frames_for(scenario: &OverlayDriveScenario, buffer: Option<&str>) -> Vec<String> {
    match scenario {
        OverlayDriveScenario::Type => typed_prefixes(buffer.unwrap_or(DEFAULT_BUFFER)),
        OverlayDriveScenario::Backspace => BACKSPACE_CYCLE.iter().map(|frame| (*frame).to_string()).collect(),
    }
}

fn typed_prefixes(text: &str) -> Vec<String> {
    text.char_indices()
        .map(|(index, ch)| text[..index + ch.len_utf8()].to_string())
        .collect()
}

fn shell_context(session_id: &str, cwd: &str) -> ShellContext {
    ShellContext {
        pid: Some(std::process::id() as i32),
        ttys: Some("/dev/ttys-overlay-drive".into()),
        process_name: Some("zsh".into()),
        current_working_directory: Some(cwd.into()),
        session_id: Some(session_id.into()),
        terminal: Some("overlay-drive".into()),
        hostname: Some("overlay-drive".into()),
        shell_path: Some("/bin/zsh".into()),
        ..Default::default()
    }
}

async fn wait_for_handshake<R>(reader: &mut BufferedReader<R>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for overlay-drive handshake");
        }
        let message = tokio::time::timeout(remaining, reader.recv_message::<Clientbound>())
            .await
            .context("timed out waiting for overlay-drive handshake")?
            .context("reading overlay-drive handshake")?;
        match message {
            Some(Clientbound {
                packet: Some(clientbound::Packet::HandshakeResponse(response)),
            }) => {
                if response.success {
                    return Ok(());
                }
                bail!("desktop rejected the overlay-drive handshake");
            },
            Some(_) => {},
            None => bail!("desktop closed the overlay-drive handshake"),
        }
    }
}

async fn send_caret(x: f64, y: f64) -> Result<()> {
    fastab_ipc::local::send_hook_to_socket(new_caret_position_hook(x, y, 8.0, 16.0, Origin::TopLeft))
        .await
        .context("sending caret hook")
}

async fn send_edit_buffer<W>(writer: &mut W, nonce: u64, context: ShellContext, text: &str) -> Result<()>
where
    W: SendMessage + Send,
{
    let mut request = new_edit_buffer_hook(context, text, text.len() as i64, nonce as i64, None);
    request.nonce = Some(nonce);
    writer
        .send_message(hook_to_message(request))
        .await
        .with_context(|| format!("sending edit buffer {text:?}"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    #[command(name = "drive-overlay")]
    struct Parse {
        #[command(flatten)]
        args: DriveOverlayArgs,
    }

    #[test]
    fn loads_t3_session_keystroke_frames() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/session-replay/sessions/git.jsonl");
        let session = load_session_frames(&path).expect("git session");
        assert!(session.buffers.len() >= 100);
        assert_eq!(session.cwd.as_deref(), Some("git"));
        assert_eq!(session.buffers[0], "g");
        assert_eq!(session.buffers[1], "gi");
    }

    #[test]
    fn types_one_character_at_a_time() {
        assert_eq!(
            typed_prefixes("git ch"),
            vec!["g", "gi", "git", "git ", "git c", "git ch"]
        );
        assert_eq!(typed_prefixes(""), Vec::<String>::new());
    }

    #[test]
    fn backspace_scenario_stays_one_edit_per_frame() {
        let frames = frames_for(&OverlayDriveScenario::Backspace, None);
        assert_eq!(frames, BACKSPACE_CYCLE);
        for window in frames.windows(2) {
            let previous = &window[0];
            let current = &window[1];
            assert!(
                previous.starts_with(current.as_str()) || current.starts_with(previous.as_str()),
                "{previous:?} -> {current:?} would look like a paste"
            );
            let delta = (previous.encode_utf16().count() as i64 - current.encode_utf16().count() as i64).abs();
            assert!(delta < 2, "{previous:?} -> {current:?} changes {delta} UTF-16 units");
        }
    }

    #[test]
    fn parses_default_and_overrides() {
        let default = Parse::parse_from(["drive-overlay"]).args;
        assert_eq!(default.scenario, OverlayDriveScenario::Type);
        assert_eq!(default.delay_ms, 80);
        assert_eq!(default.hold_ms, 2000);
        assert_eq!(default.cycles, 1);
        assert_eq!(default.x, 120.0);
        assert_eq!(default.y, 700.0);

        let custom = Parse::parse_from([
            "drive-overlay",
            "--buffer",
            "python ",
            "--scenario",
            "backspace",
            "--cwd",
            "/tmp",
            "--delay-ms",
            "20",
            "--hold-ms",
            "500",
            "--cycles",
            "3",
            "--x",
            "10",
            "--y",
            "20",
        ])
        .args;
        assert_eq!(
            custom,
            DriveOverlayArgs {
                buffer: Some("python ".into()),
                scenario: OverlayDriveScenario::Backspace,
                cwd: Some("/tmp".into()),
                delay_ms: 20,
                hold_ms: 500,
                cycles: 3,
                x: 10.0,
                y: 20.0,
                session: None,
            }
        );
    }
}
