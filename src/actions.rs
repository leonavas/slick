//! App buttons and menus answering clicks, as the official client sends
//! them: Block Kit elements to `blocks.actions`, legacy attachment buttons
//! to `chat.attachmentAction`. A `confirm` asks first, in the app's words;
//! menus, people, conversations, dates and times are picked here, and the
//! pick shows on the control until the app changes the message.

use crate::app::{Action, App, Event, Menu};
use crate::blocks::{Accessory, Button, Piece};
use crate::i18n::tr;
use crate::slack::{Kind, Message, Res};
use crate::ui::{dim, truncate};
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct State {
    /// What was picked, by `key`: shown on the control while the message
    /// stays as it was when picked.
    picks: HashMap<String, Pick>,
    pub dialog: Option<Dialog>,
    areas: Areas,
}

/// Where the last draw put the dialog and its parts, for the mouse.
#[derive(Default)]
struct Areas {
    rect: Rect,
    yes: Rect,
    no: Rect,
    rows: Vec<(Rect, usize)>,
}

struct Pick {
    /// The message's `sig` when picked.
    sig: u64,
    label: Option<String>,
    on: Option<Vec<String>>,
}

/// A click on a control and what Slack needs to know about its message.
#[derive(Clone)]
pub struct Ctx {
    b: Button,
    chan: String,
    bot: Option<String>,
    user: Option<String>,
    ephemeral: bool,
    thread: Option<String>,
    sig: u64,
}

/// What a click answers: Slack's fields for it (`selected_option`…) and
/// what the control shows meanwhile.
#[derive(Clone, Default)]
pub struct Answer {
    fields: Map<String, Value>,
    label: Option<String>,
    on: Option<Vec<String>>,
}

/// A menu entry of an overflow or a static select: the click and the choice.
#[derive(Clone)]
pub struct Choice {
    ctx: Ctx,
    value: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Pool {
    Users,
    Convs,
    Channels,
    /// A multi static select's own choices.
    Options,
}

#[derive(Clone, Copy, PartialEq)]
pub enum When {
    Date,
    Time,
    DateTime,
}

pub enum Dialog {
    /// The app's `confirm`: its title, text and labels; `yes`: the button highlighted.
    Confirm { ctx: Ctx, ans: Answer, title: String, text: String, ok: String, deny: String, danger: bool, yes: bool },
    /// People, conversations or channels, one or several, searchable.
    List { ctx: Ctx, pool: Pool, multi: bool, query: String, sel: usize, chosen: Vec<String> },
    /// A date, a time or both, typed; arrows step them.
    When { ctx: Ctx, kind: When, text: String, error: Option<String> },
}

pub enum Ev {
    Done { res: Res<()> },
}

/// A control's place, for its pick: the message, the block and the action.
fn key(b: &Button) -> String {
    format!("{}/{}/{}", b.ts, b.block_id, b.action_id)
}

/// The message's blocks and attachments as they are: a change drops the picks.
fn sig(m: &Message) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    m.blocks.to_string().hash(&mut h);
    m.attachments.to_string().hash(&mut h);
    h.finish()
}

/// An element's choices as Slack sent them, groups flattened.
fn options_raw(el: &Value) -> Vec<Value> {
    let flat = el["options"].as_array().into_iter().flatten();
    let grouped = el["option_groups"].as_array().into_iter().flatten().flat_map(|g| g["options"].as_array().into_iter().flatten());
    flat.chain(grouped).cloned().collect()
}

fn text_of(v: &Value) -> String {
    v.as_str().or(v["text"].as_str()).unwrap_or("").to_string()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

impl App {
    /// A click on an app's button, menu or choice in the conversation
    /// (`thread`: in the thread panel) at `x`, `y`.
    pub(crate) fn block_click(&mut self, b: Button, thread: bool, x: u16, y: u16) {
        let Some((chan, msgs)) = self.pane(thread) else { return };
        let m = msgs.iter().find(|m| m.ts == b.ts);
        let ctx = Ctx {
            chan: chan.to_string(),
            bot: m.and_then(|m| m.bot_id.clone()),
            user: m.and_then(|m| m.user.clone()),
            ephemeral: m.is_some_and(crate::slash::is_ephemeral),
            thread: m.and_then(|m| m.thread_ts.clone()).filter(|t| *t != b.ts),
            sig: m.map_or(0, sig),
            b,
        };
        let kind = ctx.b.kind.clone();
        let multi = kind.starts_with("multi_");
        let list = |pool| Dialog::List { ctx: ctx.clone(), pool, multi, query: String::new(), sel: 0, chosen: Vec::new() };
        let when = |kind, text: String| Dialog::When { ctx: ctx.clone(), kind, text, error: None };
        match kind.as_str() {
            "button" | "workflow_button" => {
                // A link opens, and the app hears of the click, as in Slack.
                if let Some(u) = &ctx.b.url {
                    self.open_url(u);
                }
                self.answer(ctx, Answer::default(), false);
            }
            "checkboxes" | "radio_buttons" => self.toggle(ctx),
            "overflow" | "static_select" => {
                let items: Vec<(String, Action)> = ctx
                    .b
                    .options
                    .iter()
                    .map(|o| (o.label.clone(), Action::Block(Box::new(Choice { ctx: ctx.clone(), value: o.value.clone() }))))
                    .collect();
                if !items.is_empty() {
                    self.menu = Some(Menu::new(x, y + 1, items));
                }
            }
            "multi_static_select" => self.block.dialog = Some(list(Pool::Options)),
            "users_select" | "multi_users_select" => self.block.dialog = Some(list(Pool::Users)),
            "conversations_select" | "multi_conversations_select" => self.block.dialog = Some(list(Pool::Convs)),
            "channels_select" | "multi_channels_select" => self.block.dialog = Some(list(Pool::Channels)),
            "datepicker" => {
                let t = ctx.b.el["initial_date"].as_str().map(String::from).unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());
                self.block.dialog = Some(when(When::Date, t));
            }
            "timepicker" => {
                let t = ctx.b.el["initial_time"].as_str().map(String::from).unwrap_or_else(|| Local::now().format("%H:%M").to_string());
                self.block.dialog = Some(when(When::Time, t));
            }
            "datetimepicker" => {
                let at = ctx.b.el["initial_date_time"].as_i64().and_then(|t| Local.timestamp_opt(t, 0).single()).unwrap_or_else(Local::now);
                self.block.dialog = Some(when(When::DateTime, at.format("%Y-%m-%d %H:%M").to_string()));
            }
            // External selects (their choices come from the app) and text
            // boxes: in Slack's web page.
            _ => {
                let url = ctx.b.url.clone().unwrap_or_else(|| self.message_url(&ctx.chan, &ctx.b.ts));
                self.open_url(&url);
            }
        }
        self.dirty = true;
    }

    /// A checkbox or radio button clicked: checked in place, the app told
    /// which are on.
    fn toggle(&mut self, ctx: Ctx) {
        let Some(v) = ctx.b.option.clone() else { return };
        let radio = ctx.b.kind == "radio_buttons";
        let mut on: Vec<String> = ctx.b.options.iter().filter(|o| o.on).map(|o| o.value.clone()).collect();
        if radio {
            if on == [v.clone()] {
                return;
            }
            on = vec![v];
        } else if let Some(i) = on.iter().position(|x| *x == v) {
            on.remove(i);
        } else {
            on.push(v);
        }
        let raw: Vec<Value> = options_raw(&ctx.b.el).into_iter().filter(|o| o["value"].as_str().is_some_and(|x| on.iter().any(|v| v == x))).collect();
        let mut fields = Map::new();
        if radio {
            fields.insert("selected_option".into(), raw.into_iter().next().unwrap_or(Value::Null));
        } else {
            fields.insert("selected_options".into(), Value::Array(raw));
        }
        self.answer(ctx, Answer { fields, label: None, on: Some(on) }, false);
    }

    /// A choice from an overflow or static select's menu.
    pub(crate) fn block_choice(&mut self, c: Choice) {
        let Some(raw) = options_raw(&c.ctx.b.el).into_iter().find(|o| o["value"].as_str() == Some(c.value.as_str())) else { return };
        if let Some(u) = raw["url"].as_str().filter(|u| !u.is_empty()) {
            self.open_url(u);
        }
        let label = (c.ctx.b.kind != "overflow").then(|| c.ctx.b.options.iter().find(|o| o.value == c.value).map(|o| o.label.clone())).flatten();
        let mut fields = Map::new();
        fields.insert("selected_option".into(), raw);
        self.answer(c.ctx, Answer { fields, label, on: None }, false);
        self.dirty = true;
    }

    /// Sends the click to the app, once its `confirm` (if any) said yes.
    fn answer(&mut self, ctx: Ctx, ans: Answer, confirmed: bool) {
        let c = &ctx.b.el["confirm"];
        if !confirmed && c.is_object() {
            let or = |k: &str, d: &str| Some(text_of(&c[k])).filter(|s| !s.is_empty()).unwrap_or_else(|| d.to_string());
            self.block.dialog = Some(Dialog::Confirm {
                title: or("title", tr("Are you sure?", "Tem certeza?")),
                text: crate::text::shortcodes(&text_of(&c["text"])),
                ok: or("confirm", tr("Yes", "Sim")),
                deny: or("deny", tr("Cancel", "Cancelar")),
                danger: c["style"] == "danger",
                yes: true,
                ctx,
                ans,
            });
            self.dirty = true;
            return;
        }
        if ans.label.is_some() || ans.on.is_some() {
            self.block.picks.insert(key(&ctx.b), Pick { sig: ctx.sig, label: ans.label.clone(), on: ans.on.clone() });
        }
        let b = &ctx.b;
        let token = format!("web-{}", now_ms());
        let mut params: Vec<(&'static str, String)> = vec![("client_token", token)];
        if let Some(bot) = &ctx.bot {
            params.push(("service_id", bot.clone()));
        }
        let method = if let Some((attachment, callback)) = &b.legacy {
            let act = json!({ "id": b.el["id"], "name": b.el["name"].as_str().unwrap_or(""), "value": b.value, "type": b.el["type"].as_str().unwrap_or("button") });
            let id = attachment.parse::<u64>().map(Value::from).unwrap_or_else(|_| Value::from(attachment.clone()));
            let mut payload = json!({ "actions": [act], "attachment_id": id, "callback_id": callback, "channel_id": ctx.chan,
                                      "is_ephemeral": ctx.ephemeral, "message_ts": b.ts, "prompt_app_install": false });
            if let Some(t) = &ctx.thread {
                payload["thread_ts"] = json!(t);
            }
            params.push(("payload", payload.to_string()));
            if let Some(u) = &ctx.user {
                params.push(("bot_user_id", u.clone()));
            }
            "chat.attachmentAction"
        } else {
            let mut act = Map::new();
            act.insert("action_id".into(), json!(b.action_id));
            act.insert("block_id".into(), json!(b.block_id));
            act.insert("type".into(), json!(b.kind));
            act.insert("action_ts".into(), json!(format!("{:.6}", now_ms() as f64 / 1000.0)));
            for k in ["text", "value", "style"] {
                if !b.el[k].is_null() {
                    act.insert(k.into(), b.el[k].clone());
                }
            }
            act.extend(ans.fields);
            let container = json!({ "type": "message", "message_ts": b.ts, "channel_id": ctx.chan, "is_ephemeral": ctx.ephemeral });
            params.push(("actions", Value::Array(vec![Value::Object(act)]).to_string()));
            params.push(("container", container.to_string()));
            "blocks.actions"
        };
        self.spawn(move |s| {
            let args: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
            Event::Block(Ev::Done { res: s.call(method, &args).map(|_| ()) })
        });
        self.dirty = true;
    }

    pub(crate) fn on_block(&mut self, ev: Ev) {
        match ev {
            // The app answers by changing the message, which the socket tells.
            Ev::Done { res: Ok(()) } => {}
            Ev::Done { res: Err(e) } => self.error = Some(trf!("the app didn't take the click: {e}", "o app não recebeu o clique: {e}")),
        }
        self.dirty = true;
    }

    /// What was picked, on the controls of a message that hasn't changed since.
    pub(crate) fn apply_picks(&self, m: &Message, pieces: &mut [Piece]) {
        let prefix = format!("{}/", m.ts);
        if !self.block.picks.keys().any(|k| k.starts_with(&prefix)) {
            return;
        }
        let sig = sig(m);
        let f = |b: &mut Button| {
            let Some(p) = self.block.picks.get(&key(b)).filter(|p| p.sig == sig) else { return };
            if let Some(l) = &p.label {
                b.label = l.clone();
            }
            if let Some(on) = &p.on {
                for o in &mut b.options {
                    o.on = on.contains(&o.value);
                }
            }
        };
        walk(pieces, &f);
    }

    /// The choices of a list dialog matching what is typed: (id, name).
    fn pool(&self, ctx: &Ctx, pool: Pool, query: &str) -> Vec<(String, String)> {
        let q = query.trim();
        let mut hits: Vec<(u8, String, String)> = Vec::new();
        let mut add = |id: &str, name: String| {
            if let Some((rank, _)) = crate::text::search(&name, q) {
                hits.push((rank, name, id.to_string()));
            }
        };
        match pool {
            Pool::Users => {
                for (id, name) in &self.users.names {
                    if !self.users.bots.contains(id) && !self.users.deleted.contains(id) {
                        add(id, name.clone());
                    }
                }
            }
            Pool::Convs | Pool::Channels => {
                for c in self.convs.iter().filter(|c| !c.archived) {
                    let name = match c.kind {
                        Kind::Channel | Kind::Private => format!("#{}", self.label(c)),
                        _ if pool == Pool::Channels => continue,
                        _ => self.label(c),
                    };
                    add(&c.id, name);
                }
            }
            Pool::Options => {
                for o in &ctx.b.options {
                    add(&o.value, o.label.clone());
                }
                // In the app's order, not by name.
                return hits.into_iter().map(|(_, n, id)| (id, n)).collect();
            }
        }
        hits.sort();
        hits.into_iter().map(|(_, n, id)| (id, n)).collect()
    }

    /// The list dialog's pick sent: ids by Slack's names for them.
    fn send_list(&mut self) {
        let Some(Dialog::List { ctx, pool, multi, chosen, .. }) = self.block.dialog.take() else { return };
        if chosen.is_empty() {
            return;
        }
        let names: HashMap<String, String> = self.pool(&ctx, pool, "").into_iter().collect();
        let label = chosen.iter().map(|id| names.get(id).cloned().unwrap_or_else(|| id.clone())).collect::<Vec<_>>().join(", ");
        let label = if pool == Pool::Users { label.split(", ").map(|n| format!("@{n}")).collect::<Vec<_>>().join(", ") } else { label };
        let (one, many) = match pool {
            Pool::Users => ("selected_user", "selected_users"),
            Pool::Convs => ("selected_conversation", "selected_conversations"),
            Pool::Channels => ("selected_channel", "selected_channels"),
            Pool::Options => ("selected_option", "selected_options"),
        };
        let value = |id: &String| {
            if pool == Pool::Options {
                options_raw(&ctx.b.el).into_iter().find(|o| o["value"].as_str() == Some(id)).unwrap_or(Value::Null)
            } else {
                json!(id)
            }
        };
        let mut fields = Map::new();
        if multi {
            fields.insert(many.into(), Value::Array(chosen.iter().map(value).collect()));
        } else {
            fields.insert(one.into(), value(&chosen[0]));
        }
        self.answer(ctx, Answer { fields, label: Some(label), on: None }, false);
    }

    /// The date or time typed, checked and sent.
    fn send_when(&mut self) {
        let Some(Dialog::When { kind, text, error, .. }) = self.block.dialog.as_mut() else { return };
        let t = text.trim();
        let (field, value, label) = match *kind {
            When::Date => match NaiveDate::parse_from_str(t, "%Y-%m-%d") {
                Ok(d) => ("selected_date", json!(d.format("%Y-%m-%d").to_string()), d.format("%Y-%m-%d").to_string()),
                Err(_) => return *error = Some(tr("Type the date as 2026-01-31.", "Digite a data como 2026-01-31.").into()),
            },
            When::Time => match NaiveTime::parse_from_str(t, "%H:%M") {
                Ok(h) => ("selected_time", json!(h.format("%H:%M").to_string()), h.format("%H:%M").to_string()),
                Err(_) => return *error = Some(tr("Type the time as 14:30.", "Digite a hora como 14:30.").into()),
            },
            When::DateTime => match NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M").ok().and_then(|d| Local.from_local_datetime(&d).single()) {
                Some(d) => ("selected_date_time", json!(d.timestamp()), d.format("%Y-%m-%d %H:%M").to_string()),
                None => return *error = Some(tr("Type it as 2026-01-31 14:30.", "Digite como 2026-01-31 14:30.").into()),
            },
        };
        let Some(Dialog::When { ctx, .. }) = self.block.dialog.take() else { return };
        let mut fields = Map::new();
        fields.insert(field.into(), value);
        self.answer(ctx, Answer { fields, label: Some(label), on: None }, false);
    }

    fn confirm_answer(&mut self, yes: bool) {
        if let Some(Dialog::Confirm { ctx, ans, .. }) = self.block.dialog.take() {
            if yes {
                self.answer(ctx, ans, true);
            }
        }
    }

    /// Keys while a block dialog is open: all of them.
    pub(crate) fn block_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Esc {
            self.block.dialog = None;
            self.dirty = true;
            return;
        }
        let picks = match &self.block.dialog {
            Some(Dialog::List { ctx, pool, query, .. }) => self.pool(ctx, *pool, query),
            _ => Vec::new(),
        };
        match self.block.dialog.as_mut() {
            Some(Dialog::Confirm { yes, .. }) => match k.code {
                KeyCode::Enter => {
                    let y = *yes;
                    self.confirm_answer(y);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => *yes = !*yes,
                KeyCode::Char('y' | 's') => self.confirm_answer(true),
                KeyCode::Char('n') => self.confirm_answer(false),
                _ => {}
            },
            Some(Dialog::List { multi, query, sel, chosen, .. }) => {
                *sel = (*sel).min(picks.len().saturating_sub(1));
                let send = (ctrl && matches!(k.code, KeyCode::Enter | KeyCode::Char('j'))) || (k.code == KeyCode::Enter && *multi && query.is_empty() && !chosen.is_empty());
                match k.code {
                    _ if send => self.send_list(),
                    KeyCode::Enter => {
                        if let Some((id, _)) = picks.get(*sel) {
                            if !*multi {
                                *chosen = vec![id.clone()];
                                self.send_list();
                            } else if let Some(i) = chosen.iter().position(|c| c == id) {
                                chosen.remove(i);
                            } else {
                                chosen.push(id.clone());
                                query.clear();
                                *sel = 0;
                            }
                        }
                    }
                    KeyCode::Up => *sel = sel.saturating_sub(1),
                    KeyCode::Down => *sel = (*sel + 1).min(picks.len().saturating_sub(1)),
                    KeyCode::Backspace if query.is_empty() => {
                        chosen.pop();
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        *sel = 0;
                    }
                    KeyCode::Char('u') if ctrl => query.clear(),
                    KeyCode::Char(c) if !ctrl => {
                        query.push(c);
                        *sel = 0;
                    }
                    _ => {}
                }
            }
            Some(Dialog::When { kind, text, error, .. }) => match k.code {
                KeyCode::Enter => self.send_when(),
                KeyCode::Up | KeyCode::Down => {
                    let up = k.code == KeyCode::Up;
                    *text = step(*kind, text, up);
                    *error = None;
                }
                KeyCode::Backspace => {
                    text.pop();
                    *error = None;
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if !ctrl && (c.is_ascii_digit() || matches!(c, '-' | ':' | ' ')) => {
                    text.push(c);
                    *error = None;
                }
                _ => {}
            },
            None => {}
        }
        self.dirty = true;
    }

    /// The mouse while a block dialog is open: all of it; a click outside closes it.
    pub(crate) fn block_mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        let a = &self.block.areas;
        let (inside, yes, no) = (a.rect.contains(at), a.yes.contains(at), a.no.contains(at));
        let row = a.rows.iter().find(|(r, _)| r.contains(at)).map(|(_, i)| *i);
        let picks = match &self.block.dialog {
            Some(Dialog::List { ctx, pool, query, .. }) => self.pool(ctx, *pool, query),
            _ => Vec::new(),
        };
        match m.kind {
            MouseEventKind::Down(_) if !inside => self.block.dialog = None,
            MouseEventKind::Down(MouseButton::Left) if no => self.block.dialog = None,
            MouseEventKind::Down(MouseButton::Left) if yes => match self.block.dialog {
                Some(Dialog::Confirm { .. }) => self.confirm_answer(true),
                Some(Dialog::List { .. }) => self.send_list(),
                Some(Dialog::When { .. }) => self.send_when(),
                None => {}
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if let (Some(Dialog::List { multi, sel, chosen, query, .. }), Some(i)) = (self.block.dialog.as_mut(), row) {
                    let Some((id, _)) = picks.get(i) else { return };
                    *sel = i;
                    if !*multi {
                        *chosen = vec![id.clone()];
                        self.send_list();
                    } else if let Some(k) = chosen.iter().position(|c| c == id) {
                        chosen.remove(k);
                    } else {
                        chosen.push(id.clone());
                        query.clear();
                    }
                }
            }
            MouseEventKind::Moved => match (self.block.dialog.as_mut(), row) {
                (Some(Dialog::List { sel, .. }), Some(i)) if *sel != i => *sel = i,
                _ => return,
            },
            MouseEventKind::ScrollUp => {
                if let Some(Dialog::List { sel, .. }) = self.block.dialog.as_mut() {
                    *sel = sel.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(Dialog::List { sel, .. }) = self.block.dialog.as_mut() {
                    *sel = (*sel + 3).min(picks.len().saturating_sub(1));
                }
            }
            _ => return,
        }
        self.dirty = true;
    }
}

/// The controls among the pieces, nested ones included.
fn walk(pieces: &mut [Piece], f: &dyn Fn(&mut Button)) {
    for p in pieces {
        match p {
            Piece::Text(_, Some(Accessory::Button(b))) => f(b),
            Piece::Controls(bs) => bs.iter_mut().for_each(f),
            Piece::Bar(_, inner) => walk(inner, f),
            _ => {}
        }
    }
}

/// Up and Down on a date or time: a day, or five minutes.
fn step(kind: When, text: &str, up: bool) -> String {
    let t = text.trim();
    let d = if up { 1 } else { -1 };
    match kind {
        When::Date => NaiveDate::parse_from_str(t, "%Y-%m-%d").map(|x| (x + chrono::Duration::days(d)).format("%Y-%m-%d").to_string()),
        When::Time => NaiveTime::parse_from_str(t, "%H:%M").map(|x| (x + chrono::Duration::minutes(5 * d)).format("%H:%M").to_string()),
        When::DateTime => NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M").map(|x| (x + chrono::Duration::minutes(5 * d)).format("%Y-%m-%d %H:%M").to_string()),
    }
    .unwrap_or_else(|_| text.to_string())
}

// ---- drawing -------------------------------------------------------------

/// A centred, bordered box; returns its inside.
fn frame(f: &mut Frame, app: &mut App, title: &str, w: u16, h: u16) -> Rect {
    let area = f.area();
    let (w, h) = (w.min(area.width), h.min(area.height));
    let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, rect);
    let title = truncate(title, w.saturating_sub(4) as usize);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(dim()).title(Line::from(format!(" {title} ")).bold());
    let inner = block.inner(rect).inner(Margin { horizontal: 1, vertical: 0 });
    f.render_widget(block, rect);
    app.block.areas.rect = rect;
    inner
}

/// The bottom row: the hint on the left, then the two buttons on the right.
fn buttons(f: &mut Frame, app: &mut App, inner: Rect, hint: &str, deny: &str, ok: &str, ok_style: Style, deny_style: Style) {
    let y = inner.bottom().saturating_sub(1);
    let (ok, deny) = (format!(" {ok} "), format!(" {deny} "));
    let (ow, dw) = (ok.width() as u16, deny.width() as u16);
    let yes = Rect::new(inner.right().saturating_sub(ow), y, ow.min(inner.width), 1);
    let no = Rect::new(yes.x.saturating_sub(dw + 1), y, dw.min(inner.width), 1);
    f.render_widget(Paragraph::new(Span::styled(ok, ok_style)), yes);
    f.render_widget(Paragraph::new(Span::styled(deny, deny_style)), no);
    (app.block.areas.yes, app.block.areas.no) = (yes, no);
    let room = no.x.saturating_sub(inner.x + 1) as usize;
    f.render_widget(Paragraph::new(truncate(hint, room)).style(dim()), Rect::new(inner.x, y, room as u16, 1));
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.block.areas = Areas::default();
    let area = f.area();
    let quiet = Style::new().fg(Color::White).bg(Color::DarkGray);
    match &app.block.dialog {
        None => {}
        Some(Dialog::Confirm { title, text, ok, deny, danger, yes, .. }) => {
            let (title, text, ok, deny, danger, yes) = (title.clone(), text.clone(), ok.clone(), deny.clone(), *danger, *yes);
            let w = 56.min(area.width.saturating_sub(4));
            let lines = (text.width() as u16 / w.saturating_sub(4).max(1)) + text.lines().count() as u16 + 1;
            let inner = frame(f, app, &title, w, (lines + 5).min(area.height.saturating_sub(2)));
            if inner.height < 3 {
                return;
            }
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), Rect::new(inner.x, inner.y + 1, inner.width, inner.height.saturating_sub(3)));
            let loud = if danger { Style::new().fg(Color::White).bg(Color::Red).bold() } else { Style::new().fg(Color::Black).bg(Color::Green).bold() };
            let mark = |s: Style, on: bool| if on { s.add_modifier(Modifier::UNDERLINED) } else { s };
            let hint = tr("Enter · ←→ · Esc", "Enter · ←→ · Esc");
            buttons(f, app, inner, hint, &deny, &ok, mark(loud, yes), mark(quiet, !yes));
        }
        Some(Dialog::List { ctx, pool, multi, query, sel, chosen }) => {
            let (pool, multi, query, chosen) = (*pool, *multi, query.clone(), chosen.clone());
            let picks = app.pool(ctx, pool, &query);
            let names: HashMap<String, String> = if chosen.is_empty() { HashMap::new() } else { app.pool(ctx, pool, "").into_iter().collect() };
            let sel = (*sel).min(picks.len().saturating_sub(1));
            let title = match pool {
                Pool::Users => if multi { tr("Pick people", "Escolher pessoas") } else { tr("Pick a person", "Escolher uma pessoa") },
                Pool::Convs => if multi { tr("Pick conversations", "Escolher conversas") } else { tr("Pick a conversation", "Escolher uma conversa") },
                Pool::Channels => if multi { tr("Pick channels", "Escolher canais") } else { tr("Pick a channel", "Escolher um canal") },
                Pool::Options => tr("Pick items", "Escolher itens"),
            };
            let inner = frame(f, app, title, 60.min(area.width.saturating_sub(4)), 20.min(area.height.saturating_sub(2)));
            if inner.width < 16 || inner.height < 6 {
                return;
            }
            let width = inner.width as usize;
            // What is typed, after those picked.
            let picked = chosen.iter().map(|id| names.get(id).cloned().unwrap_or_else(|| id.clone())).collect::<Vec<_>>().join(", ");
            let lead = if picked.is_empty() { String::new() } else { format!("{} · ", truncate(&picked, width / 2)) };
            let mut spans = vec![Span::styled(lead.clone(), Style::new().fg(Color::Cyan))];
            if query.is_empty() {
                spans.push(Span::styled(tr("type to search", "digite para buscar"), dim()));
            } else {
                spans.push(Span::raw(truncate(&query, width.saturating_sub(lead.width() + 1))));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), Rect::new(inner.x, inner.y, inner.width, 1));
            f.set_cursor_position((inner.x + (lead.width() + query.width()).min(width.saturating_sub(1)) as u16, inner.y));
            f.render_widget(Paragraph::new("─".repeat(width)).style(dim()), Rect::new(inner.x, inner.y + 1, inner.width, 1));
            let top = inner.y + 2;
            let list_h = inner.bottom().saturating_sub(top + 2) as usize;
            if picks.is_empty() {
                f.render_widget(Paragraph::new(format!(" {}", tr("Nothing found", "Nada encontrado"))).style(dim()), Rect::new(inner.x, top, inner.width, 1));
            }
            let offset = (sel + 1).saturating_sub(list_h);
            for (n, (id, name)) in picks.iter().enumerate().skip(offset).take(list_h) {
                let rect = Rect::new(inner.x, top + (n - offset) as u16, inner.width, 1);
                let mark = match (multi, chosen.contains(id)) {
                    (true, true) => "☑ ",
                    (true, false) => "☐ ",
                    _ => "",
                };
                let mut line = Line::from(vec![Span::raw(" "), Span::raw(mark), Span::raw(truncate(name, width.saturating_sub(4)))]);
                if n == sel {
                    line = line.style(Style::new().add_modifier(Modifier::REVERSED));
                }
                f.render_widget(Paragraph::new(line), rect);
                app.block.areas.rows.push((rect, n));
            }
            let hint = if multi { tr("Enter checks · Ctrl+Enter sends · Esc", "Enter marca · Ctrl+Enter envia · Esc") } else { tr("Enter picks · Esc closes", "Enter escolhe · Esc fecha") };
            let ok_style = if multi && chosen.is_empty() { quiet } else { Style::new().fg(Color::Black).bg(Color::Green).bold() };
            buttons(f, app, inner, hint, tr("Cancel", "Cancelar"), tr("Send", "Enviar"), ok_style, quiet);
        }
        Some(Dialog::When { kind, text, error, .. }) => {
            let (kind, text, error) = (*kind, text.clone(), error.clone());
            let (title, format) = match kind {
                When::Date => (tr("Pick a date", "Escolher data"), "2026-01-31"),
                When::Time => (tr("Pick a time", "Escolher hora"), "14:30"),
                When::DateTime => (tr("Pick a date and time", "Escolher data e hora"), "2026-01-31 14:30"),
            };
            let inner = frame(f, app, title, 48.min(area.width.saturating_sub(4)), 8.min(area.height.saturating_sub(2)));
            if inner.height < 4 {
                return;
            }
            let field = Line::from(vec![Span::styled("› ", Style::new().fg(Color::Cyan).bold()), Span::raw(text.clone()), Span::styled(format!("   {format}"), dim())]);
            f.render_widget(Paragraph::new(field), Rect::new(inner.x, inner.y + 1, inner.width, 1));
            f.set_cursor_position((inner.x + 2 + text.width() as u16, inner.y + 1));
            if let Some(e) = error {
                f.render_widget(Paragraph::new(truncate(&e, inner.width as usize)).style(Style::new().fg(Color::Yellow)), Rect::new(inner.x, inner.y + 2, inner.width, 1));
            }
            let hint = tr("↑↓ steps · Enter sends", "↑↓ ajusta · Enter envia");
            buttons(f, app, inner, hint, tr("Cancel", "Cancelar"), tr("Send", "Enviar"), Style::new().fg(Color::Black).bg(Color::Green).bold(), quiet);
        }
    }
}
