//! One image out of a transcript, for `agent.image`. The feed only counts images; a client that
//! shows one asks for it by the id of the entry that carries it, one image per call.
//!
//! The lookup reads the way the feed does, so an entry has the id the feed gave it: it parses
//! lines with the same readers, newest first, and matches a prompt's own id or the id of the
//! tool call a result answers. The lines that cannot hold the id are skipped without being
//! parsed, because a line with a screenshot in it is hundreds of kilobytes.
//!
//! Claude Code stores an image inline as base64. omp stores it inline in tests and small
//! sessions, and in real ones as a `blob:sha256:<hex>` reference to a file in the `blobs`
//! directory next to its `sessions` directory.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;

use super::{image_blocks, parse, parse_entry, Line};
use crate::transcript::{ReverseLines, TranscriptFormat, READ_CHUNK};

/// The largest image, decoded, that is sent.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

const BLOB_PREFIX: &str = "blob:sha256:";

/// An image of a transcript, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub media_type: String,
    /// The image's size in bytes, decoded.
    pub byte_count: u64,
    /// The image's bytes in standard base64.
    pub data: String,
}

/// What a lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageLookup {
    Found(Image),
    /// The image is larger than `MAX_IMAGE_BYTES`.
    TooLarge {
        byte_count: u64,
    },
    /// The transcript has no such entry, the entry has no such image, or the file the image
    /// is kept in is gone.
    NotFound,
    /// The transcript holds the image in a form Herdr cannot read, such as a link.
    Unsupported,
}

/// Finds the `index`-th image of the entry `entry_id` of the transcript at `path`.
pub fn find_image(
    format: TranscriptFormat,
    path: &Path,
    entry_id: &str,
    index: u32,
) -> io::Result<ImageLookup> {
    let lines = ReverseLines::new(File::open(path)?, READ_CHUNK)?;
    // `<agent dir>/sessions/<project>/<session>.jsonl`, and the blobs sit in the agent dir.
    let blobs = path.ancestors().nth(3).map(|dir| dir.join("blobs"));
    find_in(format, lines, entry_id, index, blobs.as_deref())
}

pub(super) fn find_in(
    format: TranscriptFormat,
    lines: impl Iterator<Item = io::Result<Vec<u8>>>,
    entry_id: &str,
    index: u32,
    blobs: Option<&Path>,
) -> io::Result<ImageLookup> {
    let needle = needle(entry_id);
    for line in lines {
        let line = line?;
        if needle
            .as_deref()
            .is_some_and(|needle| !contains(&line, needle))
        {
            continue;
        }
        let in_result = match parse(format, &line, false).line {
            Line::Prompt(prompt) | Line::Context(prompt) | Line::MidPrompt(prompt)
                if prompt.id == entry_id =>
            {
                false
            }
            Line::ToolResults(results) if results.iter().any(|result| result.call == entry_id) => {
                true
            }
            _ => continue,
        };
        let Some(value) = parse_entry::<Value>(&line) else {
            continue;
        };
        let content = content_of(format, &value, in_result.then_some(entry_id));
        let Some(block) = image_blocks(content).nth(index as usize) else {
            return Ok(ImageLookup::NotFound);
        };
        return resolve(format, block, blobs);
    }
    Ok(ImageLookup::NotFound)
}

/// What a line holding the id must contain, when it can be told cheaply: the id as JSON
/// writes it. An id with characters that writers escape in other ways has no such text.
fn needle(entry_id: &str) -> Option<Vec<u8>> {
    let plain = entry_id
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\');
    (plain && !entry_id.is_empty()).then(|| entry_id.as_bytes().to_vec())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The content array that holds the entry's images: a tool result's own content when
/// `result_of` names the call, otherwise the content of the prompt.
fn content_of<'a>(
    format: TranscriptFormat,
    line: &'a Value,
    result_of: Option<&str>,
) -> Option<&'a Value> {
    match format {
        TranscriptFormat::Claude => {
            if line.get("type").and_then(Value::as_str) == Some("attachment") {
                return line.get("attachment")?.get("prompt");
            }
            let content = line.get("message")?.get("content")?;
            let Some(call) = result_of else {
                return Some(content);
            };
            content.as_array()?.iter().find_map(|block| {
                let is_result = block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && block.get("tool_use_id").and_then(Value::as_str) == Some(call);
                if is_result {
                    block.get("content")
                } else {
                    None
                }
            })
        }
        TranscriptFormat::Omp => {
            if line.get("type").and_then(Value::as_str) == Some("custom_message") {
                return line.get("content");
            }
            line.get("message")?.get("content")
        }
    }
}

/// The image of a block, with the media type the transcript records for it.
fn resolve(
    format: TranscriptFormat,
    block: &Value,
    blobs: Option<&Path>,
) -> io::Result<ImageLookup> {
    let (media_type, data) = match format {
        TranscriptFormat::Claude => {
            let source = block.get("source");
            let inline = source.and_then(|source| source.get("type")?.as_str()) == Some("base64");
            let data = source.and_then(|source| source.get("data")?.as_str());
            match data {
                Some(data) if inline => (
                    source.and_then(|source| source.get("media_type")?.as_str()),
                    data,
                ),
                _ => return Ok(ImageLookup::Unsupported),
            }
        }
        TranscriptFormat::Omp => match block.get("data").and_then(Value::as_str) {
            Some(data) => (block.get("mimeType").and_then(Value::as_str), data),
            None => return Ok(ImageLookup::Unsupported),
        },
    };
    let media_type = media_type.unwrap_or("image/png").to_string();
    match data.strip_prefix(BLOB_PREFIX) {
        Some(hash) => blob(media_type, hash, blobs),
        None => inline(media_type, data),
    }
}

/// An image the transcript holds as base64.
fn inline(media_type: String, data: &str) -> io::Result<ImageLookup> {
    // Padding carries no bytes, and a final partial group of `n` characters holds `n * 3 / 4`.
    let byte_count = (data.trim_end_matches('=').len() as u64) * 3 / 4;
    if byte_count > MAX_IMAGE_BYTES {
        return Ok(ImageLookup::TooLarge { byte_count });
    }
    // Clients decode what is sent, so it is only sent when it is base64 they can decode.
    let bytes = STANDARD
        .decode(data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(ImageLookup::Found(Image {
        media_type,
        byte_count: bytes.len() as u64,
        data: data.to_string(),
    }))
}

/// An image omp keeps in a file, named by the SHA-256 of its bytes.
fn blob(media_type: String, hash: &str, blobs: Option<&Path>) -> io::Result<ImageLookup> {
    // Nothing but a hash may name a file: the reference comes from a file an agent wrote.
    let is_hash = hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let Some(blobs) = blobs.filter(|_| is_hash) else {
        return Ok(ImageLookup::NotFound);
    };
    let path: PathBuf = blobs.join(hash);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ImageLookup::NotFound),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(ImageLookup::NotFound);
    }
    let byte_count = metadata.len();
    if byte_count > MAX_IMAGE_BYTES {
        return Ok(ImageLookup::TooLarge { byte_count });
    }
    // One byte more than the limit shows a file that grew since it was measured.
    let mut bytes = Vec::with_capacity(byte_count as usize);
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Ok(ImageLookup::TooLarge {
            byte_count: bytes.len() as u64,
        });
    }
    Ok(ImageLookup::Found(Image {
        media_type,
        byte_count: bytes.len() as u64,
        data: STANDARD.encode(&bytes),
    }))
}
