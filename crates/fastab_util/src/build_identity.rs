//! Identity of this mapped process, independent of files replaced by an update.
//! Kept std-only so the IME can include this leaf without the desktop stack.
//!
//! The IME compiles this file with `build_identity_from_bundle` and reads its
//! build facts from its own Info.plist instead of baking them in: a commit,
//! date or version string in the executable would change it on every release,
//! and installs replace a running IME (cutting every terminal window's IMK
//! connection) only when its code digest changed.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static START: OnceLock<(SystemTime, Instant)> = OnceLock::new();

pub fn initialize() {
    START.get_or_init(|| (SystemTime::now(), Instant::now()));
}

pub fn json(component: &str) -> String {
    let (started, elapsed) = START.get_or_init(|| (SystemTime::now(), Instant::now()));
    let build = build_fields();
    let fields = [("component", Some(component))]
        .into_iter()
        .chain(build.iter().map(|(key, value)| (*key, value.as_deref())));
    let mut output = format!(
        "{{\"pid\":{},\"started_at_unix_ms\":{},\"uptime_ms\":{}",
        std::process::id(),
        started.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        elapsed.elapsed().as_millis(),
    );
    for (key, value) in fields {
        write!(&mut output, ",\"{key}\":").unwrap();
        match value {
            Some(value) => quote(value, &mut output),
            None => output.push_str("null"),
        }
    }
    output.push_str(",\"mapped_image_uuid\":");
    match mapped_image_uuid() {
        Some(uuid) => quote(&uuid, &mut output),
        None => output.push_str("null"),
    }
    output.push('}');
    output
}

type BuildFields = [(&'static str, Option<String>); 5];

#[cfg(not(build_identity_from_bundle))]
fn build_fields() -> BuildFields {
    [
        ("version", Some(env!("CARGO_PKG_VERSION").to_owned())),
        ("commit", option_env!("AMAZON_Q_BUILD_HASH").map(str::to_owned)),
        ("built_at", option_env!("AMAZON_Q_BUILD_DATETIME").map(str::to_owned)),
        ("target", option_env!("AMAZON_Q_BUILD_TARGET_TRIPLE").map(str::to_owned)),
        ("run_id", option_env!("FASTAB_BUILD_RUN_ID").map(str::to_owned)),
    ]
}

#[cfg(build_identity_from_bundle)]
fn build_fields() -> BuildFields {
    // `<bundle>/Contents/MacOS/<exe>` → `<bundle>/Contents/Info.plist`.
    let plist = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("Info.plist")))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let value = |key| plist_string(&plist, key);
    [
        ("version", value("CFBundleShortVersionString")),
        ("commit", value("FastabBuildCommit")),
        ("built_at", value("FastabBuildDate")),
        ("target", value("FastabBuildTarget")),
        ("run_id", value("FastabBuildRunId")),
    ]
}

/// The `<string>` after `<key>key</key>` in an XML property list. Enough for
/// the flat Info.plist the IME's build script writes; not a general parser.
#[cfg_attr(not(any(build_identity_from_bundle, test)), allow(dead_code))]
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let rest = plist[plist.find(&marker)? + marker.len()..].trim_start();
    let value = rest.strip_prefix("<string>")?;
    let value = &value[..value.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

fn quote(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            character if character < ' ' => write!(output, "\\u{:04x}", character as u32).unwrap(),
            character => output.push(character),
        }
    }
    output.push('"');
}

#[cfg(target_os = "macos")]
fn mapped_image_uuid() -> Option<String> {
    unsafe extern "C" {
        fn _dyld_get_image_header(index: u32) -> *const u8;
    }
    // SAFETY: dyld owns the immutable main image header and its load commands
    // for the process lifetime. Read the 64-bit header used by supported Macs;
    // bound traversal by sizeofcmds and each command's declared size.
    unsafe {
        let header = _dyld_get_image_header(0);
        if header.is_null() || header.cast::<u32>().read_unaligned() != 0xfeedfacf {
            return None;
        }
        let count = header.add(16).cast::<u32>().read_unaligned() as usize;
        let size = header.add(20).cast::<u32>().read_unaligned() as usize;
        let commands = std::slice::from_raw_parts(header.add(32), size);
        let mut offset: usize = 0;
        for _ in 0..count {
            let prefix = commands.get(offset..offset.checked_add(8)?)?;
            let kind = u32::from_ne_bytes(prefix[..4].try_into().ok()?);
            let len = u32::from_ne_bytes(prefix[4..8].try_into().ok()?) as usize;
            if len < 8 {
                return None;
            }
            let command = commands.get(offset..offset.checked_add(len)?)?;
            if kind == 0x1b {
                let bytes = command.get(8..24)?;
                let mut uuid = String::with_capacity(32);
                for byte in bytes {
                    write!(&mut uuid, "{byte:02x}").unwrap();
                }
                return Some(uuid);
            }
            offset += len;
        }
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn mapped_image_uuid() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn plist_strings_are_read_by_key() {
        let plist = "<dict>\n\t<key>CFBundleShortVersionString</key>\n\t<string>0.0.19</string>\n\t\
                     <key>FastabBuildCommit</key>\n\t<string>a&amp;b</string>\n\t\
                     <key>FastabCodeDigest</key>\n\t<string>ff</string>\n</dict>";
        assert_eq!(
            super::plist_string(plist, "CFBundleShortVersionString").as_deref(),
            Some("0.0.19")
        );
        assert_eq!(super::plist_string(plist, "FastabBuildCommit").as_deref(), Some("a&b"));
        assert_eq!(super::plist_string(plist, "FastabBuildRunId"), None);
    }

    #[test]
    fn identity_is_valid_json_and_describes_this_process() {
        let value: serde_json::Value = serde_json::from_str(&super::json("fixture\"\\\n")).unwrap();
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(value["component"], "fixture\"\\\n");
        assert!(value["started_at_unix_ms"].as_u64().unwrap() > 0);
        #[cfg(target_os = "macos")]
        assert_eq!(value["mapped_image_uuid"].as_str().unwrap().len(), 32);
    }
}
