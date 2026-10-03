use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, terminal,
};
use shoal_value::Value;
use std::{
    cmp::Reverse,
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
    ops::Range,
};
use unicode_segmentation::UnicodeSegmentation;
#[derive(Debug, Clone)]
pub struct Options {
    pub multi: bool,
    pub height: usize,
    pub prompt: String,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            multi: false,
            height: 12,
            prompt: "> ".into(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    Toggle,
    Accept,
    Cancel,
    Backspace,
    Input(char),
}
#[derive(Debug, Clone)]
pub struct Ranked {
    pub original: usize,
    pub score: i64,
    pub positions: Vec<usize>,
    pub display: String,
}
pub struct Model {
    values: Vec<Value>,
    texts: Vec<String>,
    pub query: String,
    pub ranked: Vec<Ranked>,
    pub cursor: usize,
    selected: BTreeSet<usize>,
    multi: bool,
    page: usize,
}
impl Model {
    pub fn new(values: Vec<Value>, options: &Options) -> Self {
        let texts = values.iter().map(display).collect();
        let mut m = Self {
            values,
            texts,
            query: String::new(),
            ranked: vec![],
            cursor: 0,
            selected: BTreeSet::new(),
            multi: options.multi,
            page: options.height.max(1),
        };
        m.refine("");
        m
    }
    pub fn refine(&mut self, q: &str) {
        self.query = q.into();
        self.ranked = self
            .texts
            .iter()
            .enumerate()
            .filter_map(|(original, text)| {
                score(q, text).map(|(score, positions)| Ranked {
                    original,
                    score,
                    positions,
                    display: text.clone(),
                })
            })
            .collect();
        self.ranked.sort_by_key(|r| (Reverse(r.score), r.original));
        self.cursor = self.cursor.min(self.ranked.len().saturating_sub(1))
    }
    pub fn apply(&mut self, a: Action) -> Option<Result<Vec<Value>, ()>> {
        match a {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => {
                self.cursor = (self.cursor + 1).min(self.ranked.len().saturating_sub(1))
            }
            Action::PageUp => self.cursor = self.cursor.saturating_sub(self.page),
            Action::PageDown => {
                self.cursor = (self.cursor + self.page).min(self.ranked.len().saturating_sub(1))
            }
            Action::Toggle => {
                if self.multi
                    && let Some(r) = self.ranked.get(self.cursor)
                    && !self.selected.insert(r.original)
                {
                    self.selected.remove(&r.original);
                }
            }
            Action::Backspace => {
                self.query.pop();
                let q = self.query.clone();
                self.refine(&q)
            }
            Action::Input(c) => {
                self.query.push(c);
                let q = self.query.clone();
                self.refine(&q)
            }
            Action::Cancel => return Some(Err(())),
            Action::Accept => {
                let ids = if self.multi && !self.selected.is_empty() {
                    self.selected.iter().copied().collect()
                } else {
                    self.ranked
                        .get(self.cursor)
                        .map(|r| vec![r.original])
                        .unwrap_or_default()
                };
                return Some(Ok(ids
                    .into_iter()
                    .map(|i| self.values[i].clone())
                    .collect()));
            }
        }
        None
    }
    pub fn selected(&self, original: usize) -> bool {
        self.selected.contains(&original)
    }

    /// Ranked rows visible around the cursor. Navigation may span the complete
    /// result set, while drawing retains only this fixed-height window.
    pub fn visible_range(&self, height: usize) -> Range<usize> {
        let height = height.max(1);
        let start = if self.cursor < height {
            0
        } else {
            self.cursor + 1 - height
        };
        start..(start + height).min(self.ranked.len())
    }
}
pub fn score(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, vec![]));
    }
    let q = query
        .graphemes(true)
        .map(|g| g.to_lowercase())
        .collect::<Vec<_>>();
    let c = candidate.graphemes(true).collect::<Vec<_>>();
    let mut qi = 0;
    let mut positions = vec![];
    let mut total = 0i64;
    let mut last = None;
    for (i, g) in c.iter().enumerate() {
        if qi < q.len() && g.to_lowercase() == q[qi] {
            positions.push(i);
            total += 10;
            if i == 0 || c[i - 1].chars().all(|x| !x.is_alphanumeric()) {
                total += 8
            }
            if last == Some(i.saturating_sub(1)) {
                total += 12
            }
            total -= i as i64;
            last = Some(i);
            qi += 1
        }
    }
    if qi == q.len() {
        total -= c.len() as i64;
        Some((total, positions))
    } else {
        None
    }
}
pub fn highlight(text: &str, positions: &[usize]) -> String {
    let set = positions.iter().copied().collect::<BTreeSet<_>>();
    text.graphemes(true)
        .enumerate()
        .map(|(i, g)| {
            if set.contains(&i) {
                format!("\x1b[1;36m{g}\x1b[0m")
            } else {
                g.into()
            }
        })
        .collect()
}
pub trait PickerInput {
    fn into_picker_values(self) -> Vec<Value>;
}
impl PickerInput for Vec<Value> {
    fn into_picker_values(self) -> Vec<Value> {
        self
    }
}
impl PickerInput for Value {
    fn into_picker_values(self) -> Vec<Value> {
        match self {
            Value::List(v) => v,
            Value::Table(rows) => rows.into_iter().map(Value::Record).collect(),
            v @ Value::Record(_) => vec![v],
            v => vec![v],
        }
    }
}
pub fn pick(values: impl PickerInput, options: Options) -> io::Result<Vec<Value>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "picker requires an interactive terminal",
        ));
    }
    let mut guard = TerminalGuard::enter()?;
    let mut model = Model::new(values.into_picker_values(), &options);
    loop {
        draw(&mut guard.out, &model, &options)?;
        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let a = match k.code {
                KeyCode::Up => Action::Up,
                KeyCode::Down => Action::Down,
                KeyCode::PageUp => Action::PageUp,
                KeyCode::PageDown => Action::PageDown,
                KeyCode::Tab => Action::Toggle,
                KeyCode::Enter => Action::Accept,
                KeyCode::Esc => Action::Cancel,
                KeyCode::Backspace => Action::Backspace,
                KeyCode::Char(c) => Action::Input(c),
                _ => continue,
            };
            if let Some(done) = model.apply(a) {
                return done
                    .map_err(|()| io::Error::new(io::ErrorKind::Interrupted, "picker cancelled"));
            }
        }
    }
}
struct TerminalGuard {
    out: io::Stdout,
}
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut out = io::stdout();
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(out, terminal::EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Self { out })
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.out, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
fn draw(out: &mut impl Write, m: &Model, o: &Options) -> io::Result<()> {
    execute!(
        out,
        cursor::MoveTo(0, 0),
        terminal::Clear(terminal::ClearType::All)
    )?;
    write!(out, "{}{}\r\n", o.prompt, m.query)?;
    let visible = m.visible_range(o.height);
    for (offset, r) in m.ranked[visible.clone()].iter().enumerate() {
        let index = visible.start + offset;
        write!(
            out,
            "{}{} {}\r\n",
            if index == m.cursor { "❯" } else { " " },
            if m.selected(r.original) { "●" } else { " " },
            highlight(&r.display, &r.positions)
        )?
    }
    out.flush()
}
fn display(v: &Value) -> String {
    match v {
        Value::Record(r) => r
            .iter()
            .map(|(k, v)| format!("{k}: {}", shoal_value::render::render_inline(v)))
            .collect::<Vec<_>>()
            .join("  "),
        _ => shoal_value::render::render_inline(v),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_ties() {
        let m = Model::new(
            vec![Value::Str("ab".into()), Value::Str("ab".into())],
            &Options::default(),
        );
        assert_eq!(
            m.ranked.iter().map(|r| r.original).collect::<Vec<_>>(),
            [0, 1]
        )
    }
    #[test]
    fn contiguous_and_boundary_rank() {
        let m = Model::new(
            vec![Value::Str("abacus".into()), Value::Str("a___b".into())],
            &Options::default(),
        );
        let mut m = m;
        m.refine("ab");
        assert_eq!(m.ranked[0].original, 0)
    }
    #[test]
    fn unicode_highlight_safe() {
        let (s, p) = score("雪", "a雪🦀").unwrap();
        assert!(s != 0);
        assert_eq!(highlight("a雪🦀", &p), "a\x1b[1;36m雪\x1b[0m🦀")
    }
    #[test]
    fn multi_preserves_original_values() {
        let mut m = Model::new(
            vec![Value::Int(1), Value::Int(2)],
            &Options {
                multi: true,
                ..Default::default()
            },
        );
        m.apply(Action::Toggle);
        m.apply(Action::Down);
        m.apply(Action::Toggle);
        let out = m.apply(Action::Accept).unwrap().unwrap();
        assert_eq!(out, vec![Value::Int(1), Value::Int(2)])
    }
    #[test]
    fn refine_and_cancel() {
        let mut m = Model::new(
            vec![Value::Str("alpha".into()), Value::Str("beta".into())],
            &Options::default(),
        );
        m.apply(Action::Input('b'));
        assert_eq!(m.ranked.len(), 1);
        assert!(m.apply(Action::Cancel).unwrap().is_err())
    }
    #[test]
    fn table_input_preserves_records_and_columns() {
        let mut row = shoal_value::Record::new();
        row.insert("name".into(), Value::Str("shoal".into()));
        row.insert("n".into(), Value::Int(2));
        let values = Value::Table(vec![row.clone()]).into_picker_values();
        assert_eq!(values, vec![Value::Record(row)]);
    }

    #[test]
    fn viewport_follows_cursor_through_rows_and_pages() {
        let values = (0..10).map(Value::Int).collect();
        let mut model = Model::new(
            values,
            &Options {
                height: 3,
                ..Options::default()
            },
        );
        assert_eq!(model.visible_range(3), 0..3);
        model.apply(Action::PageDown);
        assert_eq!(model.cursor, 3);
        assert_eq!(model.visible_range(3), 1..4);
        model.apply(Action::Down);
        assert_eq!(model.visible_range(3), 2..5);
        model.apply(Action::PageDown);
        assert_eq!(model.cursor, 7);
        assert_eq!(model.visible_range(3), 5..8);
        model.apply(Action::PageDown);
        assert_eq!(model.cursor, 9);
        assert_eq!(model.visible_range(3), 7..10);
    }

    #[test]
    fn draw_marks_the_global_cursor_inside_scrolled_window() {
        let mut model = Model::new(
            (0..6).map(Value::Int).collect(),
            &Options {
                height: 2,
                ..Options::default()
            },
        );
        model.apply(Action::PageDown);
        model.apply(Action::Down);
        let mut output = Vec::new();
        draw(
            &mut output,
            &model,
            &Options {
                height: 2,
                ..Options::default()
            },
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("❯  3"));
        assert!(!output.contains("  0\r\n"));
    }
}
