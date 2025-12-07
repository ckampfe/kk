use anyhow::anyhow;
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::prelude::Backend;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use regex::Regex;
use rusqlite::{Connection, params};
use std::cmp::min;
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Debug)]
struct Repo {
    conn: Connection,
}

impl Repo {
    fn new(override_database_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let database_path = if let Some(database_path) = override_database_path {
            database_path.to_owned()
        } else {
            let mut database_path = directories::ProjectDirs::from("", "", "kk")
            .expect("unable to find home directory. if you like, you can provide a database path directly by passing the -d option.")
            .data_local_dir()
            .to_path_buf();

            std::fs::create_dir_all(&database_path)?;

            database_path.push("kk.db");

            database_path
        };

        let mut conn = rusqlite::Connection::open(database_path)?;

        conn.pragma_update(None, "foreign_keys", "on")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        Self::setup_database(&mut conn)?;

        let mut this = Self { conn };

        // TODO rm this when there is a flow
        // for creating a board on first run.
        // or just have a default board?
        this.insert_board("my great board")?;

        Ok(this)
    }

    fn setup_database(conn: &mut Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "
            create table if not exists boards (
                id integer primary key,
                name text not null,
                inserted_at timestamp not null default current_timestamp,
                updated_at timestamp not null default current_timestamp
            );

            create unique index if not exists boards_name on boards (name);

            create table if not exists cards (
                id integer primary key,
                board_id integer not null,
                title text not null,
                status text not null default 'Todo',
                body text not null,
                doing_at timestamp,
                done_at timestamp,
                inserted_at timestamp not null default current_timestamp,
                updated_at timestamp not null default current_timestamp,

                foreign key(board_id) references boards(id)
            );

            create index if not exists cards_board_id on cards (board_id);
            create index if not exists cards_status on cards (status);
    ",
        )?;
        Ok(())
    }

    fn insert_board(&mut self, name: &str) -> anyhow::Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        tx.execute(
            "
        insert into boards (name) values (?)
        on conflict do nothing;
        ",
            [name],
        )?;

        let board_id = tx.query_row(
            "
        select id from boards where name = ?
        ",
            [name],
            |row| {
                let id: u64 = row.get(0)?;
                Ok(id)
            },
        )?;

        tx.commit()?;

        Ok(board_id)
    }

    fn insert_card(&self, board_id: u64, title: &str, body: &str) -> anyhow::Result<Card> {
        let card = self.conn.query_row(
            "
        insert into cards (board_id, title, body) values (?, ?, ?)
        returning id
        ",
            params![board_id, title, body],
            |row| {
                Ok(Card {
                    id: row.get(0)?,
                    title: title.to_string(),
                    body: body.to_string(),
                })
            },
        )?;

        Ok(card)
    }

    fn cards_for_column(&self, board_id: u64, column_name: &str) -> anyhow::Result<Vec<Card>> {
        let mut s = self.conn.prepare(
            "
            select
                id,
                title,
                body
            from cards where 1
            and board_id = ?
            and status = ?
            order by id desc;
            ",
        )?;

        let cards_iter = s.query_map(params![board_id, column_name], |row| {
            Ok(Card {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
            })
        })?;

        let mut cards = vec![];

        for card in cards_iter {
            cards.push(card?);
        }

        Ok(cards)
    }

    fn update_card(&self, card_id: u64, title: &str, body: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "
        update cards
        set 
            title = ?2,
            body = ?3
        where id = ?1
        ",
            params![card_id, title, body],
        )?;

        Ok(())
    }

    fn set_card_status(&self, card_id: u64, column_name: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "
        update cards
        set status = ?2
        where id = ?1
        ",
            params![card_id, column_name],
        )?;

        Ok(())
    }
}

#[derive(Debug)]
struct Model {
    current_board_id: u64,
    columns: Vec<Column>,
    selected: SelectedState,
    mode: Mode,
    running_state: RunningState,
    repo: Repo,
    error: Option<String>,
    internal_event_tx: std::sync::mpsc::Sender<Event>,
    internal_event_rx: std::sync::mpsc::Receiver<Event>,
}

enum Event {
    KeyEvent(crossterm::event::KeyEvent),
    InternalEvent(InternalEvent),
}

enum InternalEvent {
    ClearError,
}

#[derive(Debug)]
struct Column {
    name: String,
    cards: Vec<Card>,
}

impl Column {
    fn new(name: String, cards: Vec<Card>) -> Self {
        Column { name, cards }
    }
}

impl Model {
    fn new(options: Options) -> anyhow::Result<Self> {
        let repo = Repo::new(options.database_path)?;

        // TODO actually load the most recently used board or default board or something
        let current_board_id = 1;

        // TODO load cards for the current board
        let todo_cards = repo.cards_for_column(1, "Todo")?;
        let doing_cards = repo.cards_for_column(1, "Doing")?;
        let done_cards = repo.cards_for_column(1, "Done")?;
        let archived_cards = repo.cards_for_column(1, "Archived")?;

        let (tx, rx) = std::sync::mpsc::channel();

        Ok(Self {
            current_board_id,
            columns: vec![
                Column::new("Todo".to_string(), todo_cards),
                Column::new("Doing".to_string(), doing_cards),
                Column::new("Done".to_string(), done_cards),
                Column::new("Archived".to_string(), archived_cards),
            ],
            selected: SelectedState {
                column_id: 0,
                card_index: None,
            },
            mode: Mode::ViewingBoard,
            running_state: RunningState::Running,
            repo,
            error: None,
            internal_event_tx: tx,
            internal_event_rx: rx,
        })
    }

    fn selected_card(&self) -> Option<&Card> {
        if let Some(card_index) = self.selected.card_index {
            Some(&self.columns[self.selected.column_id].cards[card_index])
        } else {
            None
        }
    }

    fn current_cards(&self) -> &[Card] {
        &self.columns[self.selected.column_id].cards
    }

    // fn current_cards_mut(&mut self) -> &mut Vec<Card> {
    //     &mut self.columns[self.selected.column_id].cards
    // }
}

#[derive(Debug, Default)]
struct SelectedState {
    column_id: usize,
    card_index: Option<usize>,
}

impl Display for Column {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[derive(Debug, Default)]
struct Card {
    id: u64,
    title: String,
    body: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum RunningState {
    #[default]
    Running,
    Done,
}

#[derive(Debug, Default, PartialEq)]
enum Mode {
    #[default]
    ViewingBoard,
    ViewingCardDetail,
    MovingCard,
}

#[derive(PartialEq)]
enum Message {
    NavigateLeft,
    NavigateDown,
    NavigateUp,
    NavigateRight,
    Quit,
    NewCard,
    SwitchToMovingState,
    MoveCardLeft,
    // MoveCardDown,
    // MoveCardUp,
    MoveCardRight,
    EditCard,
    SwitchToViewingBoardState,
    ViewCardDetail,
    SetError(Option<String>),
}

fn run_editor<B>(terminal: &mut Terminal<B>, template_text: &str) -> anyhow::Result<String>
where
    B: Backend,
{
    std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;

    let path = {
        let tempfile = tempfile::Builder::new();
        let mut f = tempfile.tempfile()?;
        f.write_all(template_text.as_bytes())?;
        f.into_temp_path()
    };

    let editor = std::env::var("EDITOR")?;

    Command::new(editor).arg(&path).status()?;

    let edited_text = std::fs::read_to_string(&path)?;

    path.close()?;

    std::io::stdout().execute(crossterm::terminal::EnterAlternateScreen)?;
    crossterm::terminal::enable_raw_mode()?;
    terminal.clear()?;

    Ok(edited_text)
}

fn view(model: &mut Model, frame: &mut ratatui::Frame) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Max(3)])
        .split(frame.area());

    let columns_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(std::iter::repeat_n(
            Constraint::Ratio(1, model.columns.len().try_into().unwrap()),
            model.columns.len(),
        ))
        .split(layout[0]);

    for (column_id, column) in model.columns.iter().enumerate() {
        let column_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Max(1), Constraint::Min(5)])
            .split(columns_layout[column_id]);

        frame.render_widget(Paragraph::new(&*column.name), column_layout[0]);

        let mut state = if model.selected.column_id == column_id {
            ListState::default().with_selected(model.selected.card_index)
        } else {
            ListState::default().with_selected(None)
        };

        let list_items = column
            .cards
            .iter()
            .map(|card| ListItem::new(format!("{} {}", card.id, card.title)))
            .collect::<Vec<_>>();

        const PINK: Color = Color::Rgb(255, 150, 167);

        let list = List::new(list_items)
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(PINK))
            .block(
                Block::new()
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                    .border_style(Style::default().fg(Color::Black)),
            );

        frame.render_stateful_widget(list, column_layout[1], &mut state);

        if model.mode == Mode::ViewingCardDetail
            && let Some(card) = model.selected_card()
        {
            let block = Block::bordered().title(format!("{} - {}", card.id, card.title));
            let paragraph = Paragraph::new(&*card.body).block(block);

            let area = popup_area(frame.area(), 60, 50);

            frame.render_widget(ratatui::widgets::Clear, area); //this clears out the background
            frame.render_widget(paragraph, area);

            /// helper function to create a centered rect using up certain percentage of the available rect `r`
            fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
                let vertical =
                    Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
                let horizontal =
                    Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
                let [area] = vertical.areas(area);
                let [area] = horizontal.areas(area);
                area
            }
        }
    }

    let modeline = {
        let modeline_block =
            Block::new().borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT | Borders::RIGHT);

        let mut modeline_text = (match model.mode {
            Mode::ViewingBoard => "VIEWING BOARD",
            Mode::ViewingCardDetail => "VIEWING CARD",
            Mode::MovingCard => "MOVING CARD",
        })
        .to_string();

        if let Some(e) = &model.error {
            modeline_text.push_str(" - Error: ");
            modeline_text.push_str(&e.replace("\n", " "));
        }

        Paragraph::new(modeline_text).block(modeline_block)
    };

    frame.render_widget(modeline, layout[1]);
}

/// Convert Event to Message
///
/// We don't need to pass in a `model` to this function in this example
/// but you might need it as your project evolves
fn receive_event(model: &Model) -> anyhow::Result<Option<Message>> {
    if crossterm::event::poll(Duration::from_millis(250))?
        && let crossterm::event::Event::Key(key) = crossterm::event::read()?
        && key.kind == crossterm::event::KeyEventKind::Press
    {
        return Ok(handle_event(Event::KeyEvent(key), model));
    }

    if let Ok(event) = model.internal_event_rx.try_recv() {
        return Ok(handle_event(event, model));
    }

    Ok(None)
}

fn handle_event(event: Event, model: &Model) -> Option<Message> {
    match event {
        Event::InternalEvent(e) => match e {
            InternalEvent::ClearError => Some(Message::SetError(None)),
        },
        Event::KeyEvent(key) => match model.mode {
            Mode::ViewingBoard => match key.code {
                KeyCode::Char('h') | KeyCode::Left => Some(Message::NavigateLeft),
                KeyCode::Char('j') | KeyCode::Down => Some(Message::NavigateDown),
                KeyCode::Char('k') | KeyCode::Up => Some(Message::NavigateUp),
                KeyCode::Char('l') | KeyCode::Right => Some(Message::NavigateRight),
                KeyCode::Char('q') => Some(Message::Quit),
                KeyCode::Char('m') => Some(Message::SwitchToMovingState),
                KeyCode::Char('n') => Some(Message::NewCard),
                KeyCode::Char('e') => Some(Message::EditCard),
                KeyCode::Enter => Some(Message::ViewCardDetail),
                _ => None,
            },
            Mode::MovingCard => match key.code {
                KeyCode::Char('h') => Some(Message::MoveCardLeft),
                KeyCode::Char('l') => Some(Message::MoveCardRight),
                KeyCode::Char('q') => Some(Message::Quit),
                KeyCode::Char('m') | KeyCode::Enter | KeyCode::Esc => {
                    Some(Message::SwitchToViewingBoardState)
                }
                _ => None,
            },
            Mode::ViewingCardDetail => match key.code {
                KeyCode::Enter | KeyCode::Esc => Some(Message::SwitchToViewingBoardState),
                _ => None,
            },
        },
    }
}

fn update<B>(
    model: &mut Model,
    msg: Message,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<Option<Message>>
where
    B: Backend,
{
    match msg {
        Message::Quit => {
            // You can handle cleanup and exit here
            model.running_state = RunningState::Done;
        }
        Message::NavigateLeft => navigate_left(model),
        Message::NavigateDown => {
            model.selected.card_index = model.selected.card_index.map(|i| {
                min(
                    i.saturating_add(1),
                    model.current_cards().len().saturating_sub(1),
                )
            })
        }
        Message::NavigateUp => {
            model.selected.card_index = model.selected.card_index.map(|i| i.saturating_sub(1))
        }
        Message::NavigateRight => navigate_right(model),
        Message::NewCard => {
            let raw_card_text = run_editor(terminal, "Title\n==========\n\nContent goes here")?;
            let (title, body) = parse_raw_card_text(&raw_card_text)?;

            let card = model
                .repo
                .insert_card(model.current_board_id, title, body)?;

            model.columns[model.selected.column_id].cards.push(card);

            model.columns[model.selected.column_id]
                .cards
                .sort_unstable_by(|a, b| b.id.cmp(&a.id));

            model.mode = Mode::ViewingBoard;
            model.selected.card_index = Some(0);
        }
        Message::SwitchToMovingState => {
            model.mode = Mode::MovingCard;
            // select current card
            // cause moves to move the card between columns
        }
        Message::MoveCardLeft => move_selected_card_left(model)?,
        Message::MoveCardRight => move_selected_card_right(model)?,
        Message::ViewCardDetail => model.mode = Mode::ViewingCardDetail,
        Message::EditCard => {
            if let Some(card_index) = model.selected.card_index {
                let card = &mut model.columns[model.selected.column_id].cards[card_index];

                let card_for_editor = format!("{}\n==========\n\n{}", card.title, card.body);

                let raw_card_text = run_editor(terminal, &card_for_editor)?;

                let (title, body) = parse_raw_card_text(&raw_card_text)?;

                model.repo.update_card(card.id, title, body)?;
                card.title = title.to_string();
                card.body = body.to_string();
            }

            model.mode = Mode::ViewingBoard;
        }
        Message::SwitchToViewingBoardState => model.mode = Mode::ViewingBoard,
        Message::SetError(e) => {
            model.error = e;
            let internal_event_tx = model.internal_event_tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(10));
                let _ = internal_event_tx.send(Event::InternalEvent(InternalEvent::ClearError));
            });
        }
    };

    Ok(None)
}

fn navigate_left(model: &mut Model) {
    let possible_left_column_id = model.selected.column_id.saturating_sub(1);

    if let Some(left_column) = model.columns.get(possible_left_column_id)
        && !left_column.cards.is_empty()
        && possible_left_column_id != model.selected.column_id
    {
        model.selected.column_id = possible_left_column_id;

        if left_column.cards.is_empty() {
            model.selected.card_index = None
        } else {
            model.selected.card_index = Some(min(
                left_column.cards.len().saturating_sub(1),
                model
                    .selected
                    .card_index
                    .unwrap_or(left_column.cards.len().saturating_sub(1)),
            ))
        }
    }
}

fn navigate_right(model: &mut Model) {
    let possible_right_column_id = model.selected.column_id.saturating_add(1);

    if let Some(right_column) = &model.columns.get(possible_right_column_id)
        && !right_column.cards.is_empty()
    {
        model.selected.column_id = possible_right_column_id;

        if right_column.cards.is_empty() {
            model.selected.card_index = None
        } else {
            model.selected.card_index = Some(min(
                right_column.cards.len().saturating_sub(1),
                model
                    .selected
                    .card_index
                    .unwrap_or(right_column.cards.len().saturating_sub(1)),
            ))
        }
    }
}

fn move_selected_card_left(model: &mut Model) -> anyhow::Result<()> {
    if let Some(selected_card_index) = model.selected.card_index {
        let current_column_id = model.selected.column_id;
        let left_column_id = model.selected.column_id.saturating_sub(1);

        if left_column_id != current_column_id {
            let card = model.columns[current_column_id]
                .cards
                .remove(selected_card_index);

            model
                .repo
                .set_card_status(card.id, &model.columns[left_column_id].name)?;

            model.columns[left_column_id].cards.insert(0, card);

            model.selected.card_index = Some(0);

            model.selected.column_id = left_column_id;
        }
    }

    Ok(())
}

fn move_selected_card_right(model: &mut Model) -> anyhow::Result<()> {
    if let Some(selected_card_index) = model.selected.card_index {
        let current_column_id = model.selected.column_id;
        let right_column_id = min(
            model.selected.column_id + 1,
            model.columns.len().saturating_sub(1),
        );

        if right_column_id != current_column_id {
            let card = model.columns[current_column_id]
                .cards
                .remove(selected_card_index);

            model
                .repo
                .set_card_status(card.id, &model.columns[right_column_id].name)?;

            model.columns[right_column_id].cards.insert(0, card);

            model.selected.card_index = Some(0);

            model.selected.column_id = right_column_id;
        }
    }

    Ok(())
}

fn parse_raw_card_text(raw_card_text: &str) -> anyhow::Result<(&str, &str)> {
    let card_regex = Regex::new(r#"(?s)(?<title>[^=\n]+)\n=+\n\n(?<body>.*)"#).unwrap();

    let m = card_regex.captures(raw_card_text);

    if let Some(captures) = m
        && let Some(title) = captures.name("title")
        && let Some(body) = captures.name("body")
    {
        Ok((title.as_str(), body.as_str()))
    } else {
        Err(anyhow!("could not parse raw card text"))
    }
}

#[derive(Parser)]
#[command(author, version, about, name = "kk")]
struct Options {
    #[arg(short, long, env)]
    database_path: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();

    let options = Options::parse();

    let mut model = Model::new(options)?;

    if !model.columns[0].cards.is_empty() {
        model.selected.card_index = Some(0);
    }

    while model.running_state != RunningState::Done {
        // Render the current view
        terminal.draw(|f| view(&mut model, f))?;

        // Handle events and map to a Message
        let mut current_msg = receive_event(&model)?;

        // Process updates as long as they return a non-None message
        while let Some(m) = current_msg {
            match update(&mut model, m, &mut terminal) {
                Ok(m) => current_msg = m,
                Err(e) => current_msg = Some(Message::SetError(Some(e.to_string()))),
            }
        }
    }

    ratatui::restore();

    Ok(())
}
