//! Files sent with a message, like Slack's attachments: pictures pasted into
//! a message box and snippets from the snippet dialog. Terminals don't
//! forward pasted images, so the clipboard is read directly: wl-paste or
//! xclip on Linux, arboard elsewhere. Each card shows its upload's progress,
//! and its ✕ cancels the upload until Slack starts posting the message.

use crate::app::{App, Event, Focus, Img, ImgRef};
use crate::editor::Editor;
use crate::files::{bar, Progress};
use crate::i18n::tr;
use crate::slack::{escape, Res, Slack};
use crate::ui::{dim, draw_image, truncate};
use crossterm::event::{KeyCode, KeyEvent};
use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;
use std::io::Read;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// The card: a few rows of picture in a rounded border.
pub const CARD_ROWS: u16 = 6;
const PIC_ROWS: u16 = CARD_ROWS - 2;
/// A snippet's card: room for its title and first lines.
const SNIPPET_COLS: u16 = 24;
/// Slack takes up to 10 files per message.
const MAX: usize = 10;
/// Bytes per write while uploading: the bar moves, and a cancel lands, between them.
const SLICE: usize = 64 << 10;
/// Wakes the screen this often at most while bytes go up.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

// Where an upload is: its bytes going up, Slack posting the message (too
// late to cancel), or cancelled.
const UPLOADING: u8 = 0;
const POSTING: u8 = 1;
const CANCELLED: u8 = 2;

/// Where a box sends: the conversation, or a thread in it.
#[derive(Clone, PartialEq)]
pub struct Target {
    pub chan: String,
    pub thread: Option<String>,
}

pub struct Attachment {
    /// Also its key in `App::images`, where the preview lives.
    pub key: String,
    pub name: String,
    pub bytes: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
    /// A snippet typed in: its title and Slack's `snippet_type` ("" lets Slack tell).
    pub snippet: Option<(String, String)>,
    /// On its way with the message, in the upload with this id: kept until
    /// Slack has it.
    pub sending: Option<u64>,
    /// How far its bytes got.
    pub progress: Arc<Progress>,
}

/// Files going up together, with the text they go with.
struct Upload {
    id: u64,
    to: Target,
    keys: Vec<String>,
    text: String,
    phase: Arc<AtomicU8>,
}

/// Each box keeps its own files, so they wait in their conversation or
/// thread like a draft.
#[derive(Default)]
pub struct Attach {
    items: Vec<(Target, Attachment)>,
    /// Each card's ✕ on screen and the file it removes (or whose upload it cancels).
    pub close: Vec<(Rect, String)>,
    reading: bool,
    next: u64,
    /// The status line set while uploading, cleared when it ends.
    status: Option<String>,
    uploads: Vec<Upload>,
    /// Snippets in messages: which are open, and their whole text.
    pub snippets: crate::snippet::Views,
}

impl Attach {
    fn of<'a>(&'a self, to: &'a Target) -> impl Iterator<Item = &'a Attachment> + 'a {
        self.items.iter().filter(move |(t, _)| t == to).map(|(_, a)| a)
    }

    fn phase(&self, id: u64) -> Option<u8> {
        self.uploads.iter().find(|u| u.id == id).map(|u| u.phase.load(Ordering::SeqCst))
    }
}

pub enum Clip {
    Image { bytes: Vec<u8>, ext: &'static str, thumb: DynamicImage, w: u32, h: u32 },
    Text(String),
    Empty,
}

pub enum Ev {
    Clip { to: Target, key: String, res: Res<Clip> },
    Sent { id: u64, res: Res<()> },
    /// A snippet's whole text, fetched for a message (`snippet`).
    Snippet(crate::snippet::Ev),
}

/// A file for `Slack::upload`.
pub struct Up {
    pub name: String,
    /// Shown instead of the name; empty for the name.
    pub title: String,
    /// Slack's `snippet_type`, to show it as code; empty for any other file.
    pub snippet_type: String,
    pub bytes: Arc<Vec<u8>>,
    pub progress: Arc<Progress>,
}

/// A file's bytes a slice at a time, counted into its progress; stops when
/// the upload is cancelled.
struct Slices<'a> {
    bytes: &'a [u8],
    pos: usize,
    progress: &'a Progress,
    phase: &'a AtomicU8,
    tick: &'a (dyn Fn() + Send + Sync),
    last: Instant,
}

impl Read for Slices<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.phase.load(Ordering::SeqCst) == CANCELLED {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = buf.len().min(SLICE).min(self.bytes.len() - self.pos);
        buf[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        self.pos += n;
        self.progress.done.store(self.pos as u64, Ordering::Relaxed);
        if self.last.elapsed() >= PROGRESS_EVERY {
            self.last = Instant::now();
            (self.tick)();
        }
        Ok(n)
    }
}

fn cancelled() -> String {
    tr("upload cancelled", "envio cancelado").to_string()
}

impl Slack {
    /// Slack's upload flow: an address per file, the bytes there (a slice at
    /// a time, into each file's progress), then one message holding all of
    /// them with the text as its comment. `phase` stops it while the bytes go
    /// up; once Slack posts the message it can no longer be cancelled.
    pub fn upload(&self, chan: &str, thread: Option<&str>, text: &str, files: &[Up], phase: &AtomicU8, tick: &(dyn Fn() + Send + Sync)) -> Res<()> {
        // The upload address is presigned: no credentials, but room for big files.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(60))
            .timeout_write(Duration::from_secs(120))
            .user_agent(concat!("slick/", env!("CARGO_PKG_VERSION")))
            .build();
        let mut done = Vec::new();
        for f in files {
            if phase.load(Ordering::SeqCst) == CANCELLED {
                return Err(cancelled());
            }
            let (name, len) = (f.name.as_str(), f.bytes.len().to_string());
            let mut p = vec![("filename", name), ("length", len.as_str())];
            if !f.snippet_type.is_empty() {
                p.push(("snippet_type", f.snippet_type.as_str()));
            }
            let v = self.call("files.getUploadURLExternal", &p)?;
            let (Some(url), Some(id)) = (v["upload_url"].as_str(), v["file_id"].as_str()) else {
                return Err("files.getUploadURLExternal: no upload_url".into());
            };
            f.progress.total.store(f.bytes.len() as u64, Ordering::Relaxed);
            let body = Slices { bytes: &f.bytes, pos: 0, progress: &f.progress, phase, tick, last: Instant::now() };
            // A known length: sent as is, not chunked.
            let sent = agent.post(url).set("Content-Type", "application/octet-stream").set("Content-Length", &len).send(body);
            if phase.load(Ordering::SeqCst) == CANCELLED {
                return Err(cancelled());
            }
            sent.map_err(|e| trf!("upload {name}: {e}", "envio de {name}: {e}"))?;
            let title = if f.title.is_empty() { name } else { f.title.as_str() };
            done.push(serde_json::json!({ "id": id, "title": title }));
        }
        // From here Slack posts it: too late to take back.
        if phase.compare_exchange(UPLOADING, POSTING, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return Err(cancelled());
        }
        tick();
        let files = serde_json::Value::Array(done).to_string();
        let comment = escape(text);
        let mut p = vec![("files", files.as_str()), ("channel_id", chan)];
        if !comment.is_empty() {
            p.push(("initial_comment", comment.as_str()));
        }
        if let Some(ts) = thread {
            p.push(("thread_ts", ts));
        }
        self.call("files.completeUploadExternal", &p).map(|_| ())
    }
}

impl App {
    /// What the conversation's box (or the thread's) sends to.
    pub(crate) fn target(&self, thread: bool) -> Option<Target> {
        if thread {
            let t = self.thread.as_ref()?;
            Some(Target { chan: t.chan.clone(), thread: Some(t.ts.clone()) })
        } else {
            Some(Target { chan: self.current.clone()?, thread: None })
        }
    }

    fn editor_for(&mut self, to: &Target) -> Option<&mut Editor> {
        match &to.thread {
            None => (self.current.as_deref() == Some(to.chan.as_str())).then_some(&mut self.editor),
            Some(ts) => self.thread.as_mut().filter(|t| t.chan == to.chan && t.ts == *ts).map(|t| &mut t.editor),
        }
    }

    /// Ctrl+V, or a paste with nothing in it: a picture on the clipboard is
    /// attached, text is typed in. Read off the UI thread.
    pub(crate) fn paste_clipboard(&mut self, thread: bool) {
        let Some(to) = self.target(thread) else { return };
        if std::mem::replace(&mut self.attach.reading, true) {
            return;
        }
        self.attach.next += 1;
        let key = format!("paste-{}-{}", std::process::id(), self.attach.next);
        self.spawn(move |_| Event::Attach(Ev::Clip { to, key, res: read_clipboard() }));
    }

    /// Backspace in an empty box: the last file goes. True when one did.
    pub(crate) fn drop_last(&mut self, thread: bool) -> bool {
        let Some(to) = self.target(thread) else { return false };
        let Some(key) = self.attach.of(&to).filter(|a| a.sending.is_none()).last().map(|a| a.key.clone()) else { return false };
        self.forget(&key);
        true
    }

    fn forget(&mut self, key: &str) {
        self.attach.items.retain(|(_, a)| a.key != key);
        self.images.remove(key);
        self.protocols.retain(|(k, _, _), _| k != key);
    }

    /// Enter with files attached: they go up with the text as their
    /// comment. False when there are none, for the plain send.
    pub(crate) fn send_files(&mut self, thread: bool) -> bool {
        let Some(to) = self.target(thread) else { return false };
        let keys: Vec<String> = self.attach.of(&to).filter(|a| a.sending.is_none()).map(|a| a.key.clone()).collect();
        if keys.is_empty() {
            // Still going: Enter waits for them instead of sending the text alone.
            return self.attach.of(&to).next().is_some();
        }
        let text = self.editor_for(&to).map(|e| e.take().trim().to_string()).unwrap_or_default();
        self.start_upload(to, keys, text);
        true
    }

    /// The snippet dialog's Create: the text goes up as a file with Slack's
    /// `snippet_type`, its card in the box while it does.
    pub(crate) fn send_snippet(&mut self, to: Target, title: String, kind: String, content: String, comment: String) {
        if self.attach.of(&to).count() >= MAX {
            self.error = Some(trf!("Up to {MAX} files per message", "No máximo {MAX} arquivos por mensagem"));
            return;
        }
        self.attach.next += 1;
        let key = format!("snippet-{}-{}", std::process::id(), self.attach.next);
        let ext = crate::snippet::ext(&kind);
        let name = match title.as_str() {
            "" => format!("snippet.{ext}"),
            t if t.contains('.') => t.to_string(),
            t => format!("{t}.{ext}"),
        };
        let a = Attachment {
            key: key.clone(),
            name,
            bytes: Arc::new(content.into_bytes()),
            w: 0,
            h: 0,
            snippet: Some((title, kind)),
            sending: None,
            progress: Default::default(),
        };
        self.attach.items.push((to.clone(), a));
        self.start_upload(to, vec![key], comment);
    }

    /// Sends the box's files `keys`, each card following its bytes up.
    fn start_upload(&mut self, to: Target, keys: Vec<String>, text: String) {
        self.attach.next += 1;
        let id = self.attach.next;
        let mut files = Vec::new();
        for (_, a) in self.attach.items.iter_mut().filter(|(t, a)| *t == to && keys.contains(&a.key)) {
            a.sending = Some(id);
            a.progress = Default::default();
            let (title, snippet_type) = a.snippet.clone().unwrap_or_default();
            files.push(Up { name: a.name.clone(), title, snippet_type, bytes: a.bytes.clone(), progress: a.progress.clone() });
        }
        match self.thread.as_mut().filter(|_| to.thread.is_some()) {
            Some(t) => t.scroll = 0,
            None => self.to_present(),
        }
        let snippets = self.attach.of(&to).filter(|a| a.sending == Some(id) && a.snippet.is_some()).count();
        let status = match (files.len(), snippets) {
            (1, 1) => tr("Sending the snippet…", "Enviando o snippet…").to_string(),
            (1, _) => tr("Sending the image…", "Enviando a imagem…").to_string(),
            (n, 0) => trf!("Sending {n} images…", "Enviando {n} imagens…"),
            (n, _) => trf!("Sending {n} files…", "Enviando {n} arquivos…"),
        };
        self.status = status.clone();
        self.attach.status = Some(status);
        let phase = Arc::new(AtomicU8::new(UPLOADING));
        self.attach.uploads.push(Upload { id, to: to.clone(), keys, text: text.clone(), phase: phase.clone() });
        let tx = self.tx.clone();
        self.spawn(move |s| {
            let tick = move || {
                let _ = tx.send(Event::Progress);
            };
            let res = s.upload(&to.chan, to.thread.as_deref(), &text, &files, &phase, &tick);
            Event::Attach(Ev::Sent { id, res })
        });
    }

    /// The status line set for an upload goes with it, and its files are
    /// no longer on their way.
    fn end_upload(&mut self, up: &Upload) {
        if self.attach.status.take().is_some_and(|s| s == self.status) {
            self.status.clear();
        }
        for (_, a) in self.attach.items.iter_mut().filter(|(_, a)| a.sending == Some(up.id)) {
            a.sending = None;
        }
    }

    /// Stops an upload before Slack posts it: its files wait in the box
    /// again and the text goes back. False when it was too late.
    fn cancel_upload(&mut self, id: u64) -> bool {
        let Some(i) = self.attach.uploads.iter().position(|u| u.id == id) else { return false };
        if self.attach.uploads[i].phase.compare_exchange(UPLOADING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            self.brief(tr("Too late to cancel: Slack is posting it", "Tarde para cancelar: o Slack já está postando").into(), 3);
            return false;
        }
        let up = self.attach.uploads.remove(i);
        self.end_upload(&up);
        self.restore(&up.to, &up.text);
        self.brief(tr("Upload cancelled", "Envio cancelado").into(), 3);
        self.dirty = true;
        true
    }

    /// The text of a message that didn't go, back in its box, before
    /// whatever was typed since.
    fn restore(&mut self, to: &Target, text: &str) {
        if text.is_empty() {
            return;
        }
        let look = crate::editor::Lookup { users: &self.users, chans: &self.chans, emoji: &self.emoji, pictures: self.picker.is_some() };
        let ed = match &to.thread {
            None => (self.current.as_deref() == Some(to.chan.as_str())).then_some(&mut self.editor),
            Some(ts) => self.thread.as_mut().filter(|t| t.chan == to.chan && t.ts == *ts).map(|t| &mut t.editor),
        };
        if let Some(ed) = ed {
            let typed = ed.wire();
            let all = if typed.trim().is_empty() { text.to_string() } else { format!("{text}\n{typed}") };
            ed.load(&all, false, &look);
        }
    }

    /// Esc in a box whose files are going up: cancels them. True when it did.
    pub(crate) fn upload_key(&mut self, k: KeyEvent) -> bool {
        if k.code != KeyCode::Esc || !k.modifiers.is_empty() {
            return false;
        }
        let thread = match self.focus {
            Focus::Input => false,
            Focus::Thread => true,
            _ => return false,
        };
        let Some(to) = self.target(thread) else { return false };
        let up = self.attach.uploads.iter().rev().find(|u| u.to == to && u.phase.load(Ordering::SeqCst) == UPLOADING);
        match up.map(|u| u.id) {
            Some(id) => self.cancel_upload(id),
            None => false,
        }
    }

    pub(crate) fn on_attach(&mut self, ev: Ev) {
        match ev {
            Ev::Clip { to, key, res } => {
                self.attach.reading = false;
                match res {
                    Ok(Clip::Empty) => self.brief(tr("Nothing to paste", "Nada para colar").into(), 3),
                    Ok(Clip::Text(s)) => {
                        if let Some(e) = self.editor_for(&to) {
                            e.insert(&s);
                        }
                    }
                    Ok(Clip::Image { .. }) if self.attach.of(&to).count() >= MAX => {
                        self.error = Some(trf!("Up to {MAX} images per message", "No máximo {MAX} imagens por mensagem"));
                    }
                    Ok(Clip::Image { bytes, ext, thumb, w, h }) => {
                        self.images.insert(key.clone(), Img::Ready(thumb));
                        let name = format!("image.{ext}");
                        let a = Attachment { key, name, bytes: Arc::new(bytes), w, h, snippet: None, sending: None, progress: Default::default() };
                        self.attach.items.push((to, a));
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Ev::Sent { id, res } => {
                // Cancelled: already back in the box.
                let Some(i) = self.attach.uploads.iter().position(|u| u.id == id) else { return };
                let up = self.attach.uploads.remove(i);
                self.end_upload(&up);
                match res {
                    Ok(()) => {
                        up.keys.iter().for_each(|k| self.forget(k));
                        // The message shows once Slack has it; don't wait for the next poll.
                        match &up.to.thread {
                            Some(_) => self.refresh_thread(),
                            None if self.current.as_deref() == Some(up.to.chan.as_str()) => {
                                self.fetch(up.to.chan.clone(), crate::app::Fetch::Poll)
                            }
                            None => {}
                        }
                    }
                    Err(e) => {
                        self.error = Some(e);
                        self.restore(&up.to, &up.text);
                    }
                }
            }
            Ev::Snippet(e) => self.on_snippet(e),
        }
    }

    /// A click on a card's ✕: removes it, or cancels its upload. True when handled.
    pub(crate) fn attach_mouse(&mut self, m: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        let at = ratatui::layout::Position::new(m.column, m.row);
        let Some(key) = self.attach.close.iter().find(|(r, _)| r.contains(at)).map(|(_, k)| k.clone()) else { return false };
        match self.attach.items.iter().find(|(_, a)| a.key == key).and_then(|(_, a)| a.sending) {
            Some(id) => {
                self.cancel_upload(id);
            }
            None => self.forget(&key),
        }
        self.dirty = true;
        true
    }
}

/// The rows a box grows by for its cards.
pub fn rows(app: &App, thread: bool) -> u16 {
    let any = app.target(thread).is_some_and(|to| app.attach.of(&to).next().is_some());
    if any { CARD_ROWS } else { 0 }
}

/// A box's inside: the text on top and the cards under it, as in Slack.
pub fn split(app: &App, inner: Rect, thread: bool) -> (Rect, Rect) {
    let h = rows(app, thread);
    if h == 0 || inner.height <= h {
        return (inner, Rect::default());
    }
    (Rect { height: inner.height - h, ..inner }, Rect::new(inner.x, inner.bottom() - h, inner.width, h))
}

/// The status line's keys while a box holds files.
pub fn hint(app: &App) -> Option<String> {
    let thread = match app.focus {
        Focus::Input => false,
        Focus::Thread => true,
        _ => return None,
    };
    let to = app.target(thread)?;
    let phases: Vec<u8> = app.attach.uploads.iter().filter(|u| u.to == to).map(|u| u.phase.load(Ordering::SeqCst)).collect();
    if phases.contains(&UPLOADING) {
        return Some(tr("Uploading… · Esc or ✕ cancels", "Enviando… · Esc ou ✕ cancela").to_string());
    }
    if phases.contains(&POSTING) {
        return Some(tr("Sending…", "Enviando…").to_string());
    }
    (rows(app, thread) > 0).then(|| {
        tr(
            "Enter send · Backspace remove last attachment · Ctrl+V paste another · Esc list",
            "Enter enviar · Backspace remover último anexo · Ctrl+V colar outro · Esc lista",
        )
        .to_string()
    })
}

/// The cards in a row, each a thumbnail (or a snippet's first lines) with a
/// ✕ on its corner; while sending, a progress bar under it. The
/// conversation's box draws first and starts the frame's ✕ list.
pub fn draw(f: &mut Frame, app: &mut App, area: Rect, thread: bool) {
    if !thread {
        app.attach.close.clear();
    }
    let Some(to) = app.target(thread) else { return };
    if area.height < CARD_ROWS {
        return;
    }
    struct Card {
        key: String,
        name: String,
        w: u32,
        h: u32,
        /// A snippet's title and first lines.
        snippet: Option<(String, String)>,
        phase: Option<u8>,
        progress: Arc<Progress>,
    }
    let cards: Vec<Card> = app
        .attach
        .of(&to)
        .map(|a| Card {
            key: a.key.clone(),
            name: a.name.clone(),
            w: a.w,
            h: a.h,
            snippet: a.snippet.as_ref().map(|(t, _)| {
                let lines = String::from_utf8_lossy(&a.bytes).lines().take(PIC_ROWS as usize - 1).collect::<Vec<_>>().join("\n");
                (if t.is_empty() { a.name.clone() } else { t.clone() }, lines)
            }),
            phase: a.sending.map(|id| app.attach.phase(id).unwrap_or(POSTING)),
            progress: a.progress.clone(),
        })
        .collect();
    let (fw, fh) = app.picker.as_ref().map(|p| p.font_size()).map(|s| (s.width as u64, s.height as u64)).unwrap_or((1, 2));
    let mut x = area.x;
    for (i, c) in cards.iter().enumerate() {
        let cols = match c.snippet {
            Some(_) => SNIPPET_COLS,
            None => (c.w as u64 * fh * PIC_ROWS as u64 / (c.h.max(1) as u64 * fw.max(1))).clamp(6, 28) as u16,
        };
        let card = Rect::new(x, area.y, cols + 2, CARD_ROWS);
        if card.right() > area.right() {
            let more = format!("+{}", cards.len() - i);
            if x + more.width() as u16 <= area.right() {
                f.render_widget(Paragraph::new(more).style(dim()), Rect::new(x, area.y + CARD_ROWS / 2, area.right() - x, 1));
            }
            break;
        }
        let mut block = Block::bordered().border_type(BorderType::Rounded).border_style(dim());
        match c.phase {
            Some(UPLOADING) => {
                let label = c.progress.label();
                let cells = (cols as usize).saturating_sub(label.width() + 3).max(1);
                let progress = format!(" {} {label} ", bar(c.progress.fraction().unwrap_or(0.0), cells));
                block = block.title_bottom(Line::from(progress).style(Style::new().fg(ratatui::style::Color::Cyan)));
            }
            Some(_) => block = block.title_bottom(Line::from(tr(" Sending… ", " Enviando… ")).style(dim().italic())),
            None => {}
        }
        let pic = block.inner(card);
        f.render_widget(block, card);
        match &c.snippet {
            Some((title, lines)) => {
                let w = pic.width as usize;
                let mut text = vec![Line::from(truncate(title, w)).style(Style::new().bold())];
                text.extend(lines.lines().map(|l| Line::from(truncate(&l.replace('\t', "  "), w)).style(dim())));
                f.render_widget(Paragraph::new(text), pic);
            }
            None if app.picker.is_some() => {
                draw_image(f, app, &ImgRef { key: c.key.clone(), url: String::new(), name: String::new(), link: None }, pic);
            }
            None => f.render_widget(Paragraph::new(format!("🖼 {}", c.name)).style(dim()), pic),
        }
        // Until Slack posts it, the ✕ takes it out, or stops its upload.
        if matches!(c.phase, None | Some(UPLOADING)) {
            let close = Rect::new(card.right() - 4, card.y, 3, 1);
            f.render_widget(Paragraph::new(" ✕ ").style(Style::new().bold()), close);
            app.attach.close.push((close, c.key.clone()));
        }
        x = card.right() + 1;
    }
}

// ---- the clipboard ---------------------------------------------------------

/// Image types Slack shows inline and we can preview, the usual first.
const TYPES: [(&str, &str); 4] = [("image/png", "png"), ("image/jpeg", "jpg"), ("image/gif", "gif"), ("image/webp", "webp")];

fn read_clipboard() -> Res<Clip> {
    let clip = raw_clipboard()?;
    let Clip::Image { bytes, ext, .. } = clip else { return Ok(clip) };
    let img = image::load_from_memory(&bytes).map_err(|e| trf!("pasted image: {e}", "imagem colada: {e}"))?;
    let (w, h) = (img.width(), img.height());
    // The card is a few rows tall; a 4K screenshot would only cost memory.
    let thumb = img.thumbnail(480, 480);
    Ok(Clip::Image { bytes, ext, thumb, w, h })
}

/// wl-paste on Wayland, else xclip: shelling out keeps X11 and Wayland
/// libraries out of the build.
#[cfg(target_os = "linux")]
fn raw_clipboard() -> Res<Clip> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let (tool, package) = if wayland { ("wl-paste", "wl-clipboard") } else { ("xclip", "xclip") };
    let get = |args: &[&str]| run(tool, package, args);
    let types = if wayland { get(&["--list-types"])? } else { get(&["-selection", "clipboard", "-t", "TARGETS", "-o"])? };
    // Nothing copied: both tools just fail.
    let Some(types) = types else { return Ok(Clip::Empty) };
    let types = String::from_utf8_lossy(&types);
    let has = |t: &str| types.lines().any(|l| l.trim() == t);
    if let Some((mime, ext)) = TYPES.iter().copied().find(|(m, _)| has(m)) {
        let args: &[&str] = if wayland { &["--type", mime] } else { &["-selection", "clipboard", "-t", mime, "-o"] };
        return match get(args)? {
            Some(bytes) if !bytes.is_empty() => Ok(Clip::Image { bytes, ext, thumb: DynamicImage::new_rgba8(0, 0), w: 0, h: 0 }),
            _ => Ok(Clip::Empty),
        };
    }
    let text = types.lines().any(|l| l.starts_with("text/") || matches!(l.trim(), "UTF8_STRING" | "STRING" | "TEXT"));
    if !text {
        return Ok(Clip::Empty);
    }
    let args: &[&str] = if wayland { &["--no-newline", "--type", "text"] } else { &["-selection", "clipboard", "-t", "UTF8_STRING", "-o"] };
    Ok(get(args)?.map(|b| String::from_utf8_lossy(&b).into_owned()).filter(|s| !s.is_empty()).map_or(Clip::Empty, Clip::Text))
}

/// What the tool printed, `None` when it failed (an empty clipboard). A tool
/// that hangs, waiting on an app that owns the clipboard, is given up on.
#[cfg(target_os = "linux")]
fn run(cmd: &str, package: &str, args: &[&str]) -> Res<Option<Vec<u8>>> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let spawned = Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(trf!("pasting images needs {cmd}: install {package}", "colar imagens precisa do {cmd}: instale o {package}"));
        }
        Err(e) => return Err(format!("{cmd}: {e}")),
    };
    let mut out = child.stdout.take().ok_or_else(|| format!("{cmd}: no output"))?;
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success().then(|| reader.join().unwrap_or_default())),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(trf!("{cmd} didn't answer", "o {cmd} não respondeu"));
            }
        }
    }
}

/// arboard hands over raw pixels: they go up as a PNG.
#[cfg(not(target_os = "linux"))]
fn raw_clipboard() -> Res<Clip> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if let Ok(img) = cb.get_image() {
        let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())
            .ok_or_else(|| tr("pasted image: unreadable", "imagem colada: ilegível").to_string())?;
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        return Ok(Clip::Image { bytes, ext: TYPES[0].1, thumb: DynamicImage::new_rgba8(0, 0), w: 0, h: 0 });
    }
    Ok(cb.get_text().ok().filter(|s| !s.is_empty()).map_or(Clip::Empty, Clip::Text))
}
