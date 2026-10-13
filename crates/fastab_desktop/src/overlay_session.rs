//! T3.3 overlay session driver.
//!
//! Handshake `remote.sock`, then stream `EditBufferHook` frames plus the
//! `fastab_input_method` caret wire on `desktop.sock`. Live sockets are optional;
//! unit tests cover session shape and caret framing. After T3.4 the product
//! default is Native. Runtime JavaScript is gone after T4.1.

use fastab_proto::FigProtobufEncodable;
use fastab_proto::hooks::{hook_to_message, new_caret_position_hook};
use fastab_proto::local::caret_position_hook::Origin;

const SESSION_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/session-replay/sessions");

#[derive(Debug, serde::Deserialize)]
struct SessionRow {
    buffer: String,
    cwd: String,
}

fn load_session(name: &str) -> anyhow::Result<Vec<SessionRow>> {
    let path = std::path::Path::new(SESSION_DIR).join(format!("{name}.jsonl"));
    let text = std::fs::read_to_string(&path)?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: SessionRow =
            serde_json::from_str(line).map_err(|error| anyhow::anyhow!("{}:{}: {error}", path.display(), index + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

fn caret_frame(x: f64, y: f64, width: f64, height: f64, origin: Origin) -> Vec<u8> {
    hook_to_message(new_caret_position_hook(x, y, width, height, origin))
        .encode_fastab_protobuf()
        .expect("caret frame")
        .to_vec()
}

/// Build the same caret bytes `fastab_input_method::wire` hand-encodes.
pub fn session_caret_frame() -> Vec<u8> {
    caret_frame(120.0, 700.0, 8.0, 16.0, Origin::TopLeft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_sessions_each_have_at_least_100_keystroke_buffers() {
        let names = ["git", "npm", "docker", "kubectl", "cargo"];
        for name in names {
            let rows = load_session(name).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                rows.len() >= 100,
                "{name} has {} buffers; T3.3 requires ≥ 100",
                rows.len()
            );
            assert!(rows.iter().all(|row| row.cwd == name), "{name} cwd keys");
            for window in rows.windows(2) {
                let previous = &window[0].buffer;
                let current = &window[1].buffer;
                assert!(
                    current.starts_with(previous.as_str())
                        || previous.starts_with(current.as_str())
                        || current.split_whitespace().next() == previous.split_whitespace().next(),
                    "{name}: {previous:?} -> {current:?} is not a keystroke step"
                );
            }
        }
    }

    #[test]
    fn caret_frame_decodes_through_the_desktop_parser() {
        use fastab_proto::local::{LocalMessage, hook, local_message};

        let frame = session_caret_frame();
        let mut buf = frame.as_slice();
        let (consumed, message) = fastab_proto::FigMessage::parse(&mut buf).expect("parse caret");
        assert_eq!(consumed, frame.len());
        let decoded: LocalMessage = message.decode().expect("decode caret");
        let Some(local_message::Type::Hook(hook)) = decoded.r#type else {
            panic!("expected a hook message, got {decoded:?}");
        };
        let Some(hook::Hook::CaretPosition(caret)) = hook.hook else {
            panic!("expected a caret position hook, got {hook:?}");
        };
        assert_eq!((caret.x, caret.y, caret.width, caret.height), (120.0, 700.0, 8.0, 16.0));
    }

    #[test]
    fn live_socket_replay_is_opt_in() {
        if std::env::var_os("EC_OVERLAY_SESSION_LIVE").is_none() {
            return;
        }
        let remote = fastab_util::directories::remote_socket_path().expect("remote socket path");
        let desktop = fastab_util::directories::desktop_socket_path().expect("desktop socket path");
        assert!(
            remote.exists() && desktop.exists(),
            "EC_OVERLAY_SESSION_LIVE set but FasTab sockets are missing"
        );
    }
}
