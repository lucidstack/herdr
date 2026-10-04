//! Atlassian Document Format, the JSON Jira keeps descriptions and comments in, as
//! GitHub-flavoured markdown. Headings, lists, code, quotes, tables and links survive for a
//! client to render and for an agent's brief to carry, and text that would otherwise read as
//! markdown is escaped where it stands, and only there.

use std::borrow::Cow;

use serde_json::Value;

/// Columns of a table in which a cell's spans are followed. A cell past them still comes through,
/// but what it spans is taken as a mistake: the empty cells that keep columns in line cost a
/// row of the output each, so a document could otherwise make its markdown many times its size.
const MAX_COLUMNS: usize = 32;

/// Markdown reads a list item's number from at most nine digits.
const MAX_LIST_NUMBER: u64 = 999_999_999;

/// `node`, usually a `doc`, as markdown: its blocks separated by blank lines.
pub(super) fn markdown(node: &Value) -> String {
    block(node)
}

fn kind(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

fn children(node: &Value) -> &[Value] {
    match node.get("content") {
        Some(Value::Array(nodes)) => nodes,
        _ => &[],
    }
}

fn attr<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
    node.get("attrs")?.get(name)?.as_str()
}

fn number_attr(node: &Value, name: &str) -> Option<u64> {
    node.get("attrs")?.get(name)?.as_u64()
}

fn is_inline(node: &Value) -> bool {
    matches!(
        kind(node),
        "text"
            | "hardBreak"
            | "mention"
            | "emoji"
            | "inlineCard"
            | "date"
            | "status"
            | "mediaInline"
            | "placeholder"
            | "inlineExtension"
    )
}

// MARK: Blocks

fn block(node: &Value) -> String {
    match kind(node) {
        "doc" => blocks(children(node)),
        "paragraph" | "caption" => paragraph(children(node)),
        "heading" => heading(node),
        "bulletList" => list(node, None),
        "orderedList" => list(node, Some(order(node))),
        "taskList" => task_list(node),
        "decisionList" => decision_list(node),
        "codeBlock" => code_block(node),
        // A panel's colour has no markdown; it reads as set apart, like a quote.
        "blockquote" | "panel" => quote(&blocks(children(node))),
        "rule" => "---".into(),
        "table" => table(node),
        "mediaSingle" => media_blocks(node, "Image"),
        "mediaGroup" | "media" => media_blocks(node, "Attachment"),
        "expand" | "nestedExpand" => expand(node),
        "blockCard" | "embedCard" => {
            let mut writer = Writer::new(Mode::Block);
            writer.card(node);
            writer.finish()
        }
        // Inline nodes where a block belongs, from a document that strays from the format.
        _ if is_inline(node) => paragraph(std::slice::from_ref(node)),
        _ => other(node),
    }
}

/// Block nodes, a blank line between each and the next; empty ones leave no gap.
fn blocks(nodes: &[Value]) -> String {
    joined_blocks(nodes, |_| "\n\n")
}

fn joined_blocks(nodes: &[Value], separator: impl Fn(&Value) -> &'static str) -> String {
    let mut out = String::new();
    for node in nodes {
        let rendered = block(node);
        if rendered.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(separator(node));
        }
        out.push_str(&rendered);
    }
    out
}

/// Nodes added to the format after this was written: their blocks, or their words, so that
/// nothing goes missing.
fn other(node: &Value) -> String {
    let nodes = children(node);
    if nodes.iter().any(is_inline) {
        paragraph(nodes)
    } else if !nodes.is_empty() {
        blocks(nodes)
    } else {
        attr(node, "text").map(paragraph_text).unwrap_or_default()
    }
}

fn paragraph(nodes: &[Value]) -> String {
    let mut writer = Writer::new(Mode::Block);
    writer.inline(nodes);
    writer.finish()
}

fn paragraph_text(text: &str) -> String {
    let mut writer = Writer::new(Mode::Block);
    writer.text(text, &[], false);
    writer.finish()
}

fn heading(node: &Value) -> String {
    let level = number_attr(node, "level").unwrap_or(1).clamp(1, 6);
    let mut writer = Writer::new(Mode::Heading);
    writer.inline(children(node));
    let mut text = writer.finish();
    if text.is_empty() {
        return text;
    }
    // A closing run of #s after a space would be read as the heading's end, not its words.
    let words = text.trim_end_matches('#').len();
    if words < text.len() && (words == 0 || text[..words].ends_with([' ', '\t'])) {
        text.insert(words, '\\');
    }
    format!("{} {text}", "#".repeat(level as usize))
}

/// The number an ordered list starts at: low enough for its last item's number to be read.
fn order(node: &Value) -> u64 {
    let last = children(node).len().saturating_sub(1) as u64;
    number_attr(node, "order")
        .unwrap_or(1)
        .min(MAX_LIST_NUMBER.saturating_sub(last))
}

/// A bullet list, or an ordered one counting from `start`.
fn list(node: &Value, start: Option<u64>) -> String {
    let mut items = Vec::new();
    let mut number = start.unwrap_or(1);
    for item in children(node) {
        let marker = match start {
            Some(_) => format!("{number}. "),
            None => "- ".into(),
        };
        items.push(list_item(&marker, &item_blocks(children(item))));
        number = number.saturating_add(1);
    }
    items.join("\n")
}

/// A list item's blocks. A nested list or a code block follows the words it belongs to on
/// the next line, which keeps the list tight; paragraphs need a blank line between them.
fn item_blocks(nodes: &[Value]) -> String {
    joined_blocks(nodes, |node| match kind(node) {
        "bulletList" | "taskList" | "decisionList" | "codeBlock" => "\n",
        // Only a list counting from 1 may follow words without a blank line.
        "orderedList" if order(node) == 1 => "\n",
        _ => "\n\n",
    })
}

/// `body` under `marker`, its later lines indented to line up with the first one's words so
/// that they stay inside the item. A body that opens with a fence or another list starts on
/// the line after the marker, where it keeps its meaning: beside the marker it would read as
/// the item's words.
fn list_item(marker: &str, body: &str) -> String {
    if body.is_empty() {
        return marker.trim_end().to_string();
    }
    if opens_a_block(body) {
        return format!(
            "{}\n{}",
            marker.trim_end(),
            indented(body, marker.len(), false)
        );
    }
    let mut out = String::from(marker);
    out.push_str(&indented(body, marker.len(), true));
    out
}

/// Whether `body` starts with a fence or a list marker. Words that start like that are
/// escaped, so these are blocks.
fn opens_a_block(body: &str) -> bool {
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    body.starts_with("```")
        || body.starts_with("- ")
        || (digits > 0 && body[digits..].starts_with(". "))
}

/// Each line of `text` after `by` spaces, the first one too unless `skip_first`; blank lines
/// stay blank.
fn indented(text: &str, by: usize, skip_first: bool) -> String {
    let indent = " ".repeat(by);
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if !line.is_empty() && (index > 0 || !skip_first) {
            out.push_str(&indent);
        }
        out.push_str(line);
    }
    out
}

/// GFM task items, ticked when done. A task list nested in another sits inside the item
/// before it.
fn task_list(node: &Value) -> String {
    let mut out = String::new();
    for child in children(node) {
        let rendered = match kind(child) {
            "taskItem" => {
                let mut writer = Writer::new(Mode::Block);
                // Words straight after the box can't start a list or a heading.
                writer.escape_line_start = false;
                writer.inline(children(child));
                task_item(child, &writer.finish())
            }
            "blockTaskItem" => task_item(child, &item_blocks(children(child))),
            "taskList" => {
                let nested = task_list(child);
                if out.is_empty() {
                    nested
                } else {
                    indented(&nested, 2, false)
                }
            }
            _ => block(child),
        };
        if rendered.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&rendered);
    }
    out
}

/// `body` behind a box, ticked when the task is done.
fn task_item(node: &Value, body: &str) -> String {
    let ticked = if attr(node, "state") == Some("DONE") {
        "x"
    } else {
        " "
    };
    list_item("- ", format!("[{ticked}] {body}").trim_end())
}

/// Decisions as a bullet list.
fn decision_list(node: &Value) -> String {
    let items: Vec<String> = children(node)
        .iter()
        .map(|item| list_item("- ", &paragraph(children(item))))
        .collect();
    items.join("\n")
}

/// The words of a code block, its lines ended by `\n` alone.
fn code_text(node: &Value) -> String {
    let code: String = children(node)
        .iter()
        .filter_map(|child| child.get("text").and_then(Value::as_str))
        .collect();
    with_line_feeds(&code).into_owned()
}

/// `text` with each `\r\n` and lone `\r` as `\n`: markdown ends a line at any of them.
fn with_line_feeds(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Fenced with more backticks than any run inside it, and its language.
fn code_block(node: &Value) -> String {
    let code = code_text(node);
    let code = code.trim_end_matches('\n');
    if code.trim().is_empty() {
        return String::new();
    }
    let fence = "`".repeat(longest_run(code, '`').max(2) + 1);
    let language = attr(node, "language")
        .and_then(|language| language.split_whitespace().next())
        .unwrap_or("")
        .replace('`', "");
    format!("{fence}{language}\n{code}\n{fence}")
}

fn longest_run(text: &str, character: char) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for current in text.chars() {
        run = if current == character { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

/// Every line of `inner` quoted, blank ones too, so the quote holds together.
fn quote(inner: &str) -> String {
    if inner.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = inner
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect();
    lines.join("\n")
}

/// A collapsed section: its title in bold, then what it holds, open.
fn expand(node: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(title) = attr(node, "title").filter(|title| !title.trim().is_empty()) {
        let mut writer = Writer::new(Mode::Block);
        writer.text(title, &[Mark::Strong], false);
        parts.push(writer.finish());
    }
    let body = blocks(children(node));
    if !body.is_empty() {
        parts.push(body);
    }
    parts.join("\n\n")
}

/// A GFM table, its first row the header: GFM has no table without one, and in Jira the first
/// row is a header unless it was switched off. A cell is one line. One spanning columns is
/// followed by empty cells, and one spanning rows leaves an empty cell in the rows under it, so
/// that the columns stay in line. Only the header and delimiter rows are as wide as the widest
/// row: markdown fills a shorter row itself.
fn table(node: &Value) -> String {
    let row_nodes = children(node);
    // In each of the first `MAX_COLUMNS` columns, how many more rows a cell above takes.
    let mut taken: Vec<u64> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(row_nodes.len());
    for (index, row) in row_nodes.iter().enumerate() {
        let rows_below = (row_nodes.len() - index - 1) as u64;
        let mut cells = Vec::new();
        for cell in children(row) {
            while taken.get(cells.len()).is_some_and(|&rows| rows > 0) {
                cells.push(String::new());
            }
            let column = cells.len();
            let room = MAX_COLUMNS.saturating_sub(column).max(1);
            let across = number_attr(cell, "colspan")
                .unwrap_or(1)
                .clamp(1, room as u64) as usize;
            let down = number_attr(cell, "rowspan")
                .unwrap_or(1)
                .clamp(1, rows_below + 1);
            cells.push(cell_text(cell));
            cells.resize(column + across, String::new());
            // A cell past the last column followed spans nothing, and takes no row below it.
            if column < MAX_COLUMNS {
                if taken.len() < column + across {
                    taken.resize(column + across, 0);
                }
                taken[column..column + across].fill(down);
            }
        }
        for rows in &mut taken {
            *rows = rows.saturating_sub(1);
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    let Some(columns) = rows.iter().map(Vec::len).max() else {
        return String::new();
    };
    let line = |cells: &[String], width: usize| {
        let mut line = String::from("|");
        for column in 0..width {
            line.push(' ');
            line.push_str(cells.get(column).map_or("", String::as_str));
            line.push_str(" |");
        }
        line
    };
    let mut lines = vec![
        line(&rows[0], columns),
        format!("|{}", " --- |".repeat(columns)),
    ];
    lines.extend(rows[1..].iter().map(|row| line(row, row.len())));
    lines.join("\n")
}

fn cell_text(cell: &Value) -> String {
    let mut writer = Writer::new(Mode::Cell);
    writer.flatten(children(cell));
    writer.finish()
}

/// Images and files. Ones Jira keeps as attachments only Jira's credentials can fetch, so
/// each becomes a line naming it, `label` saying what it is; an image from the web shows.
fn media_blocks(node: &Value, label: &str) -> String {
    if kind(node) == "media" {
        return media(node, label);
    }
    let parts: Vec<String> = children(node)
        .iter()
        .map(|child| match kind(child) {
            "media" => media(child, label),
            _ => block(child),
        })
        .filter(|part| !part.is_empty())
        .collect();
    parts.join("\n\n")
}

fn media(node: &Value, label: &str) -> String {
    let alt = attr(node, "alt")
        .map(str::trim)
        .filter(|alt| !alt.is_empty());
    if attr(node, "type") == Some("external") {
        if let Some(url) = attr(node, "url")
            .map(str::trim)
            .filter(|url| is_web_url(url))
        {
            let alt: String = alt
                .unwrap_or("")
                .chars()
                .filter(|character| !matches!(character, '[' | ']' | '\\' | '\n' | '\r'))
                .collect();
            return format!("![{alt}]({})", destination(url));
        }
    }
    paragraph_text_marked(&media_label(label, alt), &[Mark::Em])
}

fn media_label(label: &str, alt: Option<&str>) -> String {
    match alt {
        Some(alt) => format!("{label}: {alt}"),
        None => label.to_string(),
    }
}

fn paragraph_text_marked(text: &str, marks: &[Mark]) -> String {
    let mut writer = Writer::new(Mode::Block);
    writer.text(text, marks, false);
    writer.finish()
}

fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// A link's address as markdown takes it: spaces, brackets and backslashes encoded.
fn destination(href: &str) -> String {
    let mut out = String::with_capacity(href.len());
    for character in href.trim().chars() {
        match character {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\\' => out.push_str("%5C"),
            character if character.is_control() => {}
            character => out.push(character),
        }
    }
    out
}

/// What a card's link reads: a Jira issue's key, as Jira shows its links to issues, or the
/// address.
fn card_text(url: &str) -> &str {
    url.split_once("/browse/")
        .and_then(|(_, rest)| rest.split(['?', '#', '/']).next())
        .filter(|key| is_issue_key(key))
        .unwrap_or(url)
}

/// `APP-12`: a project key (an uppercase letter, then uppercase letters, digits and
/// underscores), a dash and a number.
fn is_issue_key(key: &str) -> bool {
    let Some((project, number)) = key.split_once('-') else {
        return false;
    };
    project.starts_with(|character: char| character.is_ascii_uppercase())
        && project.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
        && !number.is_empty()
        && number.chars().all(|character| character.is_ascii_digit())
}

/// A date node's day, as YYYY-MM-DD.
fn date(node: &Value) -> Option<String> {
    let millis = match node.get("attrs")?.get("timestamp")? {
        Value::String(text) => text.trim().parse::<i64>().ok()?,
        Value::Number(number) => number.as_i64()?,
        _ => return None,
    };
    let day = time::OffsetDateTime::from_unix_timestamp(millis.div_euclid(1000))
        .ok()?
        .date();
    Some(format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    ))
}

// MARK: Inline

/// The marks markdown has, in the order they nest, outermost first. A link is innermost: a
/// reader that styles text from its emphasis drops what is nested inside link text, and keeps
/// what is around it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mark {
    Strong,
    Em,
    Strike,
    Link(String),
}

impl Mark {
    fn rank(&self) -> u8 {
        match self {
            Mark::Strong => 0,
            Mark::Em => 1,
            Mark::Strike => 2,
            Mark::Link(_) => 3,
        }
    }

    fn opener(&self) -> &'static str {
        match self {
            Mark::Link(_) => "[",
            Mark::Strong => "**",
            Mark::Em => "*",
            Mark::Strike => "~~",
        }
    }
}

/// A text node's marks that markdown has, outermost first, and whether it is code.
/// Underline, colours, sub- and superscript have no markdown and are left out.
fn marks(node: &Value) -> (Vec<Mark>, bool) {
    let mut marks = Vec::new();
    let mut code = false;
    if let Some(Value::Array(list)) = node.get("marks") {
        for mark in list {
            match kind(mark) {
                "link" => {
                    if let Some(href) = attr(mark, "href").filter(|href| !href.trim().is_empty()) {
                        marks.push(Mark::Link(href.to_string()));
                    }
                }
                "strong" => marks.push(Mark::Strong),
                "em" => marks.push(Mark::Em),
                "strike" => marks.push(Mark::Strike),
                "code" => code = true,
                _ => {}
            }
        }
    }
    marks.sort_by_key(Mark::rank);
    marks.dedup();
    (marks, code)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A paragraph, list item or quote: hard breaks end lines, and a line's start is escaped
    /// where it would begin a heading, a list or a quote.
    Block,
    /// A heading's words, on one line.
    Heading,
    /// A table cell, on one line, its pipes escaped.
    Cell,
}

/// Writes inline nodes as markdown, opening and closing marks as they change from one text
/// node to the next.
struct Writer {
    mode: Mode,
    out: String,
    /// Marks open at the end of `out`, outermost first.
    open: Vec<Mark>,
    /// Hard breaks waiting for the next words; ones at the end are dropped.
    breaks: usize,
    /// The next words start a line: their leading spaces are dropped.
    line_start: bool,
    /// The start of a line is escaped where it would read as a block's marker.
    escape_line_start: bool,
    /// Where each `*`, `~` and `_` that can take part in emphasis is in `out`, and whether the
    /// words wrote it rather than a mark: `finish` escapes the words' where it is needed.
    delimiters: Vec<(usize, bool)>,
}

impl Writer {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            out: String::new(),
            open: Vec::new(),
            breaks: 0,
            line_start: true,
            escape_line_start: mode == Mode::Block,
            delimiters: Vec::new(),
        }
    }

    fn finish(mut self) -> String {
        self.close(0);
        let words = self.out.trim_end().len();
        self.out.truncate(words);
        escape_emphasis(self.out, &self.delimiters)
    }

    fn inline(&mut self, nodes: &[Value]) {
        // Neighbouring text with the same marks is one run of words. Written apart, two code
        // spans would run into each other, and a `]` and a `(` would make a link.
        let mut run: Option<(Vec<Mark>, bool, String)> = None;
        for node in nodes {
            if kind(node) != "text" {
                if let Some((marks, code, words)) = run.take() {
                    self.text(&words, &marks, code);
                }
                self.inline_node(node);
                continue;
            }
            let words = node.get("text").and_then(Value::as_str).unwrap_or("");
            if words.is_empty() {
                continue;
            }
            let (marks, code) = marks(node);
            match run.as_mut() {
                Some((open, open_code, text)) if *open == marks && *open_code == code => {
                    text.push_str(words);
                }
                _ => {
                    if let Some((open, open_code, text)) = run.take() {
                        self.text(&text, &open, open_code);
                    }
                    run = Some((marks, code, words.to_string()));
                }
            }
        }
        if let Some((marks, code, words)) = run {
            self.text(&words, &marks, code);
        }
    }

    fn inline_node(&mut self, node: &Value) {
        match kind(node) {
            "text" => {
                let (marks, code) = marks(node);
                let text = node.get("text").and_then(Value::as_str).unwrap_or("");
                self.text(text, &marks, code);
            }
            "hardBreak" => self.breaks += 1,
            "mention" => {
                let name = attr(node, "text")
                    .map(str::to_string)
                    .or_else(|| attr(node, "id").map(|id| format!("@{id}")))
                    .unwrap_or_default();
                self.text(&name, &[], false);
            }
            "emoji" => {
                let emoji = attr(node, "text").or_else(|| attr(node, "shortName"));
                self.text(emoji.unwrap_or(""), &[], false);
            }
            "inlineCard" => self.card(node),
            "date" => {
                if let Some(day) = date(node) {
                    self.text(&day, &[], false);
                }
            }
            "status" => self.text(attr(node, "text").unwrap_or(""), &[], false),
            "mediaInline" => {
                let name = media_label("Attachment", attr(node, "alt"));
                self.text(&name, &[Mark::Em], false);
            }
            "placeholder" => {}
            _ => {
                let nodes = children(node);
                if nodes.is_empty() {
                    self.text(attr(node, "text").unwrap_or(""), &[], false);
                } else {
                    self.inline(nodes);
                }
            }
        }
    }

    /// A card's address as a link.
    fn card(&mut self, node: &Value) {
        let data = node.get("attrs").and_then(|attrs| attrs.get("data"));
        let url = attr(node, "url")
            .or_else(|| data?.get("url")?.as_str())
            .map(str::trim)
            .filter(|url| !url.is_empty());
        match url {
            Some(url) => self.text(card_text(url), &[Mark::Link(url.to_string())], false),
            None => {
                let name = data
                    .and_then(|data| data.get("name"))
                    .and_then(Value::as_str);
                self.text(name.unwrap_or(""), &[], false);
            }
        }
    }

    /// Blocks inside a table cell, on one line: paragraphs and list items one after another,
    /// each item behind its bullet or number.
    fn flatten(&mut self, nodes: &[Value]) {
        for node in nodes {
            match kind(node) {
                "codeBlock" => {
                    self.breaks += 1;
                    self.text(&code_text(node).replace('\n', " "), &[], true);
                }
                "media" => {
                    self.breaks += 1;
                    let name = media_label("Attachment", attr(node, "alt"));
                    self.text(&name, &[Mark::Em], false);
                }
                "bulletList" | "orderedList" => {
                    let mut number = order(node);
                    for item in children(node) {
                        self.breaks += 1;
                        if kind(node) == "orderedList" {
                            self.text(&format!("{number}."), &[], false);
                            number = number.saturating_add(1);
                        } else {
                            self.text("\u{2022}", &[], false);
                        }
                        self.flatten(children(item));
                    }
                }
                _ if is_inline(node) => self.inline_node(node),
                _ => {
                    let nodes = children(node);
                    if nodes.iter().any(is_inline) {
                        self.breaks += 1;
                        self.inline(nodes);
                    } else {
                        self.flatten(nodes);
                    }
                }
            }
        }
    }

    /// `text` with `marks`, as code when `code`; a line break inside it is a hard break.
    fn text(&mut self, text: &str, marks: &[Mark], code: bool) {
        for (index, line) in with_line_feeds(text).split('\n').enumerate() {
            if index > 0 {
                self.breaks += 1;
            }
            self.words(line, marks, code);
        }
    }

    fn words(&mut self, text: &str, marks: &[Mark], code: bool) {
        let core = text.trim();
        if core.is_empty() {
            // Spaces alone change no marks, and are dropped at a line's start.
            if !text.is_empty() && !self.line_start && self.breaks == 0 {
                self.out.push_str(text);
            }
            return;
        }
        self.flush_breaks();
        let lead = &text[..text.len() - text.trim_start().len()];
        let trail = &text[text.trim_end().len()..];
        // The open marks that begin this text's own, in the same order, stay open; the rest
        // close and this text's others open.
        let keep = self
            .open
            .iter()
            .zip(marks)
            .take_while(|(open, mark)| open == mark)
            .count();
        self.close(keep);
        if !self.line_start {
            self.out.push_str(lead);
        }
        let opening = &marks[keep..];
        let opened = !opening.is_empty();
        for mark in opening {
            self.open_mark(mark.clone());
        }
        if code {
            self.code_span(core);
        } else {
            let context = Context {
                line_start: self.line_start && self.escape_line_start && !opened,
                in_link: self.open.iter().any(|mark| matches!(mark, Mark::Link(_))),
                cell: self.mode == Mode::Cell,
                before: self.out.chars().next_back(),
            };
            escape_into(&mut self.out, core, context, &mut self.delimiters);
        }
        self.line_start = false;
        self.out.push_str(trail);
    }

    fn open_mark(&mut self, mark: Mark) {
        // `![` would start an image.
        if matches!(mark, Mark::Link(_)) && self.out.ends_with('!') && !self.out.ends_with("\\!") {
            self.out.insert(self.out.len() - 1, '\\');
        }
        self.delimit(mark.opener());
        self.open.push(mark);
    }

    /// Writes a mark's `delimiter`, noting where its `*` and `~` are: they count when the
    /// words' own are decided.
    fn delimit(&mut self, delimiter: &str) {
        for (offset, character) in delimiter.char_indices() {
            if matches!(character, '*' | '~') {
                self.delimiters.push((self.out.len() + offset, false));
            }
        }
        self.out.push_str(delimiter);
    }

    /// Closes the open marks past the first `keep`, innermost first.
    fn close(&mut self, keep: usize) {
        if self.open.len() <= keep {
            return;
        }
        // Spaces before a closing mark would stop it closing, so they go after it.
        let words = self
            .out
            .trim_end_matches(|character: char| character.is_whitespace() && character != '\n')
            .len();
        let spaces = self.out.split_off(words);
        while self.open.len() > keep {
            match self.open.pop() {
                Some(Mark::Link(href)) => {
                    self.out.push_str("](");
                    self.out.push_str(&destination(&href));
                    self.out.push(')');
                }
                Some(mark) => self.delimit(mark.opener()),
                None => break,
            }
        }
        self.out.push_str(&spaces);
    }

    /// Writes the hard breaks waiting for these words: a backslash ending each line in a
    /// block, a space where everything is on one line. None before the first words.
    fn flush_breaks(&mut self) {
        let breaks = std::mem::take(&mut self.breaks);
        if breaks == 0 || self.out.is_empty() {
            return;
        }
        self.close(0);
        let words = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(words);
        match self.mode {
            Mode::Block => {
                for _ in 0..breaks {
                    self.out.push_str("\\\n");
                }
                self.line_start = true;
                self.escape_line_start = true;
            }
            Mode::Heading | Mode::Cell => self.out.push(' '),
        }
    }

    /// Fenced with one more backtick than the longest run inside it; padded when it starts or
    /// ends with one.
    fn code_span(&mut self, code: &str) {
        let code = if self.mode == Mode::Cell {
            code.replace('|', "\\|")
        } else {
            code.to_string()
        };
        let fence = "`".repeat(longest_run(&code, '`') + 1);
        let pad = if code.starts_with('`') || code.ends_with('`') {
            " "
        } else {
            ""
        };
        self.out.push_str(&fence);
        self.out.push_str(pad);
        self.out.push_str(&code);
        self.out.push_str(pad);
        self.out.push_str(&fence);
    }
}

// MARK: Escaping

#[derive(Clone, Copy)]
struct Context {
    /// The text starts a line, where a block's marker would count.
    line_start: bool,
    /// The text is a link's words, where brackets would end it.
    in_link: bool,
    /// The text is in a table cell, where a pipe would end it.
    cell: bool,
    /// The character written just before the text, if any.
    before: Option<char>,
}

/// Writes `text` to `out` with a backslash before each character that would otherwise read as
/// markdown where it stands. What follows the text is not known yet (a closing mark, a line
/// break or more words), so a character that needs knowing is escaped. Whether a `*`, `~` or
/// `_` needs it depends on all that follows, so each is written as it is and its place added to
/// `delimiters`, for `escape_emphasis` to decide.
fn escape_into(
    out: &mut String,
    text: &str,
    context: Context,
    delimiters: &mut Vec<(usize, bool)>,
) {
    let chars: Vec<char> = text.chars().collect();
    let marker = if context.line_start {
        block_marker(&chars)
    } else {
        None
    };
    for (index, &character) in chars.iter().enumerate() {
        if marker != Some(index) && matches!(character, '*' | '~' | '_') {
            delimiters.push((out.len(), true));
            out.push(character);
            continue;
        }
        let next = chars.get(index + 1).copied();
        let previous = match index {
            0 => context.before,
            _ => Some(chars[index - 1]),
        };
        let escaped = marker == Some(index)
            || match character {
                '\\' => next.is_none_or(|next| next.is_ascii_punctuation()),
                '`' => true,
                '[' => context.in_link || next == Some('^') || previous == Some('!'),
                ']' => context.in_link || matches!(next, Some('(' | ':')),
                // Right after a bracket the text before wrote, these would make it a link.
                '(' | ':' => index == 0 && context.before == Some(']'),
                '<' => next.is_some_and(|next| {
                    next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?')
                }),
                '&' => starts_entity(&chars[index + 1..]),
                '|' => context.cell,
                _ => false,
            };
        if escaped {
            out.push('\\');
        }
        out.push(character);
    }
}

/// `out` with a backslash before each run of `*`, `~` or `_` that the words wrote and that
/// could make emphasis where it stands. `delimiters` has the byte offset in `out` of each such
/// character that takes part in emphasis, and whether the words wrote it rather than a mark.
/// A run that a later delimiter could close, or that stands beside a mark's own (which it would
/// join), is escaped. One with nothing after it to close it, one between spaces and an `_` in
/// the middle of a word read as themselves, so paths and wildcards keep their characters. That
/// can only be told once everything after a run is written.
fn escape_emphasis(out: String, delimiters: &[(usize, bool)]) -> String {
    if !delimiters.iter().any(|&(_, from_words)| from_words) {
        return out;
    }
    let chars: Vec<char> = out.chars().collect();
    // For each character: `Some(true)` when the words wrote it as a delimiter, `Some(false)`
    // when a mark did, `None` when it is not one.
    let mut written_by: Vec<Option<bool>> = vec![None; chars.len()];
    let mut offsets = out.char_indices().enumerate();
    for &(place, from_words) in delimiters {
        if let Some((index, _)) = offsets.find(|&(_, (byte, _))| byte == place) {
            written_by[index] = Some(from_words);
        }
    }
    let at = |index: usize| chars.get(index).copied();
    // The start and the end of the text count as spaces.
    let space = |character: Option<char>| character.is_none_or(char::is_whitespace);
    let alphanumeric = |character: Option<char>| character.is_some_and(char::is_alphanumeric);
    let mut escaped = vec![false; chars.len()];
    // For `*`, `~` and `_`: whether a delimiter that could close emphasis comes after the place
    // reached, going from the end.
    let mut closer_after = [false; 3];
    let mut end = chars.len();
    while end > 0 {
        let Some(from_words) = written_by[end - 1] else {
            end -= 1;
            continue;
        };
        let character = chars[end - 1];
        let kind = match character {
            '*' => 0,
            '~' => 1,
            _ => 2,
        };
        // The words' run of it; a mark's delimiters are taken one at a time.
        let mut start = end - 1;
        while from_words
            && start > 0
            && chars[start - 1] == character
            && written_by[start - 1] == Some(true)
        {
            start -= 1;
        }
        let before = start.checked_sub(1).and_then(at);
        let after = at(end);
        let inert = (space(before) && space(after))
            || (character == '_' && alphanumeric(before) && alphanumeric(after));
        // Beside a mark's own delimiter, a run would join it or stop it from closing.
        let touches_a_mark = start
            .checked_sub(1)
            .is_some_and(|index| written_by[index] == Some(false))
            || written_by.get(end) == Some(&Some(false));
        let joins_another = before == Some(character) || after == Some(character) || touches_a_mark;
        if from_words && (joins_another || (!inert && closer_after[kind])) {
            escaped[start..end].fill(true);
        } else {
            closer_after[kind] |= !inert && !space(before);
        }
        end = start;
    }
    let mut escaped_out = String::with_capacity(out.len() + delimiters.len());
    for (character, escape) in chars.into_iter().zip(escaped) {
        if escape {
            escaped_out.push('\\');
        }
        escaped_out.push(character);
    }
    escaped_out
}

/// `&amp;`, `&#123;`: an ampersand that would start a character reference.
fn starts_entity(rest: &[char]) -> bool {
    if rest.first() == Some(&'#') {
        return true;
    }
    let name = rest
        .iter()
        .take_while(|character| character.is_ascii_alphanumeric())
        .count();
    name > 0 && rest.get(name) == Some(&';')
}

/// Where a line's start would read as a block's marker, the index of the character to escape:
/// a heading's #s, a quote, a bullet, an ordered list's number, a task's box, a rule or a
/// setext heading's underline.
fn block_marker(chars: &[char]) -> Option<usize> {
    let first = *chars.first()?;
    let ends_marker = |at: usize| {
        chars
            .get(at)
            .is_none_or(|&character| character == ' ' || character == '\t')
    };
    let only = |mark: char| {
        chars
            .iter()
            .all(|&character| character == mark || character == ' ' || character == '\t')
    };
    match first {
        '#' => {
            let hashes = chars
                .iter()
                .take_while(|&&character| character == '#')
                .count();
            (hashes <= 6 && ends_marker(hashes)).then_some(0)
        }
        '>' => Some(0),
        '-' | '+' | '*' => {
            (ends_marker(1) || only(first) || is_table_delimiter(chars)).then_some(0)
        }
        '|' | ':' => is_table_delimiter(chars).then_some(0),
        // Three tildes open a code fence.
        '~' => {
            let tildes = chars
                .iter()
                .take_while(|&&character| character == '~')
                .count();
            (tildes >= 3).then_some(0)
        }
        '=' | '_' => only(first).then_some(0),
        '[' => {
            let boxed = matches!(chars.get(1), Some(' ' | 'x' | 'X')) && chars.get(2) == Some(&']');
            (boxed && ends_marker(3)).then_some(0)
        }
        _ => {
            let digits = chars
                .iter()
                .take_while(|character| character.is_ascii_digit())
                .count();
            let delimited = matches!(chars.get(digits), Some('.' | ')'));
            ((1..=9).contains(&digits) && delimited && ends_marker(digits + 1)).then_some(digits)
        }
    }
}

/// A line of dashes, colons and pipes with a pipe and a dash in it. Under a line with a pipe it
/// would be a table's delimiter row.
fn is_table_delimiter(chars: &[char]) -> bool {
    chars
        .iter()
        .all(|character| matches!(character, '-' | ':' | '|' | ' ' | '\t'))
        && chars.contains(&'|')
        && chars.contains(&'-')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::markdown;

    fn text(text: &str) -> serde_json::Value {
        json!({"type": "text", "text": text})
    }

    fn marked(text: &str, marks: serde_json::Value) -> serde_json::Value {
        json!({"type": "text", "text": text, "marks": marks})
    }

    fn paragraph(content: serde_json::Value) -> serde_json::Value {
        json!({"type": "paragraph", "content": content})
    }

    fn doc(content: serde_json::Value) -> serde_json::Value {
        json!({"type": "doc", "version": 1, "content": content})
    }

    fn item(content: serde_json::Value) -> serde_json::Value {
        json!({"type": "listItem", "content": content})
    }

    fn table_of(rows: serde_json::Value) -> serde_json::Value {
        doc(json!([{"type": "table", "content": rows}]))
    }

    fn row(cells: serde_json::Value) -> serde_json::Value {
        json!({"type": "tableRow", "content": cells})
    }

    fn cell(words: &str) -> serde_json::Value {
        json!({"type": "tableCell", "content": [paragraph(json!([text(words)]))]})
    }

    fn spanning(words: &str, spans: serde_json::Value) -> serde_json::Value {
        json!({"type": "tableCell", "attrs": spans, "content": [paragraph(json!([text(words)]))]})
    }

    #[test]
    fn a_ticket_description_reads_as_markdown() {
        let adf = doc(json!([
            {"type": "heading", "attrs": {"level": 2}, "content": [text("Context")]},
            paragraph(json!([
                text("Drivers whose car isn't in the catalogue "),
                marked("can't finish sign-up", json!([{"type": "strong"}])),
                text(". See the "),
                marked("design notes", json!([{"type": "link", "attrs": {"href": "https://example.test/notes"}}])),
                text(" first."),
            ])),
            {"type": "heading", "attrs": {"level": 3}, "content": [text("Acceptance")]},
            {"type": "bulletList", "content": [
                item(json!([
                    paragraph(json!([text("A form asks for:")])),
                    {"type": "bulletList", "content": [
                        item(json!([paragraph(json!([text("make and model")]))])),
                        item(json!([paragraph(json!([text("registration")]))])),
                    ]},
                ])),
                item(json!([paragraph(json!([text("Saved vehicles show up in the picker")]))])),
            ]},
            {"type": "orderedList", "attrs": {"order": 1}, "content": [
                item(json!([paragraph(json!([text("Add the form")]))])),
                item(json!([paragraph(json!([text("Check the registration")]))])),
            ]},
            {"type": "codeBlock", "attrs": {"language": "swift"}, "content": [
                text("let form = VehicleForm()\nform.validate()"),
            ]},
        ]));
        assert_eq!(
            markdown(&adf),
            "## Context\n\
             \n\
             Drivers whose car isn't in the catalogue **can't finish sign-up**. See the \
             [design notes](https://example.test/notes) first.\n\
             \n\
             ### Acceptance\n\
             \n\
             - A form asks for:\n  \
               - make and model\n  \
               - registration\n\
             - Saved vehicles show up in the picker\n\
             \n\
             1. Add the form\n\
             2. Check the registration\n\
             \n\
             ```swift\n\
             let form = VehicleForm()\n\
             form.validate()\n\
             ```"
        );
    }

    #[test]
    fn a_heading_has_as_many_hashes_as_its_level() {
        let adf = doc(json!([
            {"type": "heading", "attrs": {"level": 1}, "content": [text("Title")]},
            {"type": "heading", "attrs": {"level": 4}, "content": [text("Detail")]},
        ]));
        assert_eq!(markdown(&adf), "# Title\n\n#### Detail");
    }

    #[test]
    fn empty_paragraphs_leave_no_gap() {
        let adf = doc(json!([
            paragraph(json!([text("First")])),
            {"type": "paragraph"},
            paragraph(json!([text("  ")])),
            paragraph(json!([text("Second")])),
        ]));
        assert_eq!(markdown(&adf), "First\n\nSecond");
    }

    #[test]
    fn an_ordered_list_counts_from_its_start_and_nests_under_the_number() {
        let adf = doc(
            json!([{"type": "orderedList", "attrs": {"order": 9}, "content": [
                item(json!([paragraph(json!([text("Ninth")]))])),
                item(json!([
                    paragraph(json!([text("Tenth")])),
                    {"type": "bulletList", "content": [item(json!([paragraph(json!([text("detail")]))]))]},
                ])),
            ]}]),
        );
        assert_eq!(markdown(&adf), "9. Ninth\n10. Tenth\n    - detail");
    }

    #[test]
    fn a_nested_ordered_list_not_starting_at_one_follows_a_blank_line() {
        let adf = doc(json!([{"type": "bulletList", "content": [item(json!([
            paragraph(json!([text("Steps")])),
            {"type": "orderedList", "attrs": {"order": 3}, "content": [
                item(json!([paragraph(json!([text("third")]))])),
            ]},
        ]))]}]));
        assert_eq!(markdown(&adf), "- Steps\n\n  3. third");
    }

    #[test]
    fn a_code_block_fence_outlasts_the_backticks_inside_it() {
        let adf = doc(json!([{"type": "codeBlock", "content": [text("echo ```")]}]));
        assert_eq!(markdown(&adf), "````\necho ```\n````");
    }

    #[test]
    fn a_code_block_in_a_list_item_stays_inside_it() {
        let adf = doc(json!([{"type": "bulletList", "content": [item(json!([
            paragraph(json!([text("Run")])),
            {"type": "codeBlock", "attrs": {"language": "sh"}, "content": [text("make\nmake test")]},
        ]))]}]));
        assert_eq!(markdown(&adf), "- Run\n  ```sh\n  make\n  make test\n  ```");
    }

    #[test]
    fn every_line_of_a_quote_or_panel_is_quoted() {
        let adf = doc(json!([
            {"type": "blockquote", "content": [
                paragraph(json!([text("One")])),
                paragraph(json!([text("Two")])),
            ]},
            {"type": "panel", "attrs": {"panelType": "warning"}, "content": [
                paragraph(json!([text("Mind the gap")])),
            ]},
        ]));
        assert_eq!(markdown(&adf), "> One\n>\n> Two\n\n> Mind the gap");
    }

    #[test]
    fn a_rule_is_three_dashes_between_blank_lines() {
        let adf = doc(json!([
            paragraph(json!([text("Above")])),
            {"type": "rule"},
            paragraph(json!([text("Below")])),
        ]));
        assert_eq!(markdown(&adf), "Above\n\n---\n\nBelow");
    }

    #[test]
    fn a_hard_break_ends_the_line_inside_its_paragraph() {
        let adf = doc(json!([paragraph(json!([
            text("Line one "),
            {"type": "hardBreak"},
            text("- line two"),
            {"type": "hardBreak"},
        ]))]));
        assert_eq!(markdown(&adf), "Line one\\\n\\- line two");
    }

    #[test]
    fn spaces_at_the_edge_of_a_mark_sit_outside_it() {
        let adf = doc(json!([paragraph(json!([
            text("Ship"),
            marked(" today ", json!([{"type": "strong"}])),
            text("or"),
        ]))]));
        assert_eq!(markdown(&adf), "Ship **today** or");
    }

    #[test]
    fn marks_on_the_same_text_nest_in_a_fixed_order() {
        let adf = doc(json!([
            paragraph(json!([marked(
                "one",
                json!([{"type": "em"}, {"type": "strike"}])
            )])),
            paragraph(json!([marked(
                "two",
                json!([{"type": "strike"}, {"type": "em"}])
            )])),
        ]));
        assert_eq!(markdown(&adf), "*~~one~~*\n\n*~~two~~*");
    }

    #[test]
    fn a_mark_shared_by_neighbouring_text_stays_open() {
        let bold = json!({"type": "strong"});
        let link = json!({"type": "link", "attrs": {"href": "https://example.test/guide"}});
        let adf = doc(json!([paragraph(json!([
            marked("Read ", json!([bold])),
            marked("the guide", json!([bold, link])),
            marked(" first", json!([bold])),
        ]))]));
        assert_eq!(
            markdown(&adf),
            "**Read [the guide](https://example.test/guide) first**"
        );
    }

    #[test]
    fn inline_code_is_fenced_past_its_backticks_and_not_escaped() {
        let adf = doc(json!([paragraph(json!([
            text("Call "),
            marked("run_all(*args)", json!([{"type": "code"}])),
            text(" or "),
            marked("a`b", json!([{"type": "code"}])),
        ]))]));
        assert_eq!(markdown(&adf), "Call `run_all(*args)` or ``a`b``");
    }

    #[test]
    fn a_link_address_is_encoded_where_markdown_would_cut_it() {
        let adf = doc(json!([paragraph(json!([marked(
            "spec",
            json!([{"type": "link", "attrs": {"href": "https://example.test/a b_(c)"}}])
        )]))]));
        assert_eq!(markdown(&adf), "[spec](https://example.test/a%20b_%28c%29)");
    }

    #[test]
    fn mentions_emoji_dates_and_statuses_read_as_text() {
        let adf = doc(json!([paragraph(json!([
            {"type": "mention", "attrs": {"id": "0001", "text": "@Ada Example"}},
            text(" "),
            {"type": "emoji", "attrs": {"shortName": ":thumbsup:"}},
            text(" by "),
            {"type": "date", "attrs": {"timestamp": "1790812800000"}},
            text(", "),
            {"type": "status", "attrs": {"text": "Blocked", "color": "red"}},
        ]))]));
        assert_eq!(
            markdown(&adf),
            "@Ada Example :thumbsup: by 2026-10-01, Blocked"
        );
    }

    #[test]
    fn cards_link_their_address_and_jira_issues_read_as_their_key() {
        let adf = doc(json!([
            paragraph(json!([
                text("Blocked by "),
                {"type": "inlineCard", "attrs": {"url": "https://acme.atlassian.net/browse/APP-12?focusedCommentId=3"}},
            ])),
            {"type": "blockCard", "attrs": {"url": "https://example.test/board"}},
        ]));
        assert_eq!(
            markdown(&adf),
            "Blocked by [APP-12](https://acme.atlassian.net/browse/APP-12?focusedCommentId=3)\n\
             \n\
             [https://example.test/board](https://example.test/board)"
        );
    }

    #[test]
    fn a_table_keeps_its_rows_under_the_first_as_header() {
        let header = |words: &str| json!({"type": "tableHeader", "content": [paragraph(json!([text(words)]))]});
        let adf = table_of(json!([
            row(json!([header("Field"), header("Rule")])),
            row(json!([cell("Plate"), cell("Up to 8 characters")])),
            row(json!([cell("Make"), cell("Free text")])),
        ]));
        assert_eq!(
            markdown(&adf),
            "| Field | Rule |\n\
             | --- | --- |\n\
             | Plate | Up to 8 characters |\n\
             | Make | Free text |"
        );
    }

    #[test]
    fn a_pipe_in_a_table_cell_is_escaped() {
        let adf = table_of(json!([
            row(json!([cell("Rule")])),
            row(json!([cell("A|B or C")]))
        ]));
        assert_eq!(markdown(&adf), "| Rule |\n| --- |\n| A\\|B or C |");
    }

    #[test]
    fn a_cell_spanning_columns_is_followed_by_empty_ones() {
        let adf = table_of(json!([
            row(json!([cell("Field"), cell("Rule")])),
            row(json!([spanning("Both columns", json!({"colspan": 2}))])),
        ]));
        assert_eq!(
            markdown(&adf),
            "| Field | Rule |\n| --- | --- |\n| Both columns |  |"
        );
    }

    #[test]
    fn a_cell_spanning_rows_leaves_an_empty_cell_in_each_row_it_spans() {
        let adf = table_of(json!([
            row(json!([cell("Field"), cell("Rule"), cell("Example")])),
            row(json!([
                spanning("Plate", json!({"rowspan": 2})),
                cell("Letters"),
                cell("AB")
            ])),
            row(json!([cell("Digits"), cell("12")])),
        ]));
        assert_eq!(
            markdown(&adf),
            "| Field | Rule | Example |\n\
             | --- | --- | --- |\n\
             | Plate | Letters | AB |\n\
             |  | Digits | 12 |"
        );
    }

    #[test]
    fn a_row_shorter_than_the_header_keeps_its_own_cells() {
        let adf = table_of(json!([
            row(json!([cell("A"), cell("B"), cell("C")])),
            row(json!([cell("1")])),
        ]));
        assert_eq!(markdown(&adf), "| A | B | C |\n| --- | --- | --- |\n| 1 |");
    }

    #[test]
    fn one_wide_row_does_not_widen_the_rows_beside_it() {
        let mut rows = vec![row(json!((0..150).map(|_| cell("x")).collect::<Vec<_>>()))];
        rows.extend((0..150).map(|_| row(json!([cell("x")]))));
        let adf = table_of(json!(rows));
        let markdown = markdown(&adf);
        assert!(
            markdown.len() < adf.to_string().len(),
            "{} bytes of markdown for {} bytes of document",
            markdown.len(),
            adf.to_string().len()
        );
        assert!(markdown.ends_with("\n| x |"));
    }

    #[test]
    fn spans_past_the_columns_followed_add_no_more_empty_cells() {
        let hostile = spanning("x", json!({"colspan": 1_000_000, "rowspan": 1_000_000}));
        let adf = table_of(json!((0..200)
            .map(|_| row(json!([hostile.clone()])))
            .collect::<Vec<_>>()));
        let markdown = markdown(&adf);
        assert!(
            markdown.len() < adf.to_string().len(),
            "{} bytes of markdown for {} bytes of document",
            markdown.len(),
            adf.to_string().len()
        );
    }

    #[test]
    fn a_task_list_ticks_what_is_done_and_nests_under_its_item() {
        let task = |state: &str, words: &str| json!({"type": "taskItem", "attrs": {"localId": "t", "state": state}, "content": [text(words)]});
        let adf = doc(
            json!([{"type": "taskList", "attrs": {"localId": "l"}, "content": [
                task("DONE", "Write the form"),
                task("TODO", "Ship it"),
                {"type": "taskList", "attrs": {"localId": "n"}, "content": [task("TODO", "1. Tell support")]},
            ]}]),
        );
        assert_eq!(
            markdown(&adf),
            "- [x] Write the form\n- [ ] Ship it\n  - [ ] 1. Tell support"
        );
    }

    #[test]
    fn an_attachment_is_named_and_an_image_from_the_web_shows() {
        let adf = doc(json!([
            {"type": "mediaSingle", "content": [
                {"type": "media", "attrs": {"type": "file", "id": "f1", "collection": "", "alt": "form-mockup.png"}},
            ]},
            {"type": "mediaGroup", "content": [
                {"type": "media", "attrs": {"type": "file", "id": "f2", "collection": ""}},
            ]},
            {"type": "mediaSingle", "content": [
                {"type": "media", "attrs": {"type": "external", "url": "https://example.test/flow.png", "alt": "Flow"}},
            ]},
        ]));
        assert_eq!(
            markdown(&adf),
            "*Image: form-mockup.png*\n\n*Attachment*\n\n![Flow](https://example.test/flow.png)"
        );
    }

    #[test]
    fn an_expand_shows_its_title_in_bold_above_its_content() {
        let adf = doc(
            json!([{"type": "expand", "attrs": {"title": "Logs"}, "content": [
                paragraph(json!([text("Nothing unusual")])),
            ]}]),
        );
        assert_eq!(markdown(&adf), "**Logs**\n\nNothing unusual");
    }

    #[test]
    fn a_line_that_would_start_a_block_is_escaped() {
        let adf = doc(json!([
            paragraph(json!([text("# Not a heading")])),
            paragraph(json!([text("1. Not a list")])),
            paragraph(json!([text("> Not a quote")])),
            paragraph(json!([text("- Not a bullet")])),
        ]));
        assert_eq!(
            markdown(&adf),
            "\\# Not a heading\n\n1\\. Not a list\n\n\\> Not a quote\n\n\\- Not a bullet"
        );
    }

    #[test]
    fn backticks_in_the_words_are_escaped() {
        let adf = doc(json!([paragraph(json!([text("Run `make` first")]))]));
        assert_eq!(markdown(&adf), "Run \\`make\\` first");
    }

    #[test]
    fn brackets_tags_and_entities_the_words_could_make_are_escaped() {
        let adf = doc(json!([paragraph(json!([text(
            "[not](a link) & not &amp; <b>bold</b>"
        )]))]));
        assert_eq!(
            markdown(&adf),
            "[not\\](a link) & not \\&amp; \\<b>bold\\</b>"
        );
    }

    #[test]
    fn emphasis_the_words_could_make_is_escaped_up_to_its_closing_mark() {
        let adf = doc(json!([paragraph(json!([text(
            "Use *this* and __that__ or ~~those~~"
        )]))]));
        assert_eq!(
            markdown(&adf),
            "Use \\*this* and \\_\\_that__ or \\~\\~those~~"
        );
    }

    #[test]
    fn ordinary_punctuation_is_left_alone() {
        let words = "Rename user_id in C:\\temp\\app, [WIP] 2 * 3 = 6 ~ about 1.5 a < b, R&D (soon)! #12 - done";
        let adf = doc(json!([paragraph(json!([text(words)]))]));
        assert_eq!(markdown(&adf), words);
    }

    #[test]
    fn unknown_nodes_keep_their_words() {
        let adf = doc(json!([
            {"type": "layoutSection", "content": [
                {"type": "layoutColumn", "content": [paragraph(json!([text("Left")]))]},
                {"type": "layoutColumn", "content": [paragraph(json!([text("Right")]))]},
            ]},
            paragraph(json!([
                text("Ask "),
                {"type": "futureInline", "attrs": {"text": "someone"}},
            ])),
        ]));
        assert_eq!(markdown(&adf), "Left\n\nRight\n\nAsk someone");
    }

    #[test]
    fn a_list_item_opening_with_code_or_a_list_puts_it_under_its_marker() {
        let adf = doc(json!([
            {"type": "bulletList", "content": [item(json!([
                {"type": "codeBlock", "content": [text("make\r\nmake test")]},
                paragraph(json!([text("then it passes")])),
            ]))]},
            {"type": "orderedList", "content": [item(json!([
                {"type": "bulletList", "content": [item(json!([paragraph(json!([text("nested")]))]))]},
            ]))]},
        ]));
        assert_eq!(
            markdown(&adf),
            "-\n  ```\n  make\n  make test\n  ```\n\n  then it passes\n\n1.\n   - nested"
        );
    }

    #[test]
    fn a_backslash_ending_a_line_is_kept_apart_from_the_break_after_it() {
        let adf = doc(json!([paragraph(json!([
            text("path\\ "),
            {"type": "hardBreak"},
            text("next"),
        ]))]));
        assert_eq!(markdown(&adf), "path\\\\\\\nnext");
    }

    #[test]
    fn a_backslash_ending_bold_text_does_not_swallow_the_closing_mark() {
        let adf = doc(json!([paragraph(json!([
            marked("C:\\ ", json!([{"type": "strong"}])),
            text("done"),
        ]))]));
        assert_eq!(markdown(&adf), "**C:\\\\** done");
    }

    #[test]
    fn a_backslash_ending_link_text_does_not_swallow_the_closing_bracket() {
        let link = json!({"type": "link", "attrs": {"href": "https://e.test/l"}});
        let adf = doc(json!([paragraph(json!([
            marked("dir\\ ", json!([link])),
            text("after"),
        ]))]));
        assert_eq!(markdown(&adf), "[dir\\\\](https://e.test/l) after");
    }

    #[test]
    fn a_star_ending_bold_text_does_not_join_the_closing_mark() {
        let adf = doc(json!([paragraph(json!([
            marked("a * ", json!([{"type": "strong"}])),
            text("b"),
        ]))]));
        assert_eq!(markdown(&adf), "**a \\*** b");
    }

    #[test]
    fn a_link_after_an_exclamation_mark_is_not_an_image() {
        let link = json!({"type": "link", "attrs": {"href": "https://example.test/x"}});
        let adf = doc(json!([paragraph(json!([
            text("Wow!"),
            marked("see this", json!([link])),
        ]))]));
        assert_eq!(markdown(&adf), "Wow\\![see this](https://example.test/x)");
    }

    #[test]
    fn a_heading_ending_in_hashes_keeps_them() {
        let adf = doc(json!([
            {"type": "heading", "attrs": {"level": 2}, "content": [text("Issue #")]},
            {"type": "heading", "attrs": {"level": 3}, "content": [text("C#")]},
        ]));
        assert_eq!(markdown(&adf), "## Issue \\#\n\n### C#");
    }

    #[test]
    fn a_table_cell_holds_its_list_and_code_on_one_line() {
        let cell = |content: serde_json::Value| json!({"type": "tableCell", "content": content});
        let adf = doc(json!([{"type": "table", "content": [
            {"type": "tableRow", "content": [
                {"type": "tableHeader", "content": [paragraph(json!([text("Step")]))]},
                {"type": "tableHeader", "content": [paragraph(json!([text("Notes")]))]},
            ]},
            {"type": "tableRow", "content": [
                cell(json!([paragraph(json!([text("Build")]))])),
                cell(json!([
                    {"type": "orderedList", "content": [
                        item(json!([paragraph(json!([text("fetch")]))])),
                        item(json!([paragraph(json!([text("compile")]))])),
                    ]},
                    {"type": "codeBlock", "content": [text("make\nmake test")]},
                ])),
            ]},
        ]}]));
        assert_eq!(
            markdown(&adf),
            "| Step | Notes |\n| --- | --- |\n| Build | 1. fetch 2. compile `make make test` |"
        );
    }

    #[test]
    fn marks_stay_apart_from_the_punctuation_beside_them() {
        let strong = json!({"type": "strong"});
        let em = json!({"type": "em"});
        let adf = doc(json!([paragraph(json!([
            text("Use "),
            marked("(carefully)", json!([strong])),
            text(", then "),
            marked("\"quoted\"", json!([em])),
        ]))]));
        assert_eq!(markdown(&adf), "Use **(carefully)**, then *\"quoted\"*");
    }

    #[test]
    fn paths_and_wildcards_are_left_alone() {
        let words = "Edit ~/projects/herdr, src/_app.tsx, *.swift and user_id";
        let adf = doc(json!([paragraph(json!([text(words)]))]));
        assert_eq!(markdown(&adf), words);
    }

    #[test]
    fn neighbouring_code_with_the_same_marks_is_one_code_span() {
        let adf = doc(json!([paragraph(json!([
            marked("make", json!([{"type": "code"}])),
            marked("-test", json!([{"type": "code"}])),
        ]))]));
        assert_eq!(markdown(&adf), "`make-test`");
    }

    #[test]
    fn neighbouring_plain_text_is_read_as_one_run() {
        let adf = doc(json!([paragraph(json!([
            text("see [docs]"),
            text("(here)")
        ]))]));
        assert_eq!(markdown(&adf), "see [docs\\](here)");
    }

    #[test]
    fn emphasis_wraps_a_link_so_that_a_reader_keeps_both() {
        let link = json!({"type": "link", "attrs": {"href": "https://e.test/guide"}});
        let adf = doc(json!([paragraph(json!([marked(
            "the guide",
            json!([link, {"type": "strong"}])
        ),]))]));
        assert_eq!(markdown(&adf), "**[the guide](https://e.test/guide)**");
    }

    #[test]
    fn text_that_adds_emphasis_to_a_link_part_way_closes_the_link_first() {
        let link = json!({"type": "link", "attrs": {"href": "https://e.test/guide"}});
        let adf = doc(json!([paragraph(json!([
            marked("read ", json!([link])),
            marked("this", json!([link, {"type": "strong"}])),
        ]))]));
        assert_eq!(
            markdown(&adf),
            "[read](https://e.test/guide) **[this](https://e.test/guide)**"
        );
    }

    #[test]
    fn a_tilde_before_a_closing_mark_is_escaped_so_that_the_mark_closes() {
        let adf = doc(json!([paragraph(json!([
            marked("about ~", json!([{"type": "em"}])),
            text(" a day"),
        ]))]));
        assert_eq!(markdown(&adf), "*about \\~* a day");
    }

    #[test]
    fn three_tildes_opening_a_line_are_not_a_code_fence() {
        let adf = doc(json!([paragraph(json!([text("~~~ not code")]))]));
        assert_eq!(markdown(&adf), "\\~\\~\\~ not code");
    }

    #[test]
    fn a_line_of_dashes_and_pipes_is_not_a_table_delimiter() {
        let adf = doc(json!([paragraph(json!([
            text("a | b"),
            {"type": "hardBreak"},
            text("--- | ---"),
        ]))]));
        assert_eq!(markdown(&adf), "a | b\\\n\\--- | ---");
    }

    #[test]
    fn brackets_after_an_exclamation_mark_do_not_start_an_image() {
        let link = json!({"type": "link", "attrs": {"href": "https://e.test/x"}});
        let adf = doc(json!([paragraph(json!([
            text("![alt "),
            marked("see", json!([link])),
        ]))]));
        assert_eq!(markdown(&adf), "!\\[alt [see](https://e.test/x)");
    }

    #[test]
    fn a_lone_carriage_return_ends_a_line_like_any_other() {
        let adf = doc(json!([paragraph(json!([text("foo\r# bar")]))]));
        assert_eq!(markdown(&adf), "foo\\\n\\# bar");
    }

    #[test]
    fn a_lone_carriage_return_in_code_ends_a_line() {
        let adf = doc(json!([{"type": "codeBlock", "content": [text("make\rmake test")]}]));
        assert_eq!(markdown(&adf), "```\nmake\nmake test\n```");
    }

    #[test]
    fn list_numbers_stay_within_the_nine_digits_markdown_reads() {
        let adf = doc(json!([
            {"type": "orderedList", "attrs": {"order": 1_000_000_000_u64}, "content": [
                item(json!([paragraph(json!([text("a")]))])),
                item(json!([paragraph(json!([text("b")]))])),
            ]},
        ]));
        assert_eq!(markdown(&adf), "999999998. a\n999999999. b");
    }

    #[test]
    fn a_very_long_run_of_underscores_is_escaped_in_one_pass() {
        let run = "_".repeat(100_000);
        let adf = doc(json!([paragraph(json!([text(&run)]))]));
        assert_eq!(markdown(&adf), "\\_".repeat(100_000));
    }
}
