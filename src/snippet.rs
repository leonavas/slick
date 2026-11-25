//! Text snippets, as Slack draws and makes them. In a message: the title,
//! type and line count over the code with line numbers, its first lines
//! only until "Show more", which fetches the whole text. In a message box:
//! Ctrl+Shift+Enter (Alt+S where the terminal can't tell it from Enter)
//! opens the dialog that makes one: title, type, content and a message,
//! sent through the attachments' upload (`attach`).

use crate::app::{App, Event, Focus};
use crate::attach::Target;
use crate::compose::{footer, frame, tail};
use crate::files::{self, Actions, Click, Hit, Job, State};
use crate::i18n::tr;
use crate::slack::{File, Res};
use crate::ui::{dim, truncate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::{HashMap, HashSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Slack's snippet types: `snippet_type`, its name and the file's extension.
/// The first is Slack's Auto Detect: no `snippet_type`, Slack tells.
pub const TYPES: [(&str, &str, &str); 26] = [
    ("", "Auto Detect Type", "txt"),
    ("text", "Plain Text", "txt"),
    ("markdown", "Markdown", "md"),
    ("javascript", "JavaScript", "js"),
    ("typescript", "TypeScript", "ts"),
    ("json", "JSON", "json"),
    ("python", "Python", "py"),
    ("shell", "Shell", "sh"),
    ("sql", "SQL", "sql"),
    ("yaml", "YAML", "yaml"),
    ("html", "HTML", "html"),
    ("css", "CSS", "css"),
    ("xml", "XML", "xml"),
    ("java", "Java", "java"),
    ("kotlin", "Kotlin", "kt"),
    ("csharp", "C#", "cs"),
    ("c", "C", "c"),
    ("cpp", "C++", "cpp"),
    ("go", "Go", "go"),
    ("rust", "Rust", "rs"),
    ("php", "PHP", "php"),
    ("ruby", "Ruby", "rb"),
    ("swift", "Swift", "swift"),
    ("dart", "Dart", "dart"),
    ("diff", "Diff", "diff"),
    ("dockerfile", "Dockerfile", "dockerfile"),
];

/// Lines a closed snippet shows.
const COLLAPSED: usize = 5;
/// Lines an open one shows at most; the rest is for Open or Download.
const OPEN_MAX: usize = 500;

/// The file extension for a snippet type.
pub fn ext(kind: &str) -> &'static str {
    TYPES.iter().find(|t| t.0 == kind).map_or("txt", |t| t.2)
}

fn type_label(i: usize) -> &'static str {
    match i {
        0 => tr("Auto Detect Type", "Detectar tipo"),
        1 => tr("Plain Text", "Texto simples"),
        _ => TYPES[i].1,
    }
}

/// Shown as a snippet: Slack made it one, or it's text with a preview.
pub fn is_snippet(f: &File) -> bool {
    f.mode == "snippet"
        || (!matches!(f.mode.as_str(), "post" | "space" | "external" | "tombstone")
            && !f.preview.is_empty()
            && f.thumb.is_none()
            && !f.mimetype.starts_with("image/"))
}

/// Snippets in messages: which are open, and their whole text once fetched
/// (`None` while it comes).
#[derive(Default)]
pub struct Views {
    open: HashSet<String>,
    text: HashMap<String, Option<String>>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum What {
    /// Show more / Show less.
    Toggle,
    Copy,
}

/// A click on a snippet's Show more, Show less or Copy.
#[derive(Clone)]
pub struct Act {
    pub what: What,
    pub id: String,
    pub url: Option<String>,
    pub html_ok: bool,
    /// The whole text, when the preview already holds all of it.
    pub whole: Option<String>,
}

/// The whole text came, for `then`.
pub struct Ev {
    id: String,
    then: What,
    res: Res<String>,
}

/// The text as lines: one kind of line break, none at the end.
fn clean(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").trim_end_matches('\n').to_string()
}

/// Like Slack's snippet: the icon and title, type and line count; the code
/// with line numbers; then Show more (or less), Copy and the file's
/// download. `line` is where it starts in the message list.
pub fn view(f: &File, views: &Views, state: Option<&State>, indent: &str, width: usize, line: usize) -> (Vec<Line<'static>>, Vec<Hit>) {
    let (glyph, color) = files::icon(f);
    let full = views.text.get(&f.id).and_then(|t| t.as_deref()).map(clean);
    let loading = matches!(views.text.get(&f.id), Some(None));
    let open = views.open.contains(&f.id);
    let preview = clean(&f.preview);
    let count = |s: &str| if s.is_empty() { 0 } else { s.split('\n').count() };
    let total = (f.lines as usize).max(count(&preview) + f.lines_more as usize).max(full.as_deref().map_or(0, count));
    // The preview is the whole text: nothing to fetch.
    let whole = full.clone().or_else(|| (!preview.is_empty() && count(&preview) >= total).then(|| preview.clone()));
    let src = if open { whole.as_deref().unwrap_or(&preview) } else { &preview };
    let all: Vec<&str> = if src.is_empty() { Vec::new() } else { src.split('\n').collect() };
    let shown = all.len().min(if open { OPEN_MAX } else { COLLAPSED });

    // Title · type · lines
    let title = if f.title.is_empty() { &f.name } else { &f.title };
    let kind = if !f.pretty_type.is_empty() { f.pretty_type.clone() } else { f.filetype.to_uppercase() };
    let lines_label = match total {
        0 => String::new(),
        1 => tr("1 line", "1 linha").to_string(),
        n => trf!("{n} lines", "{n} linhas"),
    };
    let meta = [kind, lines_label].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let shown_title = truncate(title, width.saturating_sub(3 + 2 + meta.width()).max(8));
    let saved = match state {
        Some(State::Saved(p)) => Some(p.clone()),
        _ => None,
    };
    let mut hits = vec![Hit { line, x: indent.width() as u16, w: 3 + shown_title.width() as u16, click: Click::Menu(Job::file(f), saved) }];
    let mut out = vec![Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(glyph, Style::new().fg(color)),
        Span::raw("  "),
        Span::styled(shown_title, Style::new().bold()),
        Span::styled(format!("  {meta}"), dim()),
    ])];

    // The code, numbered.
    let digits = total.max(shown).max(1).to_string().len();
    let room = width.saturating_sub(3 + digits + 3);
    for (i, l) in all.iter().take(shown).enumerate() {
        out.push(Line::from(vec![
            Span::raw(format!("{indent}   ")),
            Span::styled(format!("{:>digits$}", i + 1), dim()),
            Span::styled(" │ ", dim()),
            Span::raw(truncate(&l.replace('\t', "    "), room)),
        ]));
    }
    if open && all.len() > shown {
        let n = all.len() - shown;
        out.push(Line::from(Span::styled(format!("{indent}   {}", trf!("… {n} more lines", "… mais {n} linhas")), dim().italic())));
    }

    // Show more · Copy · Download
    let act = |what| {
        Click::Snippet(Act { what, id: f.id.clone(), url: f.full.clone().or_else(|| f.download.clone()), html_ok: f.filetype == "html", whole: whole.clone() })
    };
    let mut row = Actions::new(vec![Span::raw(format!("{indent}   "))], line + out.len(), &mut hits);
    let mut gap = "";
    if loading {
        row.push(gap, tr("Loading…", "Carregando…"), dim().italic(), None);
        gap = " · ";
    } else if !open && (total > shown || whole.is_none()) {
        let label = match total.saturating_sub(shown) {
            0 => tr("Show more", "Mostrar mais").to_string(),
            1 => tr("Show more (1 line)", "Mostrar mais (1 linha)").to_string(),
            n => trf!("Show more ({n} lines)", "Mostrar mais ({n} linhas)"),
        };
        row.link(gap, &label, act(What::Toggle));
        gap = " · ";
    } else if open && total > COLLAPSED {
        row.link(gap, tr("Show less", "Mostrar menos"), act(What::Toggle));
        gap = " · ";
    }
    row.link(gap, tr("Copy", "Copiar"), act(What::Copy));
    row.download(f, state, " · ");
    let spans = row.spans;
    out.push(Line::from(spans));
    (out, hits)
}

impl App {
    /// Show more / less and Copy on a snippet; the whole text is fetched
    /// first when the preview doesn't hold it.
    pub(crate) fn snippet_click(&mut self, act: Act) {
        let v = &mut self.attach.snippets;
        let have = act.whole.clone().or_else(|| v.text.get(&act.id).cloned().flatten());
        match (act.what, have) {
            (What::Toggle, _) if v.open.remove(&act.id) => {}
            (What::Toggle, Some(_)) => {
                v.open.insert(act.id);
            }
            (What::Copy, Some(text)) => self.copy(&clean(&text)),
            (then, None) => {
                if matches!(v.text.get(&act.id), Some(None)) {
                    return;
                }
                let Some(url) = act.url else {
                    self.error = Some(tr("The snippet can't be loaded", "O snippet não pode ser carregado").into());
                    return;
                };
                v.text.insert(act.id.clone(), None);
                let (id, html_ok) = (act.id, act.html_ok);
                self.spawn(move |s| {
                    let mut buf = Vec::new();
                    let res = s.download_to(&url, &mut buf, html_ok, &|_| {}).map(|_| String::from_utf8_lossy(&buf).into_owned());
                    Event::Attach(crate::attach::Ev::Snippet(Ev { id, then, res }))
                });
            }
        }
        self.dirty = true;
    }

    pub(crate) fn on_snippet(&mut self, ev: Ev) {
        match ev.res {
            Ok(text) => {
                self.attach.snippets.text.insert(ev.id.clone(), Some(text.clone()));
                match ev.then {
                    What::Toggle => {
                        self.attach.snippets.open.insert(ev.id);
                    }
                    What::Copy => self.copy(&clean(&text)),
                }
            }
            Err(e) => {
                self.attach.snippets.text.remove(&ev.id);
                self.error = Some(trf!("Couldn't load the snippet: {e}", "Não deu para carregar o snippet: {e}"));
            }
        }
    }

    /// Ctrl+Shift+Enter (or Alt+S) in a message box: the snippet dialog,
    /// sending to that box. True when it opened.
    pub(crate) fn snippet_key(&mut self, k: KeyEvent) -> bool {
        let m = k.modifiers;
        let (ctrl, shift, alt) = (m.contains(KeyModifiers::CONTROL), m.contains(KeyModifiers::SHIFT), m.contains(KeyModifiers::ALT));
        let hit = (k.code == KeyCode::Enter && ctrl && shift) || (alt && !ctrl && matches!(k.code, KeyCode::Char('s' | 'S')));
        if !hit {
            return false;
        }
        let thread = match self.focus {
            Focus::Input => false,
            Focus::Thread => true,
            _ => return false,
        };
        if !thread && self.read_only().is_some() {
            return false;
        }
        let Some(to) = self.target(thread) else { return false };
        self.dialog = Some(crate::compose::Dialog::Snippet(Box::new(Dialog::new(to))));
        true
    }

    pub(crate) fn snippet_dialog_key(&mut self, k: KeyEvent, ctrl: bool) {
        let Some(crate::compose::Dialog::Snippet(d)) = self.dialog.as_mut() else { return };
        if (ctrl && matches!(k.code, KeyCode::Enter | KeyCode::Char('s' | 'j'))) || (k.code == KeyCode::Enter && d.field == MESSAGE) {
            return self.submit_snippet();
        }
        d.key(k, ctrl);
    }

    pub(crate) fn snippet_dialog_mouse(&mut self, m: MouseEvent, field: Option<usize>, confirm: bool) {
        let Some(crate::compose::Dialog::Snippet(d)) = self.dialog.as_mut() else { return };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if confirm => self.submit_snippet(),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(f) = field {
                    if f == KIND && d.field == KIND {
                        d.kind = (d.kind + 1) % TYPES.len();
                    }
                    d.field = f;
                }
            }
            MouseEventKind::ScrollUp if d.field == CONTENT => d.move_rows(-3),
            MouseEventKind::ScrollDown if d.field == CONTENT => d.move_rows(3),
            _ => {}
        }
    }

    fn submit_snippet(&mut self) {
        let Some(crate::compose::Dialog::Snippet(d)) = self.dialog.as_mut() else { return };
        let content = d.lines.join("\n");
        if content.trim().is_empty() {
            d.error = Some(tr("Type or paste the snippet's content.", "Digite ou cole o conteúdo do snippet.").into());
            d.field = CONTENT;
            return;
        }
        let (to, title, kind, comment) = (d.to.clone(), d.title.trim().to_string(), TYPES[d.kind].0.to_string(), d.comment.trim().to_string());
        self.dialog = None;
        self.send_snippet(to, title, kind, content, comment);
    }
}

// ---- the dialog --------------------------------------------------------------

const TITLE: usize = 0;
const KIND: usize = 1;
const CONTENT: usize = 2;
const MESSAGE: usize = 3;
const FIELDS: usize = 4;
/// Label column.
const LABEL_W: usize = 10;

/// Slack's "Create snippet".
pub struct Dialog {
    to: Target,
    title: String,
    /// Into `TYPES`.
    kind: usize,
    /// The content, a line each; the cursor at `row`, `col` (chars).
    lines: Vec<String>,
    row: usize,
    col: usize,
    /// The content's first line on screen.
    top: usize,
    comment: String,
    field: usize,
    error: Option<String>,
}

impl Dialog {
    fn new(to: Target) -> Self {
        Self { to, title: String::new(), kind: 0, lines: vec![String::new()], row: 0, col: 0, top: 0, comment: String::new(), field: CONTENT, error: None }
    }

    fn byte(&self) -> usize {
        let l = &self.lines[self.row];
        l.char_indices().nth(self.col).map_or(l.len(), |(i, _)| i)
    }

    fn line_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            let b = self.byte();
            self.lines[self.row].insert_str(b, part);
            self.col += part.chars().count();
        }
    }

    fn newline(&mut self) {
        let b = self.byte();
        let rest = self.lines[self.row].split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let b = self.byte();
            self.lines[self.row].remove(b);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let b = self.byte();
            self.lines[self.row].remove(b);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_rows(&mut self, delta: isize) {
        self.row = self.row.saturating_add_signed(delta).min(self.lines.len() - 1);
        self.col = self.col.min(self.line_len());
    }

    /// The one-line field being typed in, if it is one.
    fn text(&mut self) -> Option<&mut String> {
        match self.field {
            TITLE => Some(&mut self.title),
            MESSAGE => Some(&mut self.comment),
            _ => None,
        }
    }

    fn paste(&mut self, s: &str) {
        match self.field {
            CONTENT => self.insert(s),
            KIND => {}
            _ => {
                let line = s.lines().next().unwrap_or("").to_string();
                if let Some(t) = self.text() {
                    t.push_str(&line);
                }
            }
        }
        self.error = None;
    }

    fn key(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Tab => self.field = (self.field + 1) % FIELDS,
            KeyCode::BackTab => self.field = (self.field + FIELDS - 1) % FIELDS,
            _ if self.field == CONTENT => self.content_key(k, ctrl),
            KeyCode::Enter | KeyCode::Down => self.field = (self.field + 1).min(MESSAGE),
            KeyCode::Up => self.field = self.field.saturating_sub(1),
            KeyCode::Left if self.field == KIND => self.kind = (self.kind + TYPES.len() - 1) % TYPES.len(),
            KeyCode::Right | KeyCode::Char(' ') if self.field == KIND => self.kind = (self.kind + 1) % TYPES.len(),
            // A letter: the next type it starts.
            KeyCode::Char(c) if self.field == KIND && !ctrl => {
                let c = c.to_ascii_lowercase();
                let n = TYPES.len();
                if let Some(i) = (1..=n).map(|d| (self.kind + d) % n).find(|&i| type_label(i).to_lowercase().starts_with(c)) {
                    self.kind = i;
                }
            }
            KeyCode::Backspace => {
                if let Some(t) = self.text() {
                    t.pop();
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(t) = self.text() {
                    t.clear();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(t) = self.text() {
                    t.push(c);
                }
            }
            _ => return,
        }
        self.error = None;
    }

    fn content_key(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Enter => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if self.col > 0 => self.col -= 1,
            KeyCode::Left if self.row > 0 => {
                self.row -= 1;
                self.col = self.line_len();
            }
            KeyCode::Right if self.col < self.line_len() => self.col += 1,
            KeyCode::Right if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = 0;
            }
            KeyCode::Up if self.row == 0 => self.field = KIND,
            KeyCode::Down if self.row + 1 == self.lines.len() => self.field = MESSAGE,
            KeyCode::Up => self.move_rows(-1),
            KeyCode::Down => self.move_rows(1),
            KeyCode::PageUp => self.move_rows(-10),
            KeyCode::PageDown => self.move_rows(10),
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.line_len(),
            KeyCode::Char('u') if ctrl => {
                (self.lines, self.row, self.col, self.top) = (vec![String::new()], 0, 0, 0);
            }
            KeyCode::Char(c) if !ctrl => {
                let mut b = [0; 4];
                self.insert(c.encode_utf8(&mut b));
            }
            _ => return,
        }
        self.error = None;
    }
}

impl App {
    pub(crate) fn snippet_paste(&mut self, s: &str) {
        if let Some(crate::compose::Dialog::Snippet(d)) = self.dialog.as_mut() {
            d.paste(s);
        }
    }
}

/// The dialog: title, type and message on a line each, the content in
/// between with line numbers, kept scrolled to the cursor.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let inner = frame(f, app, tr("Create snippet", "Criar snippet"), 78.min(area.width.saturating_sub(4)), 30.min(area.height.saturating_sub(2)));
    if inner.width < 30 || inner.height < 14 {
        return;
    }
    let Some(crate::compose::Dialog::Snippet(d)) = app.dialog.as_mut() else { return };
    let width = inner.width as usize;
    let room = width.saturating_sub(LABEL_W + 3);
    let field = d.field;
    let marker = |i: usize| if i == field { Span::styled("› ", Style::new().fg(Color::Cyan).bold()) } else { Span::raw("  ") };
    let label = |s: &str| Span::styled(format!("{s:<w$}", w = LABEL_W), Style::new().bold());
    let value = |s: &str, placeholder: &str| {
        if s.is_empty() { Span::styled(placeholder.to_string(), dim()) } else { Span::raw(tail(s, room)) }
    };
    let mut fields = Vec::new();
    let mut cursor = None;

    // Title, type.
    let placeholder = format!("snippet.{}", ext(TYPES[d.kind].0));
    let title_line = Line::from(vec![marker(TITLE), label(tr("Title", "Título")), value(&d.title, &placeholder)]);
    let kind_style = if field == KIND { Style::new().fg(Color::Cyan) } else { Style::new() };
    let kind_line = Line::from(vec![
        marker(KIND),
        label(tr("Type", "Tipo")),
        Span::styled("‹ ", dim()),
        Span::styled(type_label(d.kind), kind_style),
        Span::styled(" ›", dim()),
    ]);
    f.render_widget(Paragraph::new(vec![title_line, Line::from(""), kind_line]), Rect::new(inner.x, inner.y, inner.width, 3));
    fields.push((Rect::new(inner.x, inner.y, inner.width, 1), TITLE));
    fields.push((Rect::new(inner.x, inner.y + 2, inner.width, 1), KIND));
    if field == TITLE {
        cursor = Some((inner.x + (LABEL_W + 2 + tail(&d.title, room).width()) as u16, inner.y));
    }

    // Content, numbered, with the cursor's line in view.
    let y0 = inner.y + 4;
    f.render_widget(Paragraph::new(Line::from(vec![marker(CONTENT), label(tr("Content", "Conteúdo"))])), Rect::new(inner.x, y0, inner.width, 1));
    let box_h = inner.bottom().saturating_sub(y0 + 1 + 5) as usize;
    let box_h = box_h.max(1);
    if d.row < d.top {
        d.top = d.row;
    } else if d.row >= d.top + box_h {
        d.top = d.row + 1 - box_h;
    }
    let digits = d.lines.len().to_string().len().max(2);
    let code_w = width.saturating_sub(2 + digits + 3).max(1);
    let mut lines = Vec::new();
    for (i, l) in d.lines.iter().enumerate().skip(d.top).take(box_h) {
        let chars: Vec<char> = l.chars().collect();
        // The cursor's line scrolls sideways to keep it in view.
        let skip = if i == d.row {
            let mut skip = 0;
            while chars[skip..d.col.min(chars.len())].iter().map(|c| c.width().unwrap_or(0)).sum::<usize>() >= code_w {
                skip += 1;
            }
            skip
        } else {
            0
        };
        let shown: String = chars[skip.min(chars.len())..].iter().collect();
        if i == d.row && field == CONTENT {
            let w: usize = chars[skip.min(chars.len())..d.col.min(chars.len())].iter().map(|c| c.width().unwrap_or(0)).sum();
            cursor = Some((inner.x + (2 + digits + 3 + w) as u16, y0 + 1 + (i - d.top) as u16));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:>digits$}", i + 1), dim()),
            Span::styled(" │ ", dim()),
            Span::raw(truncate(&shown, code_w)),
        ]));
    }
    if d.lines.len() == 1 && d.lines[0].is_empty() && field != CONTENT {
        lines[0].spans.push(Span::styled(tr("paste or type the text", "cole ou digite o texto"), dim()));
    }
    let content_rect = Rect::new(inner.x, y0 + 1, inner.width, box_h as u16);
    f.render_widget(Paragraph::new(lines), content_rect);
    fields.push((Rect::new(inner.x, y0, inner.width, box_h as u16 + 1), CONTENT));

    // Message.
    let ym = y0 + 1 + box_h as u16 + 1;
    let message = Line::from(vec![marker(MESSAGE), label(tr("Message", "Mensagem")), value(&d.comment, tr("(optional)", "(opcional)"))]);
    f.render_widget(Paragraph::new(message), Rect::new(inner.x, ym, inner.width, 1));
    fields.push((Rect::new(inner.x, ym, inner.width, 1), MESSAGE));
    if field == MESSAGE {
        cursor = Some((inner.x + (LABEL_W + 2 + tail(&d.comment, room).width()) as u16, ym));
    }

    let ready = d.lines.iter().any(|l| !l.trim().is_empty());
    let error = d.error.clone();
    if let Some(c) = cursor {
        f.set_cursor_position(c);
    }
    app.areas.compose.fields = fields;
    let hint = tr(
        "Tab fields · ←→ type · Ctrl+Enter creates · Esc closes",
        "Tab campos · ←→ tipo · Ctrl+Enter cria · Esc fecha",
    );
    footer(f, app, inner, error.as_deref(), hint, tr(" Create snippet ", " Criar snippet "), ready);
}
