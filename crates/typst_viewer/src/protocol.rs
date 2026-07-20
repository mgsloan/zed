//! Wire codec for the tinymist page-image preview protocol.
//!
//! The protocol is specified in `design.md` alongside this crate. Frames are
//! **binary**: an ASCII header up to the first `\n`, then a payload. Parsing
//! therefore works over `&[u8]` and must not go through `str`, since a `raw`
//! payload is arbitrary bytes.
//!
//! Everything here is a free function over borrowed bytes so the protocol is
//! testable without a socket, an LSP, or a GPUI app.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use std::fmt;
use std::ops::Range;
use std::str::FromStr;

/// WebSocket subprotocol that selects this protocol at connect.
///
/// The server echoes it when the mode is available and refuses the upgrade
/// otherwise, so a refused upgrade means "this tinymist is too old" rather than
/// "the connection failed".
pub const SUBPROTOCOL: &str = "tinymist-page-image-v1";

/// Largest frame we will accept, matching the server's own raster budget.
///
/// The header is the only description of the payload, so an implausible length
/// must be rejected before it becomes an allocation.
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// Identifies a page by the inputs that determine its rendering.
///
/// Opaque and fixed-width: the value only ever has to round-trip and compare,
/// and a collision would draw the *wrong page* with no detectable error, so it
/// is stored whole rather than narrowed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ContentId(u128);

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl FromStr for ContentId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("content id must be 32 hex digits, got {s:?}");
        }
        Ok(Self(u128::from_str_radix(s, 16)?))
    }
}

impl<'de> Deserialize<'de> for ContentId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// How page images are encoded on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    /// The renderer's native buffer: RGBA8, premultiplied, tightly packed.
    Raw,
    /// PNG, for links where raw images are not viable.
    Png,
}

impl Encoding {
    fn parse(text: &str) -> Result<Self> {
        match text {
            "raw" => Ok(Self::Raw),
            "png" => Ok(Self::Png),
            other => bail!("unknown encoding {other:?}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Png => "png",
        }
    }
}

/// One row of a `pages` delta.
///
/// The `Option`s are load-bearing: `None` means *unchanged*, not *absent*.
/// Collapsing them to defaults would clear geometry on every keystroke.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TableEntryDelta {
    #[serde(rename = "i")]
    pub index: usize,
    #[serde(rename = "w", default)]
    pub width: Option<f32>,
    #[serde(rename = "h", default)]
    pub height: Option<f32>,
    #[serde(rename = "c", default)]
    pub content: Option<ContentId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct PagesBody {
    total: usize,
    #[serde(default)]
    full: bool,
    #[serde(default)]
    pages: Vec<TableEntryDelta>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct ErrorBody {
    #[serde(rename = "c")]
    content: ContentId,
    #[serde(default)]
    msg: String,
}

/// A frame from the server, with the payload left as a range into the buffer it
/// was parsed from so the header parse never copies megabytes.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Pages {
        total: usize,
        full: bool,
        entries: Vec<TableEntryDelta>,
    },
    Image {
        content: ContentId,
        px_width: u32,
        px_height: u32,
        scale: f32,
        encoding: Encoding,
        payload: Range<usize>,
    },
    Error {
        content: ContentId,
        msg: String,
    },
}

/// Parses one frame.
///
/// Returns `Ok(None)` for a frame this client does not recognize, which is not
/// an error: the server may add message kinds, and an unknown one is ignorable
/// by construction since every kind is self-contained.
pub fn parse_frame(frame: &[u8]) -> Result<Option<ServerMessage>> {
    if frame.len() > MAX_FRAME_BYTES {
        bail!("frame of {} bytes exceeds the cap", frame.len());
    }
    let split = frame
        .iter()
        .position(|&b| b == b'\n')
        .context("frame has no header terminator")?;
    let header = std::str::from_utf8(&frame[..split]).context("frame header is not UTF-8")?;
    let body = split + 1;

    if let Some(rest) = header.strip_prefix("image:") {
        return parse_image_header(rest, body, frame.len()).map(Some);
    }
    match header {
        "pages" => {
            let parsed: PagesBody =
                serde_json::from_slice(&frame[body..]).context("parsing pages body")?;
            Ok(Some(ServerMessage::Pages {
                total: parsed.total,
                full: parsed.full,
                entries: parsed.pages,
            }))
        }
        "error" => {
            let parsed: ErrorBody =
                serde_json::from_slice(&frame[body..]).context("parsing error body")?;
            Ok(Some(ServerMessage::Error {
                content: parsed.content,
                msg: parsed.msg,
            }))
        }
        _ => Ok(None),
    }
}

fn parse_image_header(rest: &str, body: usize, frame_len: usize) -> Result<ServerMessage> {
    let mut fields = rest.split(':');
    let mut next = |what: &'static str| -> Result<&str> {
        fields
            .next()
            .ok_or_else(|| anyhow!("image header missing {what}"))
    };

    let content: ContentId = next("content id")?.parse()?;
    let px_width: u32 = next("width")?.parse().context("image width")?;
    let px_height: u32 = next("height")?.parse().context("image height")?;
    // Parsed rather than assumed: an image requested before a zoom can arrive
    // after it, and assuming the current scale would draw it at the wrong
    // *size* rather than merely the wrong sharpness.
    let scale: f32 = next("scale")?.parse().context("image scale")?;
    let encoding = Encoding::parse(next("encoding")?)?;

    let payload = body..frame_len;

    // The header is the only description of the buffer's shape, so a mismatch
    // here would become an out-of-bounds read or a garbled image downstream.
    if encoding == Encoding::Raw {
        let expected = (px_width as usize)
            .checked_mul(px_height as usize)
            .and_then(|px| px.checked_mul(4))
            .context("image dimensions overflow")?;
        if payload.len() != expected {
            bail!(
                "raw payload is {} bytes, but {px_width}x{px_height} needs {expected}",
                payload.len(),
            );
        }
    }

    Ok(ServerMessage::Image {
        content,
        px_width,
        px_height,
        scale,
        encoding,
        payload,
    })
}

/// A message to the server.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    /// The standing subscription. Wholly replaces its predecessor.
    View {
        visible: Vec<usize>,
        prefetch: Vec<usize>,
        cached: Vec<usize>,
        scale: f32,
        encoding: Encoding,
    },
    /// Re-send these page indices regardless of what the server believes we
    /// hold. Defined for robustness; this client honors its retention contract
    /// and does not send it.
    #[allow(dead_code)]
    WantPages(Vec<usize>),
}

impl ClientMessage {
    pub fn encode(&self) -> String {
        match self {
            Self::View {
                visible,
                prefetch,
                cached,
                scale,
                encoding,
            } => {
                let body = serde_json::json!({
                    "visible": visible,
                    "prefetch": prefetch,
                    "cached": cached,
                    "scale": scale,
                    "encoding": encoding.as_str(),
                    // Ask the server to composite onto white. The bare channel
                    // swap in `decode` is only correct because of this.
                    "opaque": true,
                });
                format!("view\n{body}")
            }
            Self::WantPages(indices) => {
                let body = serde_json::json!({ "i": indices });
                format!("want-page\n{body}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn image_frame(header: &str, payload: &[u8]) -> Vec<u8> {
        let mut frame = header.as_bytes().to_vec();
        frame.push(b'\n');
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn parses_a_raw_image_header() {
        let payload = vec![0u8; 3 * 5 * 4];
        let frame = image_frame(&format!("image:{ID}:3:5:2:raw"), &payload);

        let ServerMessage::Image {
            content,
            px_width,
            px_height,
            scale,
            encoding,
            payload: range,
        } = parse_frame(&frame).unwrap().unwrap()
        else {
            panic!("expected an image");
        };
        assert_eq!(content.to_string(), ID);
        assert_eq!((px_width, px_height), (3, 5));
        assert_eq!(scale, 2.0);
        assert_eq!(encoding, Encoding::Raw);
        assert_eq!(range.len(), payload.len());
    }

    #[test]
    fn rejects_a_raw_payload_that_contradicts_the_header() {
        // Would be an out-of-bounds read or a garbled image downstream.
        let frame = image_frame(&format!("image:{ID}:3:5:2:raw"), &[0u8; 8]);
        assert!(parse_frame(&frame).is_err());
    }

    #[test]
    fn believes_the_header_scale_not_the_current_one() {
        // The in-flight case: this arrives after the client has moved to 2x.
        let frame = image_frame(&format!("image:{ID}:1:1:1:raw"), &[0u8; 4]);
        let ServerMessage::Image { scale, .. } = parse_frame(&frame).unwrap().unwrap() else {
            panic!("expected an image");
        };
        assert_eq!(scale, 1.0, "the header is authoritative");
    }

    #[test]
    fn parses_a_full_page_table() {
        let frame = br#"pages
{"total":2,"full":true,"pages":[{"i":0,"w":595.0,"h":842.0,"c":"0123456789abcdef0123456789abcdef"}]}"#;
        let ServerMessage::Pages {
            total,
            full,
            entries,
        } = parse_frame(frame).unwrap().unwrap()
        else {
            panic!("expected pages");
        };
        assert_eq!(total, 2);
        assert!(full);
        assert_eq!(entries[0].width, Some(595.0));
        assert_eq!(entries[0].content.unwrap().to_string(), ID);
    }

    #[test]
    fn absent_fields_stay_none_rather_than_defaulting() {
        // `None` means unchanged. Defaulting would clear geometry on every
        // keystroke, since a content-only delta carries no `w`/`h`.
        let frame = br#"pages
{"total":1,"pages":[{"i":0,"c":"0123456789abcdef0123456789abcdef"}]}"#;
        let ServerMessage::Pages { full, entries, .. } = parse_frame(frame).unwrap().unwrap() else {
            panic!("expected pages");
        };
        assert!(!full);
        assert_eq!(entries[0].width, None);
        assert_eq!(entries[0].height, None);
        assert!(entries[0].content.is_some());
    }

    #[test]
    fn parses_an_error_frame() {
        let frame = br#"error
{"c":"0123456789abcdef0123456789abcdef","msg":"too big"}"#;
        let ServerMessage::Error { content, msg } = parse_frame(frame).unwrap().unwrap() else {
            panic!("expected an error");
        };
        assert_eq!(content.to_string(), ID);
        assert_eq!(msg, "too big");
    }

    #[test]
    fn unknown_frames_are_ignored_not_fatal() {
        assert_eq!(parse_frame(b"something-new\n{}").unwrap(), None);
    }

    #[test]
    fn rejects_a_frame_with_no_header() {
        assert!(parse_frame(b"no newline here").is_err());
    }

    #[test]
    fn content_id_round_trips_and_rejects_junk() {
        assert_eq!(ID.parse::<ContentId>().unwrap().to_string(), ID);
        assert!("abc".parse::<ContentId>().is_err());
        // `from_str_radix` would otherwise accept a sign.
        assert!("+0000000000000000000000000000001".parse::<ContentId>().is_err());
    }

    #[test]
    fn view_encodes_with_the_opacity_request() {
        let encoded = ClientMessage::View {
            visible: vec![1, 2],
            prefetch: vec![3],
            cached: vec![],
            scale: 2.0,
            encoding: Encoding::Raw,
        }
        .encode();
        let (head, body) = encoded.split_once('\n').unwrap();
        assert_eq!(head, "view");
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["visible"], serde_json::json!([1, 2]));
        assert_eq!(json["scale"], 2.0);
        assert_eq!(json["encoding"], "raw");
        assert_eq!(json["opaque"], true, "the decode path depends on this");
    }

    #[test]
    fn want_pages_addresses_by_index() {
        let encoded = ClientMessage::WantPages(vec![7, 8]).encode();
        assert_eq!(encoded, "want-page\n{\"i\":[7,8]}");
    }
}
