//! `find_image`: the image an entry of the feed counts, by the id the feed gives the entry.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use super::image::Image;
use super::paging::{claude_chain, omp_chain, page};
use super::*;
use crate::transcript::READ_CHUNK;

// Fixtures.

fn claude_image(media_type: &str, data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
}

fn omp_image(media_type: &str, data: &str) -> Value {
    json!({"type": "image", "data": data, "mimeType": media_type})
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// A Claude Code prompt whose content is a text block and then `blocks`.
fn claude_prompt(id: &str, blocks: Vec<Value>) -> String {
    let mut content = vec![text_block("Look at this")];
    content.extend(blocks);
    json!({
        "type": "user",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "message": {"role": "user", "content": content},
    })
    .to_string()
}

fn omp_message(id: &str, role: &str, blocks: Vec<Value>) -> String {
    let mut content = vec![text_block("Look at this")];
    content.extend(blocks);
    omp_entry(id, json!({"role": role, "content": content}))
}

fn omp_tool_result(id: &str, call: &str, blocks: Vec<Value>) -> String {
    omp_entry(
        id,
        json!({"role": "toolResult", "toolCallId": call, "content": blocks, "isError": false}),
    )
}

/// The image of an entry of an in-memory transcript, read `chunk` bytes at a time.
fn find_chunked(
    format: TranscriptFormat,
    lines: &[String],
    id: &str,
    index: u32,
    chunk: usize,
) -> ImageLookup {
    image::find_in(format, lines_of(&transcript(lines), chunk), id, index, None).unwrap()
}

fn find(format: TranscriptFormat, lines: &[String], id: &str, index: u32) -> ImageLookup {
    find_chunked(format, lines, id, index, READ_CHUNK)
}

fn found(lookup: ImageLookup) -> Image {
    match lookup {
        ImageLookup::Found(image) => image,
        other => panic!("expected an image, found {other:?}"),
    }
}

/// A scratch directory shaped like omp's: `agent/sessions/<project>/<session>.jsonl` and
/// `agent/blobs/`.
struct OmpDir(PathBuf);

impl OmpDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("herdr-image-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("agent/sessions/project")).unwrap();
        std::fs::create_dir_all(dir.join("agent/blobs")).unwrap();
        Self(dir)
    }

    fn agent(&self) -> PathBuf {
        self.0.join("agent")
    }

    fn session(&self, lines: &[String]) -> PathBuf {
        let path = self.agent().join("sessions/project/session.jsonl");
        std::fs::write(&path, transcript(lines)).unwrap();
        path
    }

    fn blob(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.agent().join("blobs").join(name), bytes).unwrap();
    }
}

impl Drop for OmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn blob_ref(hash: &str) -> String {
    format!("blob:sha256:{hash}")
}

fn hash(digit: char) -> String {
    digit.to_string().repeat(64)
}

fn read_from(dir: &OmpDir, lines: &[String], id: &str, index: u32) -> ImageLookup {
    let path = dir.session(lines);
    find_image(OMP, Path::new(&path), id, index).unwrap()
}

// Claude Code.

#[test]
fn claude_prompt_gives_each_of_its_images_with_its_own_media_type() {
    let lines = [
        claude_prompt(
            "u1",
            vec![
                claude_image("image/jpeg", "/9j/4A=="),
                claude_image("image/png", "iVBORw0KGgo="),
            ],
        ),
        claude_text("a1", "m1", Some("end_turn"), "Two pictures."),
    ];

    let first = found(find(CLAUDE, &lines, "u1", 0));
    let second = found(find(CLAUDE, &lines, "u1", 1));

    assert_eq!(first.media_type, "image/jpeg");
    assert_eq!(first.data, "/9j/4A==");
    assert_eq!(first.byte_count, 4);
    assert_eq!(second.media_type, "image/png");
    assert_eq!(second.data, "iVBORw0KGgo=");
    assert_eq!(second.byte_count, 8);
    assert_eq!(find(CLAUDE, &lines, "u1", 2), ImageLookup::NotFound);
}

#[test]
fn claude_tool_result_image_is_found_by_the_call_it_answers() {
    // Two calls answered in one line, and a later prompt that merely mentions the call's id:
    // each id gets its own result's image.
    let both = json!({
        "type": "user",
        "uuid": "r1",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1",
                "content": [text_block("shot"), claude_image("image/png", "AAAA")]},
            {"type": "tool_result", "tool_use_id": "toolu_2",
                "content": [claude_image("image/jpeg", "BBBB"), claude_image("image/jpeg", "CCCC")]},
        ]},
    })
    .to_string();
    let lines = [
        claude_user("u1", "Take screenshots"),
        claude_call(
            "a1",
            "m1",
            "toolu_1",
            "Read",
            json!({"file_path": "/a.png"}),
        ),
        claude_call(
            "a2",
            "m1",
            "toolu_2",
            "Read",
            json!({"file_path": "/b.png"}),
        ),
        both,
        claude_prompt("u2", vec![claude_image("image/png", "DDDD")])
            .replace("Look at this", "Compare with toolu_1"),
    ];

    let one = found(find(CLAUDE, &lines, "toolu_1", 0));
    let two = found(find(CLAUDE, &lines, "toolu_2", 1));

    assert_eq!(
        (one.media_type.as_str(), one.data.as_str()),
        ("image/png", "AAAA")
    );
    assert_eq!(
        (two.media_type.as_str(), two.data.as_str()),
        ("image/jpeg", "CCCC")
    );
    assert_eq!(find(CLAUDE, &lines, "toolu_1", 1), ImageLookup::NotFound);
    assert_eq!(find(CLAUDE, &lines, "u2", 1), ImageLookup::NotFound);
    assert_eq!(found(find(CLAUDE, &lines, "u2", 0)).data, "DDDD");
}

#[test]
fn claude_image_without_a_media_type_is_a_png() {
    let block = json!({"type": "image", "source": {"type": "base64", "data": "AAAA"}});
    let lines = [claude_prompt("u1", vec![block])];

    assert_eq!(found(find(CLAUDE, &lines, "u1", 0)).media_type, "image/png");
}

#[test]
fn claude_image_that_is_a_link_cannot_be_sent() {
    let block = json!({"type": "image", "source": {"type": "url", "url": "https://x.test/a.png"}});
    let lines = [claude_prompt("u1", vec![block])];

    assert_eq!(find(CLAUDE, &lines, "u1", 0), ImageLookup::Unsupported);
}

#[test]
fn claude_image_that_is_not_base64_is_unreadable_rather_than_passed_on() {
    let lines = [claude_prompt(
        "u1",
        vec![claude_image("image/png", "not base64!")],
    )];

    let error = image::find_in(
        CLAUDE,
        lines_of(&transcript(&lines), READ_CHUNK),
        "u1",
        0,
        None,
    )
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn claude_queued_prompt_has_its_images_under_its_own_id() {
    let queued = json!({
        "type": "attachment",
        "uuid": "q1",
        "attachment": {
            "type": "queued_command",
            "origin": {"kind": "human"},
            "prompt": [text_block("Also this"), claude_image("image/png", "AAAA")],
        },
    })
    .to_string();
    let lines = [claude_user("u1", "Go"), queued];

    assert_eq!(found(find(CLAUDE, &lines, "q1", 0)).data, "AAAA");
}

// omp.

#[test]
fn omp_tool_result_image_with_inline_data_is_found_by_its_call() {
    let lines = [
        omp_message("u1", "user", vec![]),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call(
                "call_1",
                "browser",
                json!({"action": "screenshot"}),
            )],
        ),
        omp_tool_result("r1", "call_1", vec![omp_image("image/webp", "AAAA")]),
    ];

    let image = found(find(OMP, &lines, "call_1", 0));

    assert_eq!(image.media_type, "image/webp");
    assert_eq!(image.data, "AAAA");
    assert_eq!(image.byte_count, 3);
    // The call before its result, and the result's own entry id, are not images.
    assert_eq!(find(OMP, &lines, "a1", 0), ImageLookup::NotFound);
    assert_eq!(find(OMP, &lines, "r1", 0), ImageLookup::NotFound);
}

#[test]
fn omp_image_without_a_mime_type_is_a_png() {
    let block = json!({"type": "image", "data": "AAAA"});
    let lines = [omp_message("u1", "user", vec![block])];

    assert_eq!(found(find(OMP, &lines, "u1", 0)).media_type, "image/png");
}

#[test]
fn omp_blob_reference_is_read_from_the_blobs_next_to_the_sessions() {
    let dir = OmpDir::new("blob");
    let bytes = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
    dir.blob(&hash('a'), &bytes);
    let lines = [
        omp_message("u1", "user", vec![]),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_1", "browser", json!({}))],
        ),
        omp_tool_result(
            "r1",
            "call_1",
            vec![omp_image("image/jpeg", &blob_ref(&hash('a')))],
        ),
    ];

    let image = found(read_from(&dir, &lines, "call_1", 0));

    assert_eq!(image.media_type, "image/jpeg");
    assert_eq!(image.byte_count, bytes.len() as u64);
    assert_eq!(STANDARD.decode(&image.data).unwrap(), bytes);
}

#[test]
fn omp_blob_reference_to_a_missing_file_is_not_found() {
    let dir = OmpDir::new("missing");
    let lines = [omp_message(
        "u1",
        "user",
        vec![omp_image("image/png", &blob_ref(&hash('b')))],
    )];

    assert_eq!(read_from(&dir, &lines, "u1", 0), ImageLookup::NotFound);
}

#[test]
fn omp_blob_reference_that_is_not_a_lowercase_sha256_never_reaches_the_filesystem() {
    let dir = OmpDir::new("traversal");
    // Files a traversing or sloppy lookup would reach.
    std::fs::write(dir.agent().join("secret"), b"secret").unwrap();
    dir.blob(&"A".repeat(64), b"upper");
    dir.blob(&"c".repeat(63), b"short");
    dir.blob("not-a-hash", b"named");
    let upper = "A".repeat(64);
    let short = "c".repeat(63);
    let long = "c".repeat(65);
    let absolute = dir.agent().join("secret").display().to_string();
    for reference in [
        "../secret",
        "../../agent/secret",
        upper.as_str(),
        short.as_str(),
        long.as_str(),
        "not-a-hash",
        absolute.as_str(),
        "",
    ] {
        let lines = [omp_message(
            "u1",
            "user",
            vec![omp_image("image/png", &blob_ref(reference))],
        )];

        assert_eq!(
            read_from(&dir, &lines, "u1", 0),
            ImageLookup::NotFound,
            "reference {reference:?}"
        );
    }
}

#[test]
fn omp_blob_reference_without_an_agent_directory_is_not_found() {
    let lines = [omp_message(
        "u1",
        "user",
        vec![omp_image("image/png", &blob_ref(&hash('d')))],
    )];

    assert_eq!(find(OMP, &lines, "u1", 0), ImageLookup::NotFound);
}

#[test]
fn omp_image_without_data_cannot_be_sent() {
    let lines = [omp_message(
        "u1",
        "user",
        vec![json!({"type": "image", "mimeType": "image/png"})],
    )];

    assert_eq!(find(OMP, &lines, "u1", 0), ImageLookup::Unsupported);
}

#[test]
fn omp_skill_prompt_images_are_found_under_the_custom_messages_id() {
    let skill = json!({
        "type": "custom_message",
        "id": "s1",
        "customType": "skill-prompt",
        "attribution": "user",
        "display": true,
        "content": [text_block("EXPANDED"), omp_image("image/png", "AAAA")],
        "details": {"prompt": "/skill:look"},
    })
    .to_string();

    assert_eq!(found(find(OMP, &[skill], "s1", 0)).data, "AAAA");
}

#[test]
fn id_written_with_unicode_escapes_is_still_found() {
    // Writers may escape what JSON does not require, so the line does not hold the id as
    // given: it holds `\u00e9`.
    let line = omp_message("u1", "user", vec![omp_image("image/png", "AAAA")])
        .replace(r#""id":"u1""#, r#""id":"\u00e91""#);

    assert!(line.contains(r"\u00e91"));
    assert_eq!(found(find(OMP, &[line], "é1", 0)).data, "AAAA");
}

// Limits and misses.

#[test]
fn unknown_ids_and_indexes_past_the_last_image_are_not_found() {
    let claude = [
        claude_prompt("u1", vec![claude_image("image/png", "AAAA")]),
        claude_text("a1", "m1", Some("end_turn"), "Seen."),
    ];
    let omp = [omp_message(
        "u1",
        "user",
        vec![omp_image("image/png", "AAAA")],
    )];

    for (format, lines) in [(CLAUDE, &claude[..]), (OMP, &omp[..])] {
        assert_eq!(find(format, lines, "u1", 1), ImageLookup::NotFound);
        assert_eq!(find(format, lines, "u1", u32::MAX), ImageLookup::NotFound);
        assert_eq!(find(format, lines, "nope", 0), ImageLookup::NotFound);
        assert_eq!(find(format, &[], "u1", 0), ImageLookup::NotFound);
    }
}

#[test]
fn entry_without_images_is_not_found() {
    let lines = [claude_user("u1", "Just words")];

    assert_eq!(find(CLAUDE, &lines, "u1", 0), ImageLookup::NotFound);
}

#[test]
fn image_is_found_however_the_file_is_chunked() {
    let mut lines = vec![claude_prompt(
        "u1",
        vec![claude_image("image/png", &"AAAA".repeat(500))],
    )];
    for step in 0..30 {
        lines.push(claude_text(
            &format!("a{step}"),
            &format!("m{step}"),
            None,
            &"filler ".repeat(40),
        ));
    }

    for chunk in [7, 64, 1000, READ_CHUNK] {
        let image = found(find_chunked(CLAUDE, &lines, "u1", 0, chunk));
        assert_eq!(image.data.len(), 2000, "chunk {chunk}");
    }
}

#[test]
fn inline_image_over_ten_mebibytes_is_too_large_without_its_data() {
    // 14,100,000 base64 characters decode to 10,575,000 bytes.
    let big = "A".repeat(14_100_000);
    let lines = [claude_prompt("u1", vec![claude_image("image/png", &big)])];

    assert_eq!(
        find(CLAUDE, &lines, "u1", 0),
        ImageLookup::TooLarge {
            byte_count: 10_575_000
        }
    );
}

#[test]
fn blob_of_exactly_ten_mebibytes_is_sent_and_one_byte_more_is_too_large() {
    let dir = OmpDir::new("limit");
    let limit = 10 * 1024 * 1024;
    dir.blob(&hash('e'), &vec![7u8; limit]);
    dir.blob(&hash('f'), &vec![7u8; limit + 1]);
    let lines = |digit: char| {
        [omp_message(
            "u1",
            "user",
            vec![omp_image("image/png", &blob_ref(&hash(digit)))],
        )]
    };

    let at_limit = found(read_from(&dir, &lines('e'), "u1", 0));
    let over = read_from(&dir, &lines('f'), "u1", 0);

    assert_eq!(at_limit.byte_count, limit as u64);
    assert_eq!(
        over,
        ImageLookup::TooLarge {
            byte_count: limit as u64 + 1
        }
    );
}

// The feed and the lookup agree.

/// Every image the feed counts resolves, and the next index does not.
fn assert_every_counted_image_resolves(
    format: TranscriptFormat,
    lines: &[String],
    dir: Option<&OmpDir>,
) -> usize {
    let history = page(format, lines, None, 20);
    let live = read(format, lines);
    let counted: std::collections::BTreeMap<&str, u32> = history
        .turns
        .iter()
        .flat_map(|turn| &turn.entries)
        .chain(&live.entries)
        .filter_map(|entry| Some((entry.id.as_str(), entry.images?)))
        .collect();
    let mut resolved = 0;
    for (id, images) in counted {
        for index in 0..=images {
            let lookup = match dir {
                Some(dir) => read_from(dir, lines, id, index),
                None => find(format, lines, id, index),
            };
            if index < images {
                assert!(
                    matches!(lookup, ImageLookup::Found(_)),
                    "entry {id} image {index} of {images}: {lookup:?}"
                );
                resolved += 1;
            } else {
                assert_eq!(lookup, ImageLookup::NotFound, "entry {id} image {images}");
            }
        }
    }
    resolved
}

#[test]
fn every_image_a_claude_entry_counts_resolves() {
    let queued = json!({
        "type": "attachment",
        "uuid": "q1",
        "attachment": {
            "type": "queued_command",
            "origin": {"kind": "human"},
            "prompt": [text_block("Also this"), claude_image("image/png", "AAAA")],
        },
    })
    .to_string();
    let lines = claude_chain(
        None,
        &[
            claude_prompt(
                "u1",
                vec![
                    claude_image("image/png", "AAAA"),
                    claude_image("image/jpeg", "BBBB"),
                ],
            ),
            claude_call(
                "a1",
                "m1",
                "toolu_1",
                "Read",
                json!({"file_path": "/a.png"}),
            ),
            claude_result(
                "r1",
                "toolu_1",
                json!([claude_image("image/png", "CCCC"), text_block("shown")]),
                false,
            ),
            claude_call(
                "a2",
                "m2",
                "toolu_2",
                "Read",
                json!({"file_path": "/b.png"}),
            ),
            claude_call(
                "a3",
                "m2",
                "toolu_3",
                "Read",
                json!({"file_path": "/c.png"}),
            ),
            claude_result(
                "r2",
                "toolu_2",
                json!([
                    claude_image("image/png", "DDDD"),
                    claude_image("image/png", "EEEE")
                ]),
                false,
            ),
            claude_result(
                "r3",
                "toolu_3",
                json!([claude_image("image/png", "FFFF")]),
                false,
            ),
            queued,
            claude_text("a4", "m3", Some("end_turn"), "Done."),
            claude_user("u2", "Next"),
            claude_text("a5", "m4", Some("end_turn"), "Fine."),
        ],
    );

    let resolved = assert_every_counted_image_resolves(CLAUDE, &lines, None);

    assert_eq!(resolved, 7);
}

#[test]
fn every_image_an_omp_entry_counts_resolves() {
    let dir = OmpDir::new("invariant");
    dir.blob(&hash('1'), b"one");
    dir.blob(&hash('2'), b"two");
    let skill = json!({
        "type": "custom_message",
        "id": "s1",
        "customType": "skill-prompt",
        "attribution": "user",
        "display": true,
        "content": [text_block("EXPANDED"), omp_image("image/png", &blob_ref(&hash('1')))],
        "details": {"prompt": "/skill:look"},
    })
    .to_string();
    let steering = omp_entry(
        "u2",
        json!({"role": "user", "steering": true, "content": [
            text_block("And this"),
            omp_image("image/png", "AAAA"),
        ]}),
    );
    let lines = omp_chain(&[
        omp_message("d1", "developer", vec![omp_image("image/png", "AAAA")]),
        omp_message(
            "u1",
            "user",
            vec![
                omp_image("image/png", &blob_ref(&hash('2'))),
                omp_image("image/webp", "BBBB"),
            ],
        ),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call(
                "call_1",
                "browser",
                json!({"action": "screenshot"}),
            )],
        ),
        omp_tool_result(
            "r1",
            "call_1",
            vec![omp_image("image/png", &blob_ref(&hash('1')))],
        ),
        steering,
        omp_response("a2", "stop", vec![omp_text("Seen.")]),
        skill,
        omp_response("a3", "stop", vec![omp_text("Skilled.")]),
    ]);

    let resolved = assert_every_counted_image_resolves(OMP, &lines, Some(&dir));

    assert_eq!(resolved, 6);
}
