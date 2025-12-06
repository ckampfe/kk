use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout};
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

        setup_database(&mut conn)?;

        let mut this = Self { conn };

        this.insert_board("my great board")?;

        Ok(this)
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

    fn insert_card(&self, title: &str, body: &str, board_id: u64) -> anyhow::Result<Card> {
        let card = self.conn.query_row(
            "
        insert into cards (title, body, board_id) values (?, ?, ?)
        returning id
        ",
            params![title, body, board_id],
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

    fn todo_cards(&self) -> anyhow::Result<Vec<Card>> {
        let mut s = self.conn.prepare(
            "
        select
            id,
            title,
            body
        from cards where status = 'todo'
        order by id desc;
        ",
        )?;

        let cards_iter = s.query_map([], |row| {
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

    fn doing_cards(&self) -> anyhow::Result<Vec<Card>> {
        let mut s = self.conn.prepare(
            "
        select
            id,
            title,
            body
        from cards where status = 'doing'
        order by id desc;
        ",
        )?;

        let cards_iter = s.query_map([], |row| {
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

    fn done_cards(&self) -> anyhow::Result<Vec<Card>> {
        let mut s = self.conn.prepare(
            "
        select
            id,
            title,
            body
        from cards where status = 'done'
        order by id desc;
        ",
        )?;

        let cards_iter = s.query_map([], |row| {
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

    fn update_card(&self, id: u64, title: &str, body: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "
        update cards
        set 
            title = ?2,
            body = ?3
        where id = ?1
        ",
            params![id, title, body],
        )?;

        Ok(())
    }

    fn set_card_status(&self, card_id: u64, doing: Column) -> anyhow::Result<()> {
        self.conn.execute(
            "
        update cards
        set status = ?2
        where id = ?1
        ",
            params![card_id, doing.to_string()],
        )?;

        Ok(())
    }
}

#[derive(Debug)]
struct Model {
    current_board: u64,
    columns: Vec<Column>,
    selected: SelectedState,
    working_state: WorkingState,
    running_state: RunningState,
    repo: Repo,
}

#[derive(Debug)]
struct Column {
    name: String,
    cards: Vec<Card>,
}

impl Model {
    fn new(options: Options) -> anyhow::Result<Self> {
        let repo = Repo::new(options.database_path)?;

        let todo_cards = repo.todo_cards()?;
        let doing_cards = repo.doing_cards()?;
        let done_cards = repo.done_cards()?;

        Ok(Self {
            // TODO
            current_board: 1,
            todo_cards,
            doing_cards,
            done_cards,
            selected: SelectedState {
                column: Column::Todo,
                card_index: None,
            },
            working_state: WorkingState::ViewingBoard,
            running_state: RunningState::Running,
            repo,
        })
    }

    fn current_cards(&self) -> &[Card] {
        &self.columns[self.selected.column_id].cards
    }

    fn current_cards_mut(&mut self) -> &mut Vec<Card> {
        &mut self.columns[self.selected.column_id].cards
    }
}

#[derive(Debug, Default)]
struct SelectedState {
    column_id: usize,
    card_index: Option<usize>,
}

impl Display for Column {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Column::Todo => "Todo",
                Column::Doing => "Doing",
                Column::Done => "Done",
            }
        )
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

#[derive(Debug, Default)]
enum WorkingState {
    #[default]
    ViewingBoard,
    ViewingCardDetail,
    MovingCard,
    // Editing,
}

#[derive(PartialEq)]
enum Message {
    NavigateLeft,
    NavigateDown,
    NavigateUp,
    NavigateRight,
    Quit,
    NewCard,
    Enter,
    SwitchToMovingState,
    SwitchToViewingState,
    MoveCardLeft,
    MoveCardDown,
    MoveCardUp,
    MoveCardRight,
    EditCard,
    SwitchToViewingBoardState,
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

    Command::new("vim").arg(&path).status()?;

    let edited_text = std::fs::read_to_string(&path)?;

    path.close()?;

    std::io::stdout().execute(crossterm::terminal::EnterAlternateScreen)?;
    crossterm::terminal::enable_raw_mode()?;
    terminal.clear()?;

    Ok(edited_text)
}

fn view(model: &mut Model, frame: &mut ratatui::Frame) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(frame.area());

    for (i, column_name) in [Column::Todo, Column::Doing, Column::Done]
        .into_iter()
        .enumerate()
    {
        let column = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Max(1), Constraint::Min(5)])
            .split(columns[i]);

        frame.render_widget(Paragraph::new(column_name.to_string()), column[0]);

        let cards = match column_name {
            Column::Todo => &model.todo_cards,
            Column::Doing => &model.doing_cards,
            Column::Done => &model.done_cards,
        };

        let mut state = if model.selected.column == column_name {
            ListState::default().with_selected(model.selected.card_index)
        } else {
            ListState::default().with_selected(None)
        };

        let list_items = cards
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
        frame.render_stateful_widget(list, column[1], &mut state);
    }
}

/// Convert Event to Message
///
/// We don't need to pass in a `model` to this function in this example
/// but you might need it as your project evolves
fn handle_event(model: &Model) -> anyhow::Result<Option<Message>> {
    if crossterm::event::poll(Duration::from_millis(250))?
        && let crossterm::event::Event::Key(key) = crossterm::event::read()?
        && key.kind == crossterm::event::KeyEventKind::Press
    {
        return Ok(handle_key(key, model));
    }
    Ok(None)
}

fn handle_key(key: crossterm::event::KeyEvent, model: &Model) -> Option<Message> {
    match key.code {
        KeyCode::Char('h') => match model.working_state {
            WorkingState::ViewingBoard => Some(Message::NavigateLeft),
            WorkingState::MovingCard => Some(Message::MoveCardLeft),
            WorkingState::ViewingCardDetail => None,
        },
        KeyCode::Char('j') => match model.working_state {
            WorkingState::ViewingBoard => Some(Message::NavigateDown),
            WorkingState::MovingCard => {
                // TODO
                // Some(Message::MoveCardDown)
                None
            }
            WorkingState::ViewingCardDetail => None,
        },
        KeyCode::Char('k') => match model.working_state {
            WorkingState::ViewingBoard => Some(Message::NavigateUp),
            WorkingState::MovingCard => {
                // TODO
                // Some(Message::MoveCardUp)
                None
            }
            WorkingState::ViewingCardDetail => None,
        },
        KeyCode::Char('l') => match model.working_state {
            WorkingState::ViewingBoard => Some(Message::NavigateRight),
            WorkingState::MovingCard => Some(Message::MoveCardRight),
            WorkingState::ViewingCardDetail => None,
        },
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Char('m') => match model.working_state {
            WorkingState::ViewingBoard => Some(Message::SwitchToMovingState),
            WorkingState::ViewingCardDetail => None,
            WorkingState::MovingCard => Some(Message::SwitchToViewingBoardState),
        },
        KeyCode::Char('n') => Some(Message::NewCard),
        KeyCode::Char('e') => match model.working_state {
            WorkingState::ViewingBoard => Some(Message::EditCard),
            WorkingState::ViewingCardDetail => None,
            WorkingState::MovingCard => todo!(),
        },
        KeyCode::Enter => Some(Message::Enter),
        KeyCode::Esc => match model.working_state {
            WorkingState::ViewingBoard => None,
            WorkingState::ViewingCardDetail => Some(Message::SwitchToViewingState),
            WorkingState::MovingCard => None,
        },
        _ => None,
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
        Message::NavigateLeft => match model.selected.column {
            Column::Todo => (),
            Column::Doing => {
                model.selected.column = Column::Todo;
                if model.todo_cards.is_empty() {
                    model.selected.card_index = None;
                } else if model.doing_cards.len() < model.todo_cards.len() {
                    model.selected.card_index = Some(model.doing_cards.len().saturating_sub(1));
                }
            }
            Column::Done => model.selected.column = Column::Doing,
        },
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
            if let Ok(raw_card_text) =
                run_editor(terminal, "Title\n==========\n\nContent goes here")
                && let Some((title, body)) = parse_raw_card_text(&raw_card_text)
            {
                let card = model.repo.insert_card(title, body, model.current_board)?;

                model.todo_cards.push(card);

                model.todo_cards.sort_unstable_by(|a, b| b.id.cmp(&a.id));

                model.working_state = WorkingState::ViewingBoard;
            }
        }
        Message::Enter => match model.working_state {
            WorkingState::ViewingBoard => model.working_state = WorkingState::ViewingCardDetail,
            WorkingState::ViewingCardDetail => todo!(),
            WorkingState::MovingCard => todo!(),
        },
        Message::SwitchToMovingState => {
            model.working_state = WorkingState::MovingCard;
            // select current card
            // cause moves to move the card between columns
        }
        Message::MoveCardLeft => move_card_left(model)?,
        Message::MoveCardDown => (),
        Message::MoveCardUp => (),
        Message::MoveCardRight => move_card_right(model)?,
        Message::SwitchToViewingState => todo!(),
        Message::EditCard => {
            if let Some(card_index) = model.selected.card_index {
                let card = match model.selected.column {
                    Column::Todo => &mut model.todo_cards[card_index],
                    Column::Doing => &mut model.doing_cards[card_index],
                    Column::Done => &mut model.done_cards[card_index],
                };

                let card_for_editor = format!("{}\n==========\n\n{}", card.title, card.body);

                if let Ok(raw_card_text) = run_editor(terminal, &card_for_editor)
                    && let Some((title, body)) = parse_raw_card_text(&raw_card_text)
                {
                    model.repo.update_card(card.id, title, body)?;
                    card.title = title.to_string();
                    card.body = body.to_string();
                } else {
                    panic!("could not update")
                };
            }

            model.working_state = WorkingState::ViewingBoard;
        }
        Message::SwitchToViewingBoardState => model.working_state = WorkingState::ViewingBoard,
    };

    Ok(None)
}

// fn move_card_up(model: &mut Model) -> anyhow::Result<()> {
//     if let Some(i) = model.selected.card_index {
//         let cards = model.current_cards_mut();
//         let card = cards.remove(i);
//         cards.insert(i.saturating_sub(1), card);
//         model.selected.card_index = model.selected.card_index.map(|i| i.saturating_sub(1))
//     }

//     Ok(())
// }

// fn move_card_down(model: &mut Model) -> anyhow::Result<()> {
//     Ok(())
// }

fn navigate_right(model: &mut Model) {
    match model.selected.column {
        Column::Todo => {
            if model.doing_cards.is_empty() {
                // model.selected.card_index = None;
            } else if model.doing_cards.len() < model.todo_cards.len() {
                model.selected.column = Column::Doing;
                model.selected.card_index = Some(model.doing_cards.len().saturating_sub(1));
            }
        }
        Column::Doing => {
            if model.done_cards.is_empty() {
                // model.selected.card_index = None;
            } else if model.done_cards.len() < model.doing_cards.len() {
                model.selected.column = Column::Done;
                model.selected.card_index = Some(model.done_cards.len().saturating_sub(1));
            }
        }
        Column::Done => (),
    }
}

// fn move_selected_column(model: &mut Model, direction: ColumnMoveDirection) {
//     match direction {
//         ColumnMoveDirection::Right => match model.selected.column {
//             Column::Todo => model.selected.column = Column::Doing,
//             Column::Doing => model.selected.column = Column::Done,
//             Column::Done => (),
//         },
//         ColumnMoveDirection::Left => match model.selected.column {
//             Column::Todo => (),
//             Column::Doing => model.selected.column = Column::Todo,
//             Column::Done => model.selected.column = Column::Doing,
//         },
//     }
// }

fn move_card_left(model: &mut Model) -> anyhow::Result<()> {
    let from_to = match model.selected.column {
        Column::Todo => None,
        Column::Doing => {
            model.selected.column = Column::Todo;
            Some((&mut model.doing_cards, &mut model.todo_cards))
        }
        Column::Done => {
            model.selected.column = Column::Doing;
            Some((&mut model.done_cards, &mut model.doing_cards))
        }
    };

    if let Some(i) = model.selected.card_index
        && let Some((from_cards, to_cards)) = from_to
    {
        let card = from_cards.remove(i);

        model.repo.set_card_status(card.id, model.selected.column)?;

        model.selected.card_index = model
            .selected
            .card_index
            .map(|i| min(to_cards.len().saturating_sub(1), i));

        to_cards.insert(model.selected.card_index.unwrap(), card);
    }

    Ok(())
}

fn move_card_right(model: &mut Model) -> anyhow::Result<()> {
    let from_to = match model.selected.column {
        Column::Todo => {
            model.selected.column = Column::Doing;
            Some((&mut model.todo_cards, &mut model.doing_cards))
        }
        Column::Doing => {
            model.selected.column = Column::Done;
            Some((&mut model.doing_cards, &mut model.done_cards))
        }
        Column::Done => None,
    };

    if let Some(i) = model.selected.card_index
        && let Some((from_cards, to_cards)) = from_to
    {
        let card = from_cards.remove(i);

        model.repo.set_card_status(card.id, model.selected.column)?;

        model.selected.card_index = model
            .selected
            .card_index
            .map(|i| min(to_cards.len().saturating_sub(1), i));

        to_cards.insert(model.selected.card_index.unwrap(), card);
    }

    Ok(())
}

fn parse_raw_card_text(raw_card_text: &str) -> Option<(&str, &str)> {
    let card_regex = Regex::new(r#"(?<title>[^=\n]+)\n=+\n\n(?<body>.*)"#).unwrap();

    let m = card_regex.captures(raw_card_text);

    if let Some(captures) = m
        && let Some(title) = captures.name("title")
        && let Some(body) = captures.name("body")
    {
        Some((title.as_str(), body.as_str()))
    } else {
        None
    }
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

    create table if not exists cards (
        id integer primary key,
        board_id integer not null,
        title text not null,
        status text not null default 'todo',
        body text not null,
        doing_at timestamp,
        done_at timestamp,
        inserted_at timestamp not null default current_timestamp,
        updated_at timestamp not null default current_timestamp,

        foreign key(board_id) references boards(id)
    );

    create unique index if not exists boards_name on boards (name);
    ",
    )?;
    Ok(())
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

    // model.todo_cards.push(Card {
    //     id: 1,
    //     title: "make the corners rounder".to_string(),
    //     body:
    //         "in order to get more customers we need corners that are rounder\nthis is the only way"
    //             .to_string(),
    // });

    // model.todo_cards.push(Card {
    //     id: 2,
    //     title: "it should be faster".to_string(),
    //     body: "the customers are not happy that it is slow".to_string(),
    // });

    // model.doing_cards.push(Card {
    //     id: 4,
    //     title: "making it better".to_string(),
    //     body: "it needs to be better".to_string(),
    // });

    // model.done_cards.push(Card {
    //     id: 3,
    //     title: "make it popular".to_string(),
    //     body: "spread the word far and wide".to_string(),
    // });

    if !model.todo_cards.is_empty() {
        model.selected.card_index = Some(0);
    }

    while model.running_state != RunningState::Done {
        // Render the current view
        terminal.draw(|f| view(&mut model, f))?;

        // Handle events and map to a Message
        let mut current_msg = handle_event(&model)?;

        // Process updates as long as they return a non-None message
        while let Some(m) = current_msg {
            current_msg = update(&mut model, m, &mut terminal)?;
        }
    }

    ratatui::restore();

    Ok(())
}
