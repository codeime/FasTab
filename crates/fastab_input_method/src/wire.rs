//! The one message this process ever writes: a caret position hook wrapped in a
//! `LocalMessage`, framed the way `fastab_ipc` frames it.
//!
//! Hand-encoded so the binary does not link prost and its descriptor pool for a
//! payload of a few dozen bytes. [`tests::frame_matches_fastab_proto`] and
//! [`tests::sourced_frame_matches_fastab_proto`] diff the bytes against the
//! generated encoder.

/// `\x1b@` plus the eight-byte type tag, per `fastab_proto::FigMessage`.
const FIG_PBUF_PREFIX: &[u8] = b"\x1b@fig-pbuf";

/// `LocalMessage.hook`, from `proto/local.proto`.
const LOCAL_MESSAGE_HOOK_FIELD: u32 = 3;
/// `Hook.caret_position`, from `proto/local.proto`.
const HOOK_CARET_POSITION_FIELD: u32 = 113;

const WIRE_TYPE_VARINT: u32 = 0;
const WIRE_TYPE_FIXED64: u32 = 1;
const WIRE_TYPE_LEN: u32 = 2;

/// `CaretPositionHook.Origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    #[allow(dead_code)] // The IMK caret rect is always bottom-left.
    TopLeft = 0,
    BottomLeft = 1,
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn push_tag(out: &mut Vec<u8>, field: u32, wire_type: u32) {
    push_varint(out, u64::from(field << 3 | wire_type));
}

/// Skips the field at its default, which is what proto3 and prost both do; the
/// desktop decodes an absent `double` as `0.0` either way.
fn push_double(out: &mut Vec<u8>, field: u32, value: f64) {
    if value == 0.0 {
        return;
    }
    push_tag(out, field, WIRE_TYPE_FIXED64);
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_message(out: &mut Vec<u8>, field: u32, body: &[u8]) {
    push_tag(out, field, WIRE_TYPE_LEN);
    push_varint(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// The client a caret was read from, so the desktop can drop a frame that
/// arrives after focus moved to another app or window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaretSource<'a> {
    pub bundle_id: Option<&'a str>,
    pub window_id: Option<u32>,
}

fn caret_position_hook(x: f64, y: f64, width: f64, height: f64, origin: Origin, source: CaretSource<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(38 + source.bundle_id.map_or(0, str::len));
    push_double(&mut out, 1, x);
    push_double(&mut out, 2, y);
    push_double(&mut out, 3, width);
    push_double(&mut out, 4, height);
    // `optional` fields 5-7 are written whenever present, even at their default.
    push_tag(&mut out, 5, WIRE_TYPE_VARINT);
    push_varint(&mut out, origin as u64);
    if let Some(bundle_id) = source.bundle_id {
        push_message(&mut out, 6, bundle_id.as_bytes());
    }
    if let Some(window_id) = source.window_id {
        push_tag(&mut out, 7, WIRE_TYPE_VARINT);
        push_varint(&mut out, u64::from(window_id));
    }
    out
}

/// A complete frame without a source, ready to write to the desktop socket.
#[cfg(test)]
pub fn caret_position_frame(x: f64, y: f64, width: f64, height: f64, origin: Origin) -> Vec<u8> {
    caret_position_frame_from(x, y, width, height, origin, CaretSource::default())
}

/// A complete frame, ready to write to the desktop socket.
pub fn caret_position_frame_from(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    origin: Origin,
    source: CaretSource<'_>,
) -> Vec<u8> {
    let hook = caret_position_hook(x, y, width, height, origin, source);

    let mut body = Vec::with_capacity(hook.len() + 8);
    let mut caret_field = Vec::with_capacity(hook.len() + 4);
    push_message(&mut caret_field, HOOK_CARET_POSITION_FIELD, &hook);
    push_message(&mut body, LOCAL_MESSAGE_HOOK_FIELD, &caret_field);

    let mut frame = Vec::with_capacity(FIG_PBUF_PREFIX.len() + size_of::<u64>() + body.len());
    frame.extend_from_slice(FIG_PBUF_PREFIX);
    frame.extend_from_slice(&(body.len() as u64).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

#[cfg(test)]
mod tests {
    use fastab_proto::FigProtobufEncodable;
    use fastab_proto::hooks::{hook_to_message, new_caret_position_hook};
    use fastab_proto::local::caret_position_hook::Origin as ProtoOrigin;

    use super::*;

    fn proto_origin(origin: Origin) -> ProtoOrigin {
        match origin {
            Origin::TopLeft => ProtoOrigin::TopLeft,
            Origin::BottomLeft => ProtoOrigin::BottomLeft,
        }
    }

    #[test]
    fn frame_matches_fastab_proto() {
        let cases = [
            (1200.0, 800.0, 8.0, 16.0, Origin::BottomLeft),
            (0.0, 0.0, 0.0, 16.0, Origin::TopLeft),
            (-1920.5, -450.25, 0.0, 21.0, Origin::BottomLeft),
            (f64::MAX, f64::MIN, 1.0, f64::MIN_POSITIVE, Origin::TopLeft),
        ];

        for (x, y, width, height, origin) in cases {
            let expected = hook_to_message(new_caret_position_hook(x, y, width, height, proto_origin(origin)))
                .encode_fastab_protobuf()
                .unwrap();
            assert_eq!(
                caret_position_frame(x, y, width, height, origin),
                expected.as_ref(),
                "frame mismatch for {x},{y},{width},{height},{origin:?}"
            );
        }
    }

    #[test]
    fn sourced_frame_matches_fastab_proto() {
        use fastab_proto::local::{CaretPositionHook, Hook, hook};

        let sources = [
            CaretSource {
                bundle_id: Some("com.mitchellh.ghostty"),
                window_id: Some(4211),
            },
            CaretSource {
                bundle_id: Some(""),
                window_id: Some(0),
            },
            CaretSource {
                bundle_id: Some("net.kovidgoyal.kitty"),
                window_id: None,
            },
            CaretSource {
                bundle_id: None,
                window_id: Some(u32::MAX),
            },
        ];
        for source in sources {
            let expected = hook_to_message(Hook {
                hook: Some(hook::Hook::CaretPosition(CaretPositionHook {
                    x: 12.5,
                    y: 900.0,
                    width: 1.0,
                    height: 18.0,
                    origin: Some(ProtoOrigin::BottomLeft as i32),
                    bundle_id: source.bundle_id.map(str::to_owned),
                    window_id: source.window_id,
                })),
            })
            .encode_fastab_protobuf()
            .unwrap();
            assert_eq!(
                caret_position_frame_from(12.5, 900.0, 1.0, 18.0, Origin::BottomLeft, source),
                expected.as_ref(),
                "frame mismatch for {source:?}"
            );
        }
    }

    /// Beyond byte equality with the encoder: the exact parser the desktop app
    /// runs must accept the frame and hand back the same caret.
    #[test]
    fn frame_decodes_through_the_desktop_parser() {
        use fastab_proto::local::{LocalMessage, hook, local_message};

        let source = CaretSource {
            bundle_id: Some("com.mitchellh.ghostty"),
            window_id: Some(4211),
        };
        let frame = caret_position_frame_from(432.5, 918.0, 1.0, 21.0, Origin::BottomLeft, source);
        let mut buf = frame.as_slice();
        let (consumed, message) = fastab_proto::FigMessage::parse(&mut buf).unwrap();
        assert_eq!(consumed, frame.len());

        let decoded: LocalMessage = message.decode().unwrap();
        let Some(local_message::Type::Hook(hook)) = decoded.r#type else {
            panic!("expected a hook message, got {decoded:?}");
        };
        let Some(hook::Hook::CaretPosition(caret)) = hook.hook else {
            panic!("expected a caret position hook, got {hook:?}");
        };
        assert_eq!((caret.x, caret.y, caret.width, caret.height), (432.5, 918.0, 1.0, 21.0));
        assert_eq!(caret.origin, Some(ProtoOrigin::BottomLeft as i32));
        assert_eq!(caret.bundle_id.as_deref(), Some("com.mitchellh.ghostty"));
        assert_eq!(caret.window_id, Some(4211));
    }

    #[test]
    fn varint_spans_multiple_bytes() {
        let mut out = Vec::new();
        push_tag(&mut out, HOOK_CARET_POSITION_FIELD, WIRE_TYPE_LEN);
        assert_eq!(out, [0x8a, 0x07]);
    }
}
