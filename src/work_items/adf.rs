//! Atlassian Document Format, the JSON Jira keeps descriptions and comments in, as
//! GitHub-flavoured markdown. Headings, lists, code, quotes, tables and links survive for a
//! client to render and for an agent's brief to carry, and text that would otherwise read as
//! markdown is escaped where it stands, and only there.

use serde_json::Value;

/// Columns a table cell may span; past that a span is taken as a mistake.
const MAX_SPAN: u64 = 64;

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

/// The number an ordered list starts at.
fn order(node: &Value) -> u64 {
    number_attr(node, "order").unwrap_or(1)
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

/// The words of a code block, one line break kind throughout.
fn code_text(node: &Value) -> String {
    let code: String = children(node)
        .iter()
        .filter_map(|child| child.get("text").and_then(Value::as_str))
        .collect();
    code.replace("\r\n", "\n")
}

/// Fenced with more backticks than any run inside it, and its language.
fn code_block(node: &Value) -> String {
    let code = code_text(node);
    let code = code.trim_end_matches(['\n', '\r']);
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
/// row is a header unless it was switched off. A cell is one line; one spanning columns is
/// followed by empty ones so that the columns stay in line.
fn table(node: &Value) -> String {
    let rows: Vec<Vec<String>> = children(node)
        .iter()
        .map(|row| {
            let mut cells = Vec::new();
            for cell in children(row) {
                cells.push(cell_text(cell));
                let span = number_attr(cell, "colspan").unwrap_or(1).clamp(1, MAX_SPAN);
                cells.extend((1..span).map(|_| String::new()));
            }
            cells
        })
        .filter(|cells| !cells.is_empty())
        .collect();
    let Some(columns) = rows.iter().map(Vec::len).max() else {
        return String::new();
    };
    let line = |cells: &[String]| {
        let mut line = String::from("|");
        for column in 0..columns {
            line.push(' ');
            line.push_str(cells.get(column).map_or("", String::as_str));
            line.push_str(" |");
        }
        line
    };
    let mut lines = vec![line(&rows[0]), format!("|{}", " --- |".repeat(columns))];
    lines.extend(rows[1..].iter().map(|row| line(row)));
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

/// The marks markdown has, in the order they nest, outermost first.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mark {
    Link(String),
    Strong,
    Em,
    Strike,
}

impl Mark {
    fn rank(&self) -> u8 {
        match self {
            Mark::Link(_) => 0,
            Mark::Strong => 1,
            Mark::Em => 2,
            Mark::Strike => 3,
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
        }
    }

    fn finish(mut self) -> String {
        self.close(0);
        let words = self.out.trim_end().len();
        self.out.truncate(words);
        self.out
    }

    fn inline(&mut self, nodes: &[Value]) {
        for node in nodes {
            self.inline_node(node);
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
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.breaks += 1;
            }
            self.words(line.trim_end_matches('\r'), marks, code);
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
        // Marks this text shares with the open ones stay open; the rest close.
        let keep = self
            .open
            .iter()
            .take_while(|mark| marks.contains(mark))
            .count();
        self.close(keep);
        if !self.line_start {
            self.out.push_str(lead);
        }
        let opening: Vec<Mark> = marks
            .iter()
            .filter(|mark| !self.open.contains(mark))
            .cloned()
            .collect();
        let opened = !opening.is_empty();
        for mark in opening {
            self.open_mark(mark);
        }
        if code {
            self.code_span(core);
        } else {
            let context = Context {
                line_start: self.line_start && self.escape_line_start && !opened,
                in_link: self.open.iter().any(|mark| matches!(mark, Mark::Link(_))),
                cell: self.mode == Mode::Cell,
                before: self.out.chars().next_back(),
                after: trail.chars().next(),
            };
            let escaped = escape(core, context);
            self.out.push_str(&escaped);
        }
        self.line_start = false;
        self.out.push_str(trail);
    }

    fn open_mark(&mut self, mark: Mark) {
        // `![` would start an image.
        if matches!(mark, Mark::Link(_)) && self.out.ends_with('!') && !self.out.ends_with("\\!") {
            self.out.insert(self.out.len() - 1, '\\');
        }
        self.out.push_str(mark.opener());
        self.open.push(mark);
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
                Some(mark) => self.out.push_str(mark.opener()),
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
    /// The space that follows the text, if any; otherwise what follows is not known.
    after: Option<char>,
}

/// `text` with a backslash before each character that would otherwise read as markdown where
/// it stands.
fn escape(text: &str, context: Context) -> String {
    let chars: Vec<char> = text.chars().collect();
    let marker = if context.line_start {
        block_marker(&chars)
    } else {
        None
    };
    let mut out = String::with_capacity(text.len() + 4);
    for (index, &character) in chars.iter().enumerate() {
        let previous = match index {
            0 => context.before,
            _ => Some(chars[index - 1]),
        };
        let next = chars.get(index + 1).copied().or(context.after);
        let escaped = marker == Some(index)
            || match character {
                '\\' => next.is_none_or(|next| next.is_ascii_punctuation()),
                '`' => true,
                // Between spaces they can't open or close anything.
                '*' | '~' => !(is_space(previous) && is_space(next)),
                '_' => underscore_counts(&chars, index, context),
                '[' => context.in_link || next == Some('^'),
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
    out
}

fn is_space(character: Option<char>) -> bool {
    character.is_some_and(char::is_whitespace)
}

fn is_alphanumeric(character: Option<char>) -> bool {
    character.is_some_and(char::is_alphanumeric)
}

/// An underscore could open or close emphasis unless its run is inside a word, as in
/// `snake_case`, or stands between spaces.
fn underscore_counts(chars: &[char], index: usize, context: Context) -> bool {
    let start = chars[..index]
        .iter()
        .rposition(|&character| character != '_')
        .map_or(0, |position| position + 1);
    let end = chars[index..]
        .iter()
        .position(|&character| character != '_')
        .map_or(chars.len(), |position| index + position);
    let previous = match start {
        0 => context.before,
        _ => Some(chars[start - 1]),
    };
    let next = chars.get(end).copied().or(context.after);
    let inside_word = is_alphanumeric(previous) && is_alphanumeric(next);
    let between_spaces = is_space(previous) && is_space(next);
    !(inside_word || between_spaces)
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
        '-' | '+' | '*' => (ends_marker(1) || only(first)).then_some(0),
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
            text("or "),
            marked("never", json!([{"type": "em"}, {"type": "strike"}])),
        ]))]));
        assert_eq!(markdown(&adf), "Ship **today** or *~~never~~*");
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
        let cell = |kind: &str, words: &str| json!({"type": kind, "content": [paragraph(json!([text(words)]))]});
        let adf = doc(json!([{"type": "table", "content": [
            {"type": "tableRow", "content": [cell("tableHeader", "Field"), cell("tableHeader", "Rule")]},
            {"type": "tableRow", "content": [cell("tableCell", "Plate"), cell("tableCell", "A|B or C")]},
            {"type": "tableRow", "content": [
                {"type": "tableCell", "attrs": {"colspan": 2}, "content": [
                    paragraph(json!([text("Both")])),
                    paragraph(json!([text("columns")])),
                ]},
            ]},
        ]}]));
        assert_eq!(
            markdown(&adf),
            "| Field | Rule |\n\
             | --- | --- |\n\
             | Plate | A\\|B or C |\n\
             | Both columns |  |"
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
    fn text_that_would_read_as_markdown_is_escaped() {
        let adf = doc(json!([
            paragraph(json!([text("# Not a heading")])),
            paragraph(json!([text(
                "1. Not a list, *not emphasis*, __nor__ `code`"
            )])),
            paragraph(json!([text(
                "> Not a quote, [not](a link) & not &amp; <b>bold</b>"
            )])),
        ]));
        assert_eq!(
            markdown(&adf),
            "\\# Not a heading\n\
             \n\
             1\\. Not a list, \\*not emphasis\\*, \\_\\_nor\\_\\_ \\`code\\`\n\
             \n\
             \\> Not a quote, [not\\](a link) & not \\&amp; \\<b>bold\\</b>"
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
            text("C:\\"),
            {"type": "hardBreak"},
            text("next"),
        ]))]));
        assert_eq!(markdown(&adf), "C:\\\\\\\nnext");
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
    fn emphasis_characters_are_escaped_only_where_they_could_emphasise() {
        let adf = doc(json!([paragraph(json!([text(
            "_private, a_b_c, *args, 2*3, 2 * 3 and __dunder__"
        )]))]));
        assert_eq!(
            markdown(&adf),
            "\\_private, a_b_c, \\*args, 2\\*3, 2 * 3 and \\_\\_dunder\\_\\_"
        );
    }
}
