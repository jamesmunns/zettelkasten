mod config;
mod login;
mod register;
mod utils;
mod zettel;

use crossterm::event::{Event, KeyCode};
use snafu::{ResultExt, Snafu};
use std::{
    borrow::Cow,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};
use tui::{
    text::{Spans, Text},
    widgets::{Block, Borders, Paragraph},
};
use zettelkasten_shared::storage;

pub enum View {
    Config(config::Config),
    Login(login::Login),
    Register(register::Register),
    Zettel(zettel::Zettel),
}

impl From<login::Login> for View {
    fn from(v: login::Login) -> Self {
        Self::Login(v)
    }
}

impl From<zettel::Zettel> for View {
    fn from(v: zettel::Zettel) -> Self {
        Self::Zettel(v)
    }
}
impl From<config::Config> for View {
    fn from(v: config::Config) -> Self {
        Self::Config(v)
    }
}

impl View {
    pub fn new(system_config: &storage::SystemConfig, storage: &Arc<dyn storage::Storage>) -> Self {
        match system_config.user_mode {
            storage::UserMode::SingleUserAutoLogin => {
                match zettelkasten_shared::block_on(storage.login_single_user()) {
                    // Successfully logged in
                    Ok(user) => Self::Zettel(user.into()),
                    // Failed to log in, show the login view and the error
                    Err(source) => login::Login {
                        error: Some(login::LoginError::Storage { source }),
                        ..Default::default()
                    }
                    .into(),
                }
            }
            storage::UserMode::MultiUser | storage::UserMode::SingleUserManualLogin => {
                // show the login view
                login::Login::default().into()
            }
        }
    }

    pub(crate) fn render(&mut self, tui: &mut crate::Tui) -> Result<Option<View>> {
        let next = match self {
            Self::Zettel(li) => match li.render(tui)? {
                Some(zettel::Transition::Exit) => {
                    tui.running = false;
                    None
                }
                Some(zettel::Transition::NavigateTo { path }) => {
                    if let Some(sys_path) = path.strip_prefix("sys:") {
                        open_sys_page(sys_path, li, tui)
                    } else {
                        let zettel = zettelkasten_shared::block_on(
                            tui.storage.get_zettel_by_url(li.user.id, &path),
                        )
                        .context(DatabaseSnafu)?;

                        let zettel = if let Some(zettel) = zettel {
                            zettelkasten_shared::block_on(
                                tui.storage
                                    .set_user_last_visited_zettel(li.user.id, Some(zettel.id)),
                            )
                            .context(DatabaseSnafu)?;
                            zettel
                        } else {
                            storage::Zettel {
                                path,
                                ..Default::default()
                            }
                        };

                        Some(
                            zettel::Zettel {
                                user: li.user.clone(),
                                zettel: Some(zettel),
                            }
                            .into(),
                        )
                    }
                }
                Some(zettel::Transition::Logout) => Some(Self::Login(Default::default())),
                Some(zettel::Transition::OpenConfig) => {
                    Some(config::Config::new(Some(Self::Zettel(li.clone())), tui).into())
                }
                Some(zettel::Transition::Edit) => {
                    let zettel = li.zettel.as_mut().unwrap();
                    if let Some(new_body) = edit(zettel, tui)? {
                        zettel.body = new_body;
                        zettelkasten_shared::block_on(
                            tui.storage.update_zettel(li.user.id, zettel),
                        )
                        .context(DatabaseSnafu)?;
                    }
                    None
                }

                None => None,
            },
            Self::Login(login) => match login.render(tui)? {
                Some(login::Transition::Exit) => {
                    tui.running = false;
                    None
                }
                Some(login::Transition::Register) => Some(Self::Register(Default::default())),
                Some(login::Transition::Login { user }) => Some(Self::Zettel(user.into())),
                None => None,
            },
            Self::Register(reg) => match reg.render(tui)? {
                Some(register::Transition::Exit) => {
                    tui.running = false;
                    None
                }
                Some(register::Transition::Registered { user }) => Some(Self::Zettel(user.into())),
                Some(register::Transition::Login) => Some(Self::Login(Default::default())),
                None => None,
            },
            Self::Config(config) => match config.render(tui)? {
                Some(config::Transition::Pop) => config.parent_page.take().map(|b| *b),
                None => None,
            },
        };

        Ok(next)
    }
}

fn open_sys_page(path: &str, li: &zettel::Zettel, tui: &mut crate::Tui) -> Option<View> {
    if path == "config" {
        Some(config::Config::new(Some(View::Zettel(li.clone())), tui).into())
    } else {
        alert(tui.terminal, |f| {
            f.title("Reserved sys page")
                .text(format!(
                    "`sys:` is a reserved prefix, could not navigate to `sys:{path:?}`"
                ))
                .action(KeyCode::Char('c'), "continue")
        })
        .expect("Double fault, time to crash to desktop");
        None
    }
}

fn edit(zettel: &storage::Zettel, tui: &mut crate::Tui) -> Result<Option<String>> {
    let editor = if let Some(editor) = &tui.system_config.terminal_editor {
        editor
    } else {
        alert(tui.terminal, |cb| {
            cb.title("Could not edit zettel")
                .text("No terminal editor configured")
                .text("Please set one up in sys:config")
        })?;
        return Ok(None);
    };
    let mut tmp_file = tempfile::Builder::new()
        .suffix(".md")
        .tempfile()
        .context(IoSnafu)?;
    tmp_file
        .write_all(zettel.body.as_bytes())
        .context(IoSnafu)?;
    let _status = std::process::Command::new(editor)
        .arg(tmp_file.path())
        .status()
        .context(IoSnafu)?;

    tmp_file.seek(SeekFrom::Start(0)).context(IoSnafu)?;
    let mut result = String::new();
    tmp_file.read_to_string(&mut result).context(IoSnafu)?;
    Ok(Some(result))
}

pub type Result<T = ()> = std::result::Result<T, ViewError>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ViewError {
    #[snafu(display("Could not retrieve the terminal size"))]
    TerminalSize {
        source: std::io::Error,
    },
    #[snafu(display("Could not render a frame"))]
    RenderFrame {
        source: std::io::Error,
    },
    #[snafu(display("Could not get the next terminal event"))]
    Event {
        source: std::io::Error,
    },
    #[snafu(display("Database error: {source:?}"))]
    Database {
        source: storage::Error,
    },
    #[snafu(display("Zettel ID {id} not found"))]
    ZettelIdNotFound {
        id: i64,
    },
    #[snafu(display("Not implemented"))]
    NotImplemented,
    Io {
        source: std::io::Error,
    },
}

pub fn alert<F>(terminal: &mut super::Terminal, cb: F) -> Result<KeyCode>
where
    F: Fn(ViewBuilder) -> ViewBuilder,
{
    loop {
        let size = terminal.size().context(TerminalSizeSnafu)?;
        let builder = ViewBuilder::default();
        let builder = cb(builder);
        terminal
            .draw(|f| {
                let mut lines = Vec::<Spans>::with_capacity(
                    builder.lines.len() + if builder.actions.is_empty() { 0 } else { 2 },
                );
                for line in &builder.lines {
                    lines.push(line.as_ref().into());
                }
                if !builder.actions.is_empty() {
                    lines.push(Spans::default());
                    let mut actions = String::new();
                    for (idx, (key, text)) in builder.actions.iter().enumerate() {
                        if idx != 0 {
                            actions += ", ";
                        }
                        match key {
                            KeyCode::Char(c) => actions.push(c.to_ascii_uppercase()),
                            KeyCode::Enter => actions += "<enter>",
                            _ => unreachable!(),
                        }
                        actions += ": ";
                        actions += text;
                    }
                    lines.push(actions.into());
                }
                let mut block = Block::default().borders(Borders::ALL);
                if let Some(title) = &builder.title {
                    block = block.title(title.as_ref());
                }

                let paragraph = Paragraph::new(Text { lines }).block(block);
                f.render_widget(paragraph, size)
            })
            .context(RenderFrameSnafu)?;

        let event = crossterm::event::read().context(EventSnafu)?;
        if let Event::Key(key) = event {
            if builder.actions.iter().any(|(k, _)| k == &key.code) {
                break Ok(key.code);
            }
        }
    }
}

#[derive(Default)]
pub struct ViewBuilder {
    width: u16,
    height: u16,
    title: Option<Cow<'static, str>>,
    lines: Vec<Cow<'static, str>>,
    actions: Vec<(KeyCode, Cow<'static, str>)>,
}

impl ViewBuilder {
    pub fn title(mut self, text: impl Into<Cow<'static, str>>) -> Self {
        self.title = Some(text.into());
        self
    }
    pub fn text(mut self, text: impl Into<Cow<'static, str>>) -> Self {
        let text = text.into();
        self.width = self.width.max(text.chars().count() as u16 + 2);
        self.height += 1;
        self.lines.push(text);
        self
    }
    pub fn action(mut self, code: KeyCode, text: impl Into<Cow<'static, str>>) -> Self {
        let text = text.into();
        let mut line_width = text.chars().count() as u16
            + match code {
                KeyCode::Char(_) => 3, // 'c: '
                KeyCode::Enter => 9,   // '<return> ',
                _ => panic!("Unknown keycode character length: {code:?}"),
            };
        if !self.actions.is_empty() {
            line_width += 2; // ', '
        } else {
            self.height += 1;
        }
        self.width = self.width.max(line_width);
        self.actions.push((code, text));
        self
    }
}
