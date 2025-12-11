use anyhow::anyhow;
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::prelude::Backend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph};
use regex::Regex;
use rusqlite::{Connection, params};
use std::cmp::min;
use std::collections::HashSet;
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Debug)]
struct BoardMeta {
    id: u64,
    name: String,
    columns: Vec<String>,
    inserted_at: String,
    updated_at: String,
    viewed_at: String,
}

#[derive(Debug, PartialEq)]
enum ConfirmationState {
    Yes,
    No,
}

impl ConfirmationState {
    fn toggle(&self) -> ConfirmationState {
        if *self == ConfirmationState::Yes {
            ConfirmationState::No
        } else {
            ConfirmationState::Yes
        }
    }
}

#[derive(Debug)]
struct Model {
    board_metas: Vec<BoardMeta>,
    board: Option<Board>,
    selected: SelectedState,
    mode: Mode,
    running_state: RunningState,
    confirmation_state: ConfirmationState,
    repo: Repo,
    error: Option<String>,
    internal_event_tx: std::sync::mpsc::Sender<Event>,
    internal_event_rx: std::sync::mpsc::Receiver<Event>,
}

impl Model {
    fn new(options: Options) -> anyhow::Result<Self> {
        let repo = Repo::new(options.database_path)?;

        let (tx, rx) = std::sync::mpsc::channel();

        let board = repo.load_most_recently_viewed_board()?;

        Ok(Self {
            board_metas: vec![],
            board: Some(board),
            confirmation_state: ConfirmationState::No,
            selected: SelectedState {
                // TODO actually load the most recently used board or default board or something
                board_id: 1,
                // TODO actually load the most recently used board or default board or something
                board_index: None,
                column_index: 0,
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

    fn switch_to_viewing_boards_mode(&mut self) -> anyhow::Result<()> {
        self.mode = Mode::ViewingBoards;
        self.board_metas = self.repo.get_board_metas()?;
        self.board = None;
        if !self.board_metas.is_empty() {
            self.selected.board_index = Some(0);
            if let Some(board_index) = self.selected.board_index {
                self.selected.board_id = self.board_metas[board_index].id
            }
        }

        Ok(())
    }

    fn selected_column_mut(&mut self) -> Option<&mut Column> {
        let board = self.board.as_mut().unwrap();
        board.columns.get_mut(self.selected.column_index)
    }

    fn add_card_to_selected_column(&mut self, card: Card) {
        if let Some(current_column) = self.selected_column_mut() {
            current_column.cards.push(card);
            current_column
                .cards
                .sort_unstable_by(|a, b| b.id.cmp(&a.id));
        }
    }

    fn selected_card(&self) -> Option<&Card> {
        if let Some(card_index) = self.selected.card_index {
            self.selected_column()
                .and_then(|column| column.cards.get(card_index))
        } else {
            None
        }
    }

    fn selected_card_id(&self) -> Option<u64> {
        if let Some(card_index) = self.selected.card_index {
            self.selected_column()
                .and_then(|column| column.cards.get(card_index).map(|card| card.id))
        } else {
            None
        }
    }

    fn toggle_confirmation_state(&mut self) {
        self.confirmation_state = self.confirmation_state.toggle();
    }

    fn selected_card_mut(&mut self) -> Option<&mut Card> {
        if let Some(card_index) = self.selected.card_index {
            self.selected_column_mut()
                .and_then(|column| column.cards.get_mut(card_index))
        } else {
            None
        }
    }

    fn selected_column(&self) -> Option<&Column> {
        let board = self.board.as_ref().unwrap();
        board.columns.get(self.selected.column_index)
    }

    fn navigate_left(&mut self) {
        if let Some(board) = &mut self.board {
            let left_column_id = self.selected.column_index.saturating_sub(1);
            if left_column_id != self.selected.column_index
                && let Some(left_column) = board.columns.get(left_column_id)
            {
                let left_column_len = left_column.cards.len();

                self.selected.column_index = left_column_id;

                self.selected.card_index = if left_column.cards.is_empty() {
                    None
                } else {
                    Some(min(
                        left_column_len.saturating_sub(1),
                        self.selected
                            .card_index
                            .unwrap_or(left_column_len.saturating_sub(1)),
                    ))
                }
            }
        }
    }

    fn navigate_right(&mut self) {
        if let Some(board) = &mut self.board {
            let right_column_id = self.selected.column_index.saturating_add(1);
            if right_column_id != self.selected.column_index
                && let Some(right_column) = board.columns.get(right_column_id)
            {
                let right_column_len = right_column.cards.len();

                self.selected.column_index = right_column_id;

                self.selected.card_index = if right_column.cards.is_empty() {
                    None
                } else {
                    Some(min(
                        right_column_len.saturating_sub(1),
                        self.selected
                            .card_index
                            .unwrap_or(right_column_len.saturating_sub(1)),
                    ))
                }
            }
        }
    }

    fn load_selected_board(&mut self) -> anyhow::Result<()> {
        self.board = Some(self.repo.load_board(self.selected.board_id)?);
        Ok(())
    }

    fn create_board(&mut self, name: &str, column_names: &[&str]) -> anyhow::Result<()> {
        if !column_names.is_empty() {
            self.repo.create_board(name, column_names)?;
            self.board_metas = self.repo.get_board_metas()?;
            Ok(())
        } else {
            Err(anyhow!("Board must have at least 1 column"))
        }
    }

    fn update_selected_board(
        &mut self,
        new_board_name: &str,
        new_column_names: Vec<&str>,
    ) -> anyhow::Result<()> {
        let selected_board = &self.board_metas[self.selected.board_index.unwrap()];

        let current_names_set: HashSet<_> = selected_board.columns.iter().cloned().collect();

        let new_names_set: HashSet<_> = new_column_names.iter().map(|s| s.to_string()).collect();

        // TODO figure out adding/removing/etc
        if new_names_set.is_superset(&current_names_set) {
            let _new_board = self.repo.update_board_columns_order(
                selected_board.id,
                new_board_name,
                new_column_names,
            )?;

            self.board_metas = self.repo.get_board_metas()?;
        } else {
            return Err(anyhow!("Could not update board: columns do not match"));
        }

        Ok(())
    }

    fn confirm_card_delete(&mut self) -> anyhow::Result<()> {
        self.mode = Mode::ConfirmCardDeletion;
        Ok(())
    }

    fn delete_selected_card(&mut self) -> anyhow::Result<()> {
        if let Some(card_id) = self.selected_card_id()
            && let Some(board) = &mut self.board
            && let Some(column) = board.columns.get_mut(self.selected.column_index)
            && let Some(card_index) = self.selected.card_index.as_mut()
        {
            self.repo.delete_card(card_id)?;
            column.cards.remove(*card_index);
            if column.cards.len().saturating_sub(1) < *card_index {
                *card_index = column.cards.len().saturating_sub(1);
            }
        }

        Ok(())
    }
}

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
                updated_at timestamp not null default current_timestamp,
                viewed_at timestamp not null default current_timestamp
            );

            create unique index if not exists boards_name on boards (name);

            create table if not exists statuses (
                id integer primary key,
                name text not null,
                column_order integer not null,
                board_id integer not null,
                inserted_at timestamp not null default current_timestamp,
                updated_at timestamp not null default current_timestamp,

                foreign key(board_id) references boards(id)
            );

            create unique index if not exists statuses_name_board_id on statuses (name, board_id);
            -- not possible to do this while updating orders that could be the same
            -- during a transaction
            -- create unique index if not exists statuses_column_order_board_id on statuses (column_order, board_id);
            create index if not exists statuses_board_id on statuses (board_id);

            create table if not exists cards (
                id integer primary key,
                board_id integer not null,
                title text not null,
                status_id integer not null,
                body text not null,
                doing_at timestamp,
                done_at timestamp,
                inserted_at timestamp not null default current_timestamp,
                updated_at timestamp not null default current_timestamp,

                foreign key(board_id) references boards(id)
                foreign key(status_id) references statuses(id)
            );

            create index if not exists cards_board_id on cards (board_id);
            create index if not exists cards_status_id on cards (status_id);

            create trigger if not exists cards_updated after update on cards
            for each row
            begin
                update cards
                set updated_at = current_timestamp
                where cards.id = NEW.id;

                update boards
                set updated_at = current_timestamp
                where boards.id = NEW.board_id;
            end
    ",
        )?;
        Ok(())
    }

    fn get_board_metas(&self) -> anyhow::Result<Vec<BoardMeta>> {
        let mut s = self.conn.prepare(
            "
        select
            boards.id,
            boards.name,
            group_concat(statuses.name, '|' order by statuses.column_order),
            boards.inserted_at,
            boards.updated_at,
            boards.viewed_at
        from boards
        inner join statuses
            on statuses.board_id = boards.id
        group by boards.id, boards.name
        order by boards.viewed_at desc
        ",
        )?;

        let boards_iter = s.query_map([], |row| {
            let column_names: String = row.get(2)?;
            let columns_names = column_names.split('|').map(|s| s.to_string()).collect();

            Ok(BoardMeta {
                id: row.get(0)?,
                name: row.get(1)?,
                columns: columns_names,
                inserted_at: row.get(3)?,
                updated_at: row.get(4)?,
                viewed_at: row.get(5)?,
            })
        })?;

        let mut boards = vec![];

        for board in boards_iter {
            boards.push(board?);
        }

        Ok(boards)
    }

    fn load_board(&mut self, board_id: u64) -> anyhow::Result<Board> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let board_name = {
            let mut board_s = tx.prepare(
                "
        select
            name
        from boards
        where id = ?
        ",
            )?;

            let board_name: String = board_s.query_one([board_id], |row| row.get(0))?;

            board_name
        };

        tx.execute(
            "
        update boards
        set viewed_at = current_timestamp
        where id = ?
        ",
            [board_id],
        )?;

        tx.commit()?;

        let columns = self.get_cards_for_board(board_id)?;

        Ok(Board {
            id: board_id,
            name: board_name,
            columns,
        })
    }

    fn get_cards_for_board(&self, board_id: u64) -> anyhow::Result<Vec<Column>> {
        let mut statuses_s = self.conn.prepare(
            "
            select
                name
            from statuses
            where board_id = ?
            order by column_order asc
            ",
        )?;

        let statuses_iter = statuses_s.query_map([board_id], |row| row.get(0))?;

        let mut columns = vec![];

        for status in statuses_iter {
            let status: String = status?;
            let cards = self.cards_for_column(board_id, &status)?;
            columns.push(Column {
                name: status,
                cards,
            })
        }

        Ok(columns)
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
        let status_id: Result<u64, rusqlite::Error> = self.conn.query_one(
            "
        select
            id
        from statuses
        where board_id = ?
        order by column_order asc
        limit 1
        ",
            [board_id],
            |row| row.get(0),
        );

        let status_id = status_id.unwrap();

        let card = self.conn.query_row(
            "
        insert into cards (board_id, status_id, title, body) values (?, ?, ?, ?)
        returning id, inserted_at, updated_at;
        ",
            params![board_id, status_id, title, body],
            |row| {
                Ok(Card {
                    id: row.get(0)?,
                    title: title.to_string(),
                    body: body.to_string(),
                    inserted_at: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            },
        )?;

        Ok(card)
    }

    fn cards_for_column(&self, board_id: u64, column_name: &str) -> anyhow::Result<Vec<Card>> {
        let mut s = self.conn.prepare(
            "
            select
                cards.id,
                cards.title,
                cards.body,
                cards.inserted_at,
                cards.updated_at
            from cards
            inner join statuses
                on statuses.id = cards.status_id
                and statuses.board_id = ?1
                and statuses.name = ?2
            order by cards.id desc;
            ",
        )?;

        let cards_iter = s.query_map(params![board_id, column_name], |row| {
            Ok(Card {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                inserted_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;

        let mut cards = vec![];

        for card in cards_iter {
            cards.push(card?);
        }

        Ok(cards)
    }

    fn update_card(&mut self, card_id: u64, title: &str, body: &str) -> anyhow::Result<String> {
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

        let mut updated_at_s = self.conn.prepare(
            "
        select
            updated_at
        from cards
        where id = ?
        ",
        )?;

        let updated_at = updated_at_s.query_one([card_id], |row| row.get(0))?;

        Ok(updated_at)
    }

    fn set_card_status(
        &self,
        board_id: u64,
        card_id: u64,
        column_name: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "
        update cards
        set status_id = (
            select
                id
            from statuses
            where board_id = ?1
            and name = ?2
        )
        where id = ?3
        ",
            params![board_id, column_name, card_id],
        )?;

        Ok(())
    }

    fn create_board(&mut self, name: &str, column_names: &[&str]) -> anyhow::Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let board_id = {
            let mut board_s = tx.prepare(
                "
                insert into boards (name) values (?)
                returning id;
                ",
            )?;

            let mut columns_s = tx.prepare(
                "
                insert into statuses (name, column_order, board_id)
                values (?, ?, ?);
                ",
            )?;

            let board_id: u64 = board_s.query_row([name], |row| row.get(0))?;

            for (column_order, column_name) in column_names.iter().enumerate() {
                columns_s.execute(params![column_name, column_order, board_id])?;
            }

            board_id
        };

        tx.commit()?;

        Ok(board_id)
    }

    fn update_board_columns_order(
        &mut self,
        board_id: u64,
        board_name: &str,
        column_names: Vec<&str>,
    ) -> anyhow::Result<Board> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        {
            let mut change_column_order_s = tx.prepare(
                "
                insert into statuses (name, column_order, board_id)
                values (?, ?, ?)
                on conflict(name, board_id) do update set column_order = excluded.column_order;
                ",
            )?;

            let mut change_board_name_s = tx.prepare(
                "
            update boards
            set name = ?
            where id = ?
            ",
            )?;

            for (i, column_name) in column_names.iter().enumerate() {
                change_column_order_s.execute(params![column_name, i, board_id])?;
            }

            change_board_name_s.execute(params![board_name, board_id])?;
        }

        tx.commit()?;

        self.load_board(board_id)
    }

    fn load_most_recently_viewed_board(&self) -> anyhow::Result<Board> {
        let mut board_s = self.conn.prepare(
            "
        select
            id,
            name
        from boards
        order by viewed_at desc
        limit 1
        ",
        )?;

        let (board_id, board_name): (u64, String) =
            board_s.query_one([], |row| Ok((row.get(0)?, row.get(1)?)))?;

        let columns = self.get_cards_for_board(board_id)?;

        Ok(Board {
            id: board_id,
            name: board_name,
            columns,
        })
    }

    fn delete_card(&self, card_id: u64) -> anyhow::Result<()> {
        let mut s = self.conn.prepare(
            "
        delete from cards
        where id = ?",
        )?;

        s.execute([card_id])?;

        Ok(())
    }
}

#[derive(Debug)]
struct Board {
    id: u64,
    name: String,
    columns: Vec<Column>,
}

#[derive(Debug, Default, PartialEq)]
struct SelectedState {
    board_id: u64,
    board_index: Option<usize>,
    column_index: usize,
    card_index: Option<usize>,
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
    inserted_at: String,
    updated_at: String,
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
    ViewingBoards,
    ConfirmCardDeletion,
}

#[derive(Debug, PartialEq)]
enum Message {
    NavigateLeft,
    NavigateDown,
    NavigateUp,
    NavigateRight,
    Quit,
    NewCard,
    MoveCardMode,
    MoveCardLeft,
    // MoveCardDown,
    // MoveCardUp,
    MoveCardRight,
    EditCard,
    ViewBoardMode,
    ViewCardDetailMode,
    SetError(Option<String>),
    ViewBoardsMode,
    EditBoard,
    NewBoard,
    DeleteCard,
    ConfirmChoice,
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
    match model.mode {
        Mode::ViewingBoard
        | Mode::ViewingCardDetail
        | Mode::MovingCard
        | Mode::ConfirmCardDeletion => view_board(model, frame),
        Mode::ViewingBoards => view_boards(model, frame),
    }
}

fn view_boards(model: &mut Model, frame: &mut ratatui::Frame<'_>) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Max(99)])
        .split(frame.area());

    let mut state = ListState::default().with_selected(model.selected.board_index);

    let list_items = model
        .board_metas
        .iter()
        .map(|board| {
            ListItem::new(format!(
                "{:<30}{:<30}{:<30}{:<30}",
                &*board.name, &*board.updated_at, &*board.viewed_at, &*board.inserted_at
            ))
        })
        .collect::<Vec<_>>();

    const PINK: Color = Color::Rgb(255, 150, 167);

    let list = List::new(list_items)
        .highlight_symbol("> ")
        .highlight_style(Style::default().fg(PINK))
        .block(
            Block::new()
                .border_type(ratatui::widgets::BorderType::Rounded)
                .borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(Color::Black))
                .title(
                    "──name──────────────────────────last updated──────────────────last viewed───────────────────created",
                ),
        );

    frame.render_widget(Paragraph::new("Boards"), layout[0]);
    frame.render_stateful_widget(list, layout[1], &mut state);
}

fn view_board(model: &mut Model, frame: &mut ratatui::Frame) {
    if let Some(board) = &model.board {
        let [columns_layout, modeline_layout] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Max(3)])
            .areas(frame.area());

        let columns_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(std::iter::repeat_n(
                Constraint::Ratio(1, board.columns.len().try_into().unwrap()),
                board.columns.len(),
            ))
            .split(columns_layout);

        for (i, column) in board.columns.iter().enumerate() {
            let column_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Max(1), Constraint::Min(5)])
                .split(columns_layout[i]);

            frame.render_widget(Paragraph::new(&*column.name), column_layout[0]);

            let mut state = if model.selected.column_index == i {
                ListState::default().with_selected(model.selected.card_index)
            } else {
                ListState::default().with_selected(None)
            };

            let list_items = column
                .cards
                .iter()
                .map(|card| {
                    let s = format!("{} {}", card.id, card.title);
                    ListItem::new(Text::from(textwrap::fill(
                        &s,
                        (column_layout[1].width as usize).saturating_sub(2),
                    )))
                })
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
        }

        if model.mode == Mode::ViewingCardDetail
            && let Some(card) = model.selected_card()
        {
            let block = Block::bordered()
                .title(Line::from(card.id.to_string()).left_aligned())
                .title(
                    Line::from(format!(
                        "created {}, updated {}",
                        card.inserted_at, card.updated_at
                    ))
                    .right_aligned(),
                )
                .padding(Padding::uniform(1));

            let title_style = Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

            let area = popup_area(frame.area(), 60, 50);

            let wrapped = textwrap::wrap(&card.body, area.width as usize);

            let body = wrapped.iter().map(|line| Line::from(line.to_string()));

            let mut lines = vec![Line::styled(&*card.title, title_style)];
            lines.push(Line::from("\n\n"));
            lines.extend(body);

            let paragraph = Paragraph::new(lines).block(block);

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

        if model.mode == Mode::ConfirmCardDeletion
            && let Some(card) = model.selected_card()
        {
            let title_style = Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

            let block = Block::bordered()
                .title(format!("Delete {}", &card.title))
                .padding(Padding::uniform(1))
                .title_style(title_style);

            let area = popup_area(frame.area(), 30, 20);

            let [left, right] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .areas(area);

            let left_text = {
                let text = if model.confirmation_state == ConfirmationState::Yes {
                    "[ Delete ]"
                } else {
                    "Delete"
                };

                Text::from(text).centered()
            };

            let right_text = {
                let text = if model.confirmation_state == ConfirmationState::No {
                    "[ Cancel ]"
                } else {
                    "Cancel"
                };

                Text::from(text).centered()
            };

            // Text::from("Delete").centered();
            // let right_text = Text::from("Cancel").centered();

            let left = center(
                left,
                Constraint::Length(left_text.width() as u16),
                Constraint::Length(1),
            );

            let right = center(
                right,
                Constraint::Length(right_text.width() as u16),
                Constraint::Length(1),
            );

            frame.render_widget(ratatui::widgets::Clear, area); //this clears out the background
            frame.render_widget(left_text, left);
            frame.render_widget(right_text, right);
            frame.render_widget(block, area);

            fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
                let vertical =
                    Layout::vertical([Constraint::Percentage(percent_y)]).flex(Flex::Center);
                let horizontal =
                    Layout::horizontal([Constraint::Percentage(percent_x)]).flex(Flex::Center);
                let [area] = vertical.areas(area);
                let [area] = horizontal.areas(area);
                area
            }

            fn center(area: Rect, horizontal: Constraint, vertical: Constraint) -> Rect {
                let [area] = Layout::horizontal([horizontal])
                    .flex(Flex::Center)
                    .areas(area);
                let [area] = Layout::vertical([vertical]).flex(Flex::Center).areas(area);
                area
            }
        }

        let modeline_block = Block::new()
            .borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
            .title(
                Line::from(match model.mode {
                    Mode::ViewingBoard => "VIEWING BOARD",
                    Mode::ViewingCardDetail => "VIEWING CARD",
                    Mode::MovingCard => "MOVING CARD",
                    Mode::ConfirmCardDeletion => "DELETING CARD",
                    Mode::ViewingBoards => "VIEWING BOARDS",
                })
                .left_aligned(),
            )
            .title(Line::from(&*board.name).right_aligned());

        let modeline_text = {
            let mut modeline_text = String::new();

            if let Some(e) = &model.error {
                modeline_text.push_str(" - Error: ");
                modeline_text.push_str(&e.replace("\n", " "));
            } else {
                let formatted = match model.mode {
                    Mode::ViewingBoard => [
                        ("[h,j,k,l/arrows]", "move"),
                        ("[q]", "quit"),
                        ("[enter]", "view card"),
                        ("[m]", "move card"),
                        ("[n]", "new card"),
                        ("[e]", "edit card"),
                        ("[d]", "delete card"),
                        ("[b]", "view boards"),
                    ]
                    .iter()
                    .map(|(k, action)| format!("{} - {}", k, action))
                    .collect::<Vec<_>>(),
                    Mode::ViewingCardDetail => {
                        [("[enter/esc]", "close detail view"), ("[q]", "quit")]
                            .iter()
                            .map(|(k, action)| format!("{} - {}", k, action))
                            .collect::<Vec<_>>()
                    }
                    Mode::MovingCard => [
                        ("[h/left]", "move card left"),
                        ("[l/right]", "move card right"),
                        ("[q]", "quit"),
                        ("[m|enter|esc]", "close card detail view"),
                    ]
                    .iter()
                    .map(|(k, action)| format!("{} - {}", k, action))
                    .collect::<Vec<_>>(),
                    Mode::ViewingBoards => [
                        ("[j/down]", "down"),
                        ("[k/up]", "up"),
                        ("[enter]", "view board"),
                        ("[n]", "new board"),
                        ("[e]", "edit board"),
                        ("[q]", "quit"),
                    ]
                    .iter()
                    .map(|(k, action)| format!("{} - {}", k, action))
                    .collect::<Vec<_>>(),
                    Mode::ConfirmCardDeletion => [
                        ("[h/left]", "left"),
                        ("[l/right]", "right"),
                        ("[enter]", "confirm selection"),
                    ]
                    .iter()
                    .map(|(k, action)| format!("{} - {}", k, action))
                    .collect::<Vec<_>>(),
                };

                modeline_text.push_str(&formatted.join(" │ "));
            }

            modeline_text
        };

        let modeline = Paragraph::new(modeline_text).block(modeline_block);

        frame.render_widget(modeline, modeline_layout);
    }
}

/// Convert Event to Message
///
/// We don't need to pass in a `model` to this function in this example
/// but you might need it as your project evolves
fn receive_event(model: &Model) -> anyhow::Result<Option<Message>> {
    if crossterm::event::poll(Duration::from_millis(1000))?
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
        Event::KeyEvent(key) => match model.mode {
            Mode::ViewingBoard => match key.code {
                KeyCode::Char('h') | KeyCode::Left => Some(Message::NavigateLeft),
                KeyCode::Char('j') | KeyCode::Down => Some(Message::NavigateDown),
                KeyCode::Char('k') | KeyCode::Up => Some(Message::NavigateUp),
                KeyCode::Char('l') | KeyCode::Right => Some(Message::NavigateRight),
                KeyCode::Char('q') => Some(Message::Quit),
                KeyCode::Char('m') => Some(Message::MoveCardMode),
                KeyCode::Char('n') => Some(Message::NewCard),
                KeyCode::Char('e') => Some(Message::EditCard),
                KeyCode::Char('d') => Some(Message::DeleteCard),
                KeyCode::Char('b') => Some(Message::ViewBoardsMode),
                KeyCode::Enter => Some(Message::ViewCardDetailMode),
                _ => None,
            },
            Mode::MovingCard => match key.code {
                KeyCode::Char('h') | KeyCode::Left => Some(Message::MoveCardLeft),
                KeyCode::Char('l') | KeyCode::Right => Some(Message::MoveCardRight),
                KeyCode::Char('q') => Some(Message::Quit),
                KeyCode::Char('m') | KeyCode::Enter | KeyCode::Esc => Some(Message::ViewBoardMode),
                _ => None,
            },
            Mode::ConfirmCardDeletion => match key.code {
                KeyCode::Char('h') | KeyCode::Left => Some(Message::NavigateLeft),
                KeyCode::Char('l') | KeyCode::Right => Some(Message::NavigateRight),
                KeyCode::Enter => Some(Message::ConfirmChoice),
                _ => None,
            },
            Mode::ViewingCardDetail => match key.code {
                KeyCode::Enter | KeyCode::Esc => Some(Message::ViewBoardMode),
                KeyCode::Char('q') => Some(Message::Quit),
                _ => None,
            },
            Mode::ViewingBoards => match key.code {
                KeyCode::Char('j') | KeyCode::Down => Some(Message::NavigateDown),
                KeyCode::Char('k') | KeyCode::Up => Some(Message::NavigateUp),
                KeyCode::Char('n') => Some(Message::NewBoard),
                KeyCode::Char('e') => Some(Message::EditBoard),
                KeyCode::Char('q') => Some(Message::Quit),
                KeyCode::Enter => Some(Message::ViewBoardMode),
                _ => None,
            },
        },
        Event::InternalEvent(e) => match e {
            InternalEvent::ClearError => Some(Message::SetError(None)),
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
    update_with_run_editor_fn(model, msg, terminal, run_editor)
}

/// this exists only so we can mock out the run_editor function,
/// which in the real program actually opens the user's editor.
/// we can't do this in tests, so we need to mock it out
/// with a function that just returns whatever data
/// we tell it to, depending on the desired test condition
fn update_with_run_editor_fn<F, B>(
    model: &mut Model,
    msg: Message,
    terminal: &mut Terminal<B>,
    run_editor_fn: F,
) -> anyhow::Result<Option<Message>>
where
    F: Fn(&mut Terminal<B>, &str) -> anyhow::Result<String>,
    B: Backend,
{
    match model.mode {
        Mode::ViewingBoard => {
            match msg {
                Message::ViewBoardsMode => model.switch_to_viewing_boards_mode()?,
                Message::MoveCardMode => model.mode = Mode::MovingCard,
                Message::ViewCardDetailMode => {
                    if let Some(column) = model.selected_column()
                        && !column.cards.is_empty()
                    {
                        model.mode = Mode::ViewingCardDetail
                    }
                }
                Message::Quit => model.running_state = RunningState::Done,
                Message::NavigateLeft => model.navigate_left(),
                Message::NavigateDown => {
                    model.selected.card_index = model.selected.card_index.map(|i| {
                        min(
                            i.saturating_add(1),
                            model
                                .selected_column()
                                .map(|column| column.cards.len().saturating_sub(1))
                                .unwrap_or(usize::MAX),
                        )
                    })
                }
                Message::NavigateUp => {
                    model.selected.card_index =
                        model.selected.card_index.map(|i| i.saturating_sub(1))
                }
                Message::NavigateRight => model.navigate_right(),
                Message::NewCard => {
                    let raw_card_text =
                        run_editor_fn(terminal, "Title\n==========\n\nContent goes here")?;
                    let (title, body) = parse_raw_card_text(&raw_card_text)?;

                    let card = model
                        .repo
                        .insert_card(model.selected.board_id, title, body)?;

                    model.mode = Mode::ViewingBoard;
                    model.selected.column_index = 0;
                    model.selected.card_index = Some(0);

                    model.add_card_to_selected_column(card);
                }
                Message::EditCard => {
                    if let Some(card) = model.selected_card() {
                        let card_for_editor =
                            format!("{}\n==========\n\n{}", card.title, card.body);

                        let raw_card_text = run_editor_fn(terminal, &card_for_editor)?;

                        let (title, body) = parse_raw_card_text(&raw_card_text)?;

                        let updated_at = model.repo.update_card(card.id, title, body)?;

                        // dumb but necessary to reborrow because we previously borrow the model immutably
                        if let Some(card) = model.selected_card_mut() {
                            card.title = title.to_string();
                            card.body = body.to_string();
                            card.updated_at = updated_at;
                        }
                    }

                    model.mode = Mode::ViewingBoard;
                }
                Message::DeleteCard => model.confirm_card_delete()?,
                Message::SetError(e) => {
                    model.error = e;
                    let internal_event_tx = model.internal_event_tx.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                        let _ =
                            internal_event_tx.send(Event::InternalEvent(InternalEvent::ClearError));
                    });
                }
                m => panic!("unhandled message: {:?}", m),
            };
        }
        Mode::ViewingCardDetail => match msg {
            Message::ViewBoardMode => model.mode = Mode::ViewingBoard,
            m => panic!("unhandled message: {:?}", m),
        },
        Mode::MovingCard => match msg {
            Message::MoveCardLeft => move_selected_card_left(model)?,
            Message::MoveCardRight => move_selected_card_right(model)?,
            Message::ViewBoardMode => {
                model.mode = Mode::ViewingBoard;
                if let Some(board) = model.board.as_mut() {
                    for column in &mut board.columns {
                        column.cards.sort_unstable_by(|a, b| b.id.cmp(&a.id));
                    }
                }
            }
            m => panic!("unhandled message: {:?}", m),
        },
        Mode::ConfirmCardDeletion => match msg {
            Message::ConfirmChoice => match model.confirmation_state {
                ConfirmationState::Yes => {
                    model.delete_selected_card()?;
                    model.mode = Mode::ViewingBoard;
                    model.confirmation_state = ConfirmationState::No;
                }
                ConfirmationState::No => model.mode = Mode::ViewingBoard,
            },
            Message::NavigateLeft | Message::NavigateRight => model.toggle_confirmation_state(),
            Message::ViewBoardMode => model.mode = Mode::ViewingBoard,
            m => panic!("unhandled message: {:?}", m),
        },
        Mode::ViewingBoards => match msg {
            Message::NavigateUp => {
                model.selected.board_index =
                    model.selected.board_index.map(|i| i.saturating_sub(1));

                if let Some(board_index) = model.selected.board_index {
                    model.selected.board_id = model.board_metas[board_index].id;
                }
            }
            Message::NavigateDown => {
                model.selected.board_index = model
                    .selected
                    .board_index
                    .map(|i| min(model.board_metas.len().saturating_sub(1), i + 1));

                if let Some(board_index) = model.selected.board_index {
                    model.selected.board_id = model.board_metas[board_index].id;
                }
            }
            Message::NewBoard => {
                let raw_board_text = run_editor_fn(
                    terminal,
                    "Board Name\n==========\n\n- Column #1\n- Column #2\n- Column #3",
                )?;
                let (name, column_names) = parse_raw_board_text(&raw_board_text)?;

                // TODO
                // 1. create board, get board_id
                model.create_board(name, &column_names)?;
                // 2. insert columns, get columns ids
            }
            Message::EditBoard => {
                let selected_board = &model.board_metas[model.selected.board_index.unwrap()];
                let mut board_for_editor = format!("{}\n==========\n\n", selected_board.name);

                for column_name in &selected_board.columns {
                    board_for_editor.push_str("- ");
                    board_for_editor.push_str(column_name);
                    board_for_editor.push('\n');
                }

                let raw_board_text = run_editor_fn(terminal, &board_for_editor)?;
                let (name, column_names) = parse_raw_board_text(&raw_board_text)?;

                model.update_selected_board(name, column_names)?;
            }
            Message::ViewBoardMode => {
                model.mode = Mode::ViewingBoard;
                model.load_selected_board()?;
                model.board_metas = vec![];
            }
            Message::Quit => model.running_state = RunningState::Done,
            m => panic!("unhandled message: {:?}", m),
        },
    }

    Ok(None)
}

// TODO move this onto Model impl
fn move_selected_card_left(model: &mut Model) -> anyhow::Result<()> {
    if let Some(board) = &mut model.board
        && let Some(selected_card_index) = model.selected.card_index
    {
        let current_column_id = model.selected.column_index;
        let left_column_id = model.selected.column_index.saturating_sub(1);

        if left_column_id != current_column_id {
            let card = board.columns[current_column_id]
                .cards
                .remove(selected_card_index);

            model
                .repo
                .set_card_status(board.id, card.id, &board.columns[left_column_id].name)?;

            board.columns[left_column_id].cards.insert(0, card);

            model.selected.card_index = Some(0);

            model.selected.column_index = left_column_id;
        }
    }

    Ok(())
}

// TODO move this onto Model impl
fn move_selected_card_right(model: &mut Model) -> anyhow::Result<()> {
    if let Some(board) = &mut model.board
        && let Some(selected_card_index) = model.selected.card_index
    {
        let current_column_id = model.selected.column_index;
        let right_column_id = min(
            model.selected.column_index + 1,
            board.columns.len().saturating_sub(1),
        );

        if right_column_id != current_column_id {
            let card = board.columns[current_column_id]
                .cards
                .remove(selected_card_index);

            model
                .repo
                .set_card_status(board.id, card.id, &board.columns[right_column_id].name)?;

            board.columns[right_column_id].cards.insert(0, card);

            model.selected.card_index = Some(0);

            model.selected.column_index = right_column_id;
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

fn parse_raw_board_text(raw_board_text: &str) -> anyhow::Result<(&str, Vec<&str>)> {
    let board_regex = Regex::new(r#"(?<name>[^=\n]+)\n=+\n\n"#).unwrap();

    let columns_regex = Regex::new(r#"- (?<column>[^\n]+)"#).unwrap();

    let m_name = board_regex.captures(raw_board_text);
    let m_columns = columns_regex.captures_iter(raw_board_text);

    if let Some(captures_name) = m_name
        && let Some(name) = captures_name.name("name")
    {
        let mut columns = vec![];

        for cap in m_columns {
            if let Some(column) = cap.name("column") {
                columns.push(column.as_str())
            }
        }

        if columns.is_empty() {
            return Err(anyhow!("could not parse raw board text: bad columns"));
        }

        Ok((name.as_str(), columns))
    } else {
        Err(anyhow!("could not parse raw board text: bad board name"))
    }
}

#[derive(Parser)]
#[command(author, version, about, name = "kk")]
struct Options {
    #[arg(short, long, env)]
    database_path: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let options = Options::parse();

    let mut terminal = ratatui::init();

    let mut model = Model::new(options)?;

    if let Some(board) = &model.board
        && let Some(first_column) = board.columns.first()
        && !first_column.cards.is_empty()
    {
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use rusqlite::{OptionalExtension, params};

    use crate::{
        Card, Column, ConfirmationState, Mode, Model, Options, Repo, RunningState, update,
        update_with_run_editor_fn,
    };

    impl Model {
        fn create_column(&mut self, column_name: &str) -> anyhow::Result<()> {
            if let Some(board) = &mut self.board {
                let column = self.repo.create_column_for_board(board.id, column_name)?;
                if !board
                    .columns
                    .iter()
                    .any(|column| column.name == column_name)
                {
                    board.columns.push(column)
                }
            } else {
                return Err(anyhow::anyhow!("No board selected"));
            }

            Ok(())
        }
    }

    impl Repo {
        fn create_column_for_board(
            &self,
            board_id: u64,
            column_name: &str,
        ) -> anyhow::Result<Column> {
            let mut latest_column_order_s = self.conn.prepare(
                "
        select
            column_order
        from statuses
        where board_id = ?
        order by column_order desc
        limit 1
        ",
            )?;

            let mut statuses_s = self.conn.prepare(
                "
        insert into statuses (name, column_order, board_id)
        values (?, ?, ?)
        on conflict do nothing;
        ",
            )?;

            let column_order: Option<u64> = latest_column_order_s
                .query_one([board_id], |row| row.get(0))
                .optional()?;

            let column_order = column_order.unwrap_or_default();

            statuses_s.execute(params![column_name, column_order, board_id])?;

            Ok(Column {
                name: column_name.to_string(),
                cards: vec![],
            })
        }
    }

    /// right now, we don't care about comparing whether cards
    /// have the same inserted_at and updated_at.
    ///
    /// we don't even use PartialEq in application code
    impl PartialEq for Card {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id && self.title == other.title && self.body == other.body
        }
    }

    mod create_board {
        use ratatui::Terminal;

        use crate::{Model, Options, RunningState, update, update_with_run_editor_fn};

        #[test]
        fn with_zero_columns() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_board("Board1", &["Todo"]).unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::ViewBoardsMode, &mut terminal).unwrap();

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::NewBoard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns invalid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Some Board Name\n==========\n\n".to_string())
                },
            );

            assert!(update_result.is_err());

            assert_eq!(model.running_state, RunningState::Running);
        }

        #[test]
        fn with_at_least_one_column() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_board("Board1", &["Todo"]).unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::ViewBoardsMode, &mut terminal).unwrap();

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::NewBoard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns invalid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Some Board Name\n==========\n\n- Todo".to_string())
                },
            );

            assert!(update_result.is_ok());

            assert_eq!(model.running_state, RunningState::Running);
        }
    }

    mod new_card {
        use crate::{Card, Model, Options, RunningState, update_with_run_editor_fn};
        use ratatui::Terminal;

        #[test]
        fn with_bad_input() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_column("Todo").unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::NewCard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns invalid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("bad input".to_string())
                },
            );

            assert!(update_result.is_err());

            assert_eq!(model.running_state, RunningState::Running);
        }

        #[test]
        fn with_valid_input() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_column("Todo").unwrap();

            assert_eq!(model.selected.column_index, 0);
            assert_eq!(model.selected.card_index, None);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::NewCard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns valid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Valid Title\n==========\n\nValid card body".to_string())
                },
            );

            assert!(update_result.is_ok());

            assert_eq!(
                model.board.unwrap().columns[0].cards,
                vec![Card {
                    id: 1,
                    title: "Valid Title".to_string(),
                    body: "Valid card body".to_string(),
                    inserted_at: "".to_string(),
                    updated_at: "".to_string(),
                }]
            );

            assert_eq!(model.selected.column_index, 0);
            assert_eq!(model.selected.card_index, Some(0));

            assert_eq!(
                model.repo.cards_for_column(1, "Todo").unwrap(),
                vec![Card {
                    id: 1,
                    title: "Valid Title".to_string(),
                    body: "Valid card body".to_string(),
                    inserted_at: "".to_string(),
                    updated_at: "".to_string(),
                }]
            );

            assert_eq!(model.running_state, RunningState::Running);
        }
    }

    mod edit_card {
        use crate::{Card, Model, Options, RunningState, update_with_run_editor_fn};
        use ratatui::Terminal;

        #[test]
        fn with_bad_input() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_column("Todo").unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::NewCard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns valid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Valid Title\n==========\n\nValid card body".to_string())
                },
            );

            assert!(update_result.is_ok());

            model.selected.column_index = 0;
            model.selected.card_index = Some(0);

            assert_eq!(model.selected.column_index, 0);
            assert_eq!(model.selected.card_index, Some(0));

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::EditCard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns invalid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Bad input".to_string())
                },
            );

            assert!(update_result.is_err());

            assert_eq!(
                model.board.unwrap().columns[0].cards,
                vec![Card {
                    id: 1,
                    title: "Valid Title".to_string(),
                    body: "Valid card body".to_string(),
                    inserted_at: "".to_string(),
                    updated_at: "".to_string(),
                }]
            );

            assert_eq!(model.selected.column_index, 0);
            assert_eq!(model.selected.card_index, Some(0));

            assert_eq!(
                model.repo.cards_for_column(1, "Todo").unwrap(),
                vec![Card {
                    id: 1,
                    title: "Valid Title".to_string(),
                    body: "Valid card body".to_string(),
                    inserted_at: "".to_string(),
                    updated_at: "".to_string(),
                }]
            );

            assert_eq!(model.running_state, RunningState::Running);
        }

        #[test]
        fn with_valid_input() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_column("Todo").unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::NewCard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns valid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Valid Title\n==========\n\nValid card body".to_string())
                },
            );

            assert!(update_result.is_ok());

            model.selected.column_index = 0;
            model.selected.card_index = Some(0);

            assert_eq!(model.selected.column_index, 0);
            assert_eq!(model.selected.card_index, Some(0));

            let update_result = update_with_run_editor_fn(
                &mut model,
                crate::Message::EditCard,
                &mut terminal,
                // replace default run_editor_fn with a stub that returns valid data
                |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                    Ok("Valid Title\n==========\n\nValid card body".to_string())
                },
            );

            assert!(update_result.is_ok());

            assert_eq!(
                model.board.unwrap().columns[0].cards,
                // model.board.unwrap().columns["Todo"],
                vec![Card {
                    id: 1,
                    title: "Valid Title".to_string(),
                    body: "Valid card body".to_string(),
                    inserted_at: "".to_string(),
                    updated_at: "".to_string(),
                }]
            );

            assert_eq!(model.selected.column_index, 0);
            assert_eq!(model.selected.card_index, Some(0));

            assert_eq!(
                model.repo.cards_for_column(1, "Todo").unwrap(),
                vec![Card {
                    id: 1,
                    title: "Valid Title".to_string(),
                    body: "Valid card body".to_string(),
                    inserted_at: "".to_string(),
                    updated_at: "".to_string(),
                }]
            );

            assert_eq!(model.running_state, RunningState::Running);
        }
    }

    #[test]
    fn update_quit() {
        let mut model = Model::new(Options {
            database_path: Some(":memory:".into()),
        })
        .unwrap();

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

        update(&mut model, crate::Message::Quit, &mut terminal).unwrap();

        assert_eq!(model.running_state, RunningState::Done);
    }

    mod navigate_left {
        use crate::{Board, Card, Column, Model, Options, RunningState, SelectedState, update};

        #[test]
        fn when_left() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateLeft, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: None
                }
            );
        }

        #[test]
        fn when_right_with_card() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![
                    Column {
                        name: "Todo".to_string(),
                        cards: vec![Card {
                            id: 1,
                            title: "great card".to_string(),
                            body: "great body".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        }],
                    },
                    Column {
                        name: "Doing".to_string(),
                        cards: vec![Card {
                            id: 2,
                            title: "title 2".to_string(),
                            body: "body 2".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        }],
                    },
                ],
            });

            model.selected.column_index = 1;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateLeft, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: Some(0)
                }
            );
        }

        #[test]
        fn when_right_without_card() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![
                    Column {
                        name: "Todo".to_string(),
                        cards: vec![],
                    },
                    Column {
                        name: "Doing".to_string(),
                        cards: vec![Card {
                            id: 2,
                            title: "title 2".to_string(),
                            body: "body 2".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        }],
                    },
                ],
            });

            model.selected.column_index = 1;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateLeft, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: None
                }
            );
        }
    }

    mod navigate_right {
        use crate::{Board, Card, Column, Model, Options, RunningState, SelectedState, update};

        #[test]
        fn when_right() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateRight, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: None
                }
            );
        }

        #[test]
        fn when_left_with_card() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![
                    Column {
                        name: "Todo".to_string(),
                        cards: vec![Card {
                            id: 1,
                            title: "great card".to_string(),
                            body: "great body".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        }],
                    },
                    Column {
                        name: "Doing".to_string(),
                        cards: vec![Card {
                            id: 2,
                            title: "title 2".to_string(),
                            body: "body 2".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        }],
                    },
                ],
            });

            model.selected.column_index = 0;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateRight, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 1,
                    card_index: Some(0)
                }
            );
        }

        #[test]
        fn when_left_without_card() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![
                    Column {
                        name: "Todo".to_string(),
                        cards: vec![Card {
                            id: 2,
                            title: "title 2".to_string(),
                            body: "body 2".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        }],
                    },
                    Column {
                        name: "Doing".to_string(),
                        cards: vec![],
                    },
                ],
            });

            model.selected.column_index = 1;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateRight, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 1,
                    card_index: Some(0)
                }
            );
        }
    }

    mod navigate_down {
        use crate::{Board, Card, Column, Model, Options, RunningState, SelectedState, update};

        #[test]
        fn when_length_is_one() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![Column {
                    name: "Todo".to_string(),
                    cards: vec![Card {
                        id: 2,
                        title: "title 2".to_string(),
                        body: "body 2".to_string(),
                        inserted_at: "".to_string(),
                        updated_at: "".to_string(),
                    }],
                }],
            });

            model.selected.column_index = 0;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateDown, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: Some(0)
                }
            );
        }

        #[test]
        fn when_length_is_greater_than_one() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![Column {
                    name: "Todo".to_string(),
                    cards: vec![
                        Card {
                            id: 1,
                            title: "title 1".to_string(),
                            body: "body 1".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        },
                        Card {
                            id: 2,
                            title: "title 2".to_string(),
                            body: "body 2".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        },
                    ],
                }],
            });

            model.selected.column_index = 0;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateDown, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: Some(1)
                }
            );
        }
    }

    mod navigate_up {
        use crate::{Board, Card, Column, Model, Options, RunningState, SelectedState, update};

        #[test]
        fn when_length_is_one() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![Column {
                    name: "Todo".to_string(),
                    cards: vec![Card {
                        id: 2,
                        title: "title 2".to_string(),
                        body: "body 2".to_string(),
                        inserted_at: "".to_string(),
                        updated_at: "".to_string(),
                    }],
                }],
            });

            model.selected.column_index = 0;
            model.selected.card_index = Some(0);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateUp, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: Some(0)
                }
            );
        }

        #[test]
        fn when_length_is_greater_than_one() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.board = Some(Board {
                id: 1,
                name: "Board".to_string(),
                columns: vec![Column {
                    name: "Todo".to_string(),
                    cards: vec![
                        Card {
                            id: 1,
                            title: "title 1".to_string(),
                            body: "body 1".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        },
                        Card {
                            id: 2,
                            title: "title 2".to_string(),
                            body: "body 2".to_string(),
                            inserted_at: "".to_string(),
                            updated_at: "".to_string(),
                        },
                    ],
                }],
            });

            model.selected.column_index = 0;
            model.selected.card_index = Some(1);

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::NavigateUp, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(
                model.selected,
                SelectedState {
                    board_id: 1,
                    board_index: None,
                    column_index: 0,
                    card_index: Some(0)
                }
            );
        }
    }

    mod switch_to_moving_mode {
        use crate::{Mode, Model, Options, RunningState, update};

        #[test]
        fn switches() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoard);

            update(&mut model, crate::Message::MoveCardMode, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(model.mode, Mode::MovingCard);
        }
    }

    mod switch_to_view_card_detail_mode {
        use crate::{Card, Mode, Model, Options, RunningState, update};

        #[test]
        fn switches_when_column_is_not_empty() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_column("Todo").unwrap();

            model.add_card_to_selected_column(Card {
                id: 1,
                title: "Title".to_string(),
                body: "Body".to_string(),
                inserted_at: "".to_string(),
                updated_at: "".to_string(),
            });

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoard);

            update(
                &mut model,
                crate::Message::ViewCardDetailMode,
                &mut terminal,
            )
            .unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(model.mode, Mode::ViewingCardDetail);
        }

        #[test]
        fn does_not_switch_when_column_is_empty() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoard);

            update(
                &mut model,
                crate::Message::ViewCardDetailMode,
                &mut terminal,
            )
            .unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(model.mode, Mode::ViewingBoard);
        }
    }

    mod switch_to_viewing_board_mode {
        use crate::{Mode, Model, Options, RunningState, update};

        #[test]
        fn switches() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            model.mode = Mode::ViewingCardDetail;

            update(&mut model, crate::Message::ViewBoardMode, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(model.mode, Mode::ViewingBoard);

            model.mode = Mode::MovingCard;

            update(&mut model, crate::Message::ViewBoardMode, &mut terminal).unwrap();

            assert_eq!(model.running_state, RunningState::Running);
            assert_eq!(model.mode, Mode::ViewingBoard);
        }
    }

    mod boards_view {
        use crate::{Mode, Model, Options, update};

        #[test]
        fn navigate_down_with_one_board() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_board("Board1", &["Todo"]).unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::ViewBoardsMode, &mut terminal).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoards);

            assert_eq!(model.selected.board_index, Some(0));

            assert!(model.board.is_none());

            update(&mut model, crate::Message::NavigateDown, &mut terminal).unwrap();

            assert_eq!(model.selected.board_index, Some(0));
        }

        #[test]
        fn navigate_down_with_two_boards() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_board("Board1", &["Todo"]).unwrap();
            model.create_board("Board2", &["Todo"]).unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::ViewBoardsMode, &mut terminal).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoards);

            assert_eq!(model.selected.board_index, Some(0));

            assert!(model.board.is_none());

            update(&mut model, crate::Message::NavigateDown, &mut terminal).unwrap();

            assert_eq!(model.selected.board_index, Some(1));
        }

        #[test]
        fn navigate_up_with_one_board() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_board("Board1", &["Todo"]).unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::ViewBoardsMode, &mut terminal).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoards);

            assert_eq!(model.selected.board_index, Some(0));

            assert!(model.board.is_none());

            update(&mut model, crate::Message::NavigateUp, &mut terminal).unwrap();

            assert_eq!(model.selected.board_index, Some(0));
        }

        #[test]
        fn navigate_up_with_two_boards() {
            let mut model = Model::new(Options {
                database_path: Some(":memory:".into()),
            })
            .unwrap();

            model.create_board("Board1", &["Todo"]).unwrap();
            model.create_board("Board2", &["Todo"]).unwrap();

            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

            update(&mut model, crate::Message::ViewBoardsMode, &mut terminal).unwrap();

            assert_eq!(model.mode, Mode::ViewingBoards);

            assert_eq!(model.selected.board_index, Some(0));

            assert!(model.board.is_none());

            update(&mut model, crate::Message::NavigateDown, &mut terminal).unwrap();
            assert_eq!(model.selected.board_index, Some(1));
            update(&mut model, crate::Message::NavigateUp, &mut terminal).unwrap();
            assert_eq!(model.selected.board_index, Some(0));
        }
    }

    #[test]
    fn delete_card() {
        let mut model = Model::new(Options {
            database_path: Some(":memory:".into()),
        })
        .unwrap();

        model.create_column("Todo").unwrap();
        // model.create_board("Board1", &["Todo"]).unwrap();

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 80)).unwrap();

        let update_result = update_with_run_editor_fn(
            &mut model,
            crate::Message::NewCard,
            &mut terminal,
            // replace default run_editor_fn with a stub that returns valid data
            |_terminal: &mut Terminal<ratatui::backend::TestBackend>, _template: &str| {
                Ok("Valid Title\n==========\n\nValid card body".to_string())
            },
        );

        assert!(update_result.is_ok());

        let card = model.selected_card().unwrap();

        assert_eq!(
            &Card {
                id: 1,
                title: "Valid Title".to_string(),
                body: "Valid card body".to_string(),
                inserted_at: "".to_string(),
                updated_at: "".to_string()
            },
            card
        );

        let column = model.selected_column().unwrap();
        assert!(!column.cards.is_empty());

        update(&mut model, crate::Message::DeleteCard, &mut terminal).unwrap();
        assert_eq!(model.confirmation_state, ConfirmationState::No);
        assert_eq!(model.mode, Mode::ConfirmCardDeletion);

        let column = model.selected_column().unwrap();
        assert!(!column.cards.is_empty());

        update(&mut model, crate::Message::NavigateLeft, &mut terminal).unwrap();
        assert_eq!(model.confirmation_state, ConfirmationState::Yes);

        update(&mut model, crate::Message::ConfirmChoice, &mut terminal).unwrap();
        let column = model.selected_column().unwrap();
        assert!(column.cards.is_empty());
    }
}
