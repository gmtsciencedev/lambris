//! The script language: what a session says, written down so it can be run
//! again on other data.
//!
//! A script is a *declaration*, not a recording of keystrokes. Each line states
//! something that is true of the finished view — this column is hidden, these
//! rows are masked, it is sorted like so — rather than an action that only
//! means something from a particular starting point. Order matters exactly
//! where it genuinely does: a rename changes the name later lines must use, a
//! computed column may refer to an earlier one, and reading the header
//! differently changes what every other line refers to.
//!
//! What is in a script is what an export would carry out: the values, as
//! arranged. Widths, frozen columns, the row-number gutter, numeric styles and
//! the summary line cannot change a written file, so they are not here — the
//! line is the one `X` already draws.

use std::fmt;

use anyhow::{Result, bail};

use crate::pattern::{
    Pattern, SavedColumn, SavedCrop, SavedHeader, SavedJoin, SavedMasks, SavedSort, SavedSortKey,
    Session, SessionTab,
};

/// One statement. Handles name tabs, so nothing depends on the order tabs
/// happen to sit in — which is what makes a script readable and editable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Open {
        path: String,
        sheet: Option<String>,
        handle: String,
    },
    /// Which row holds the column names, 1-based; `None` for a file whose
    /// first row is already data.
    Header {
        handle: String,
        row: Option<usize>,
    },
    Extract {
        handle: String,
        name: String,
        from: String,
        pattern: String,
    },
    Compute {
        handle: String,
        name: String,
        formula: String,
    },
    Rename {
        handle: String,
        from: String,
        to: String,
    },
    /// Exactly these columns, in this order. Anything else the file holds is
    /// left out.
    Columns {
        handle: String,
        names: Vec<String>,
    },
    /// These columns out, everything else through — so a file that gains a
    /// column still shows it.
    Hide {
        handle: String,
        names: Vec<String>,
    },
    Sort {
        handle: String,
        column: String,
        descending: bool,
        /// Characters `from..to`, 1-based and inclusive, as `sort -k` writes
        /// them, with how to compare them.
        key: Option<(usize, usize, String)>,
    },
    Filter {
        handle: String,
        regex: String,
    },
    /// Every row holding one of these values in that column.
    MaskValues {
        handle: String,
        column: String,
        values: Vec<String>,
    },
    /// Rows by their number in the file, 1-based. Only as stable as the file.
    MaskRows {
        handle: String,
        rows: Vec<usize>,
    },
    Join {
        left: (String, String),
        right: (String, String),
        handle: String,
    },
    Crop {
        source: String,
        rows: CropRows,
        /// The columns kept, by name and in order; all of them when empty.
        /// Named rather than given as a range, the way a crop is already
        /// remembered: a name is a name, and what comes out is the same set
        /// however the file has been added to.
        cols: Vec<String>,
        handle: String,
    },
    Transpose {
        handle: String,
    },
    Export {
        handle: String,
        path: String,
    },
}

/// The rows a crop covers, named the way the view was ordered when it was
/// taken. No count goes with them: a script is a rule, and the whole point of
/// running it again is that the data has changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CropRows {
    /// From one value to another in a column the tab was sorted by.
    Between {
        column: String,
        from: String,
        to: String,
    },
    /// Row numbers, 1-based, for a tab in the file's own order.
    Numbers { from: usize, to: usize },
}

/// A script, and where each statement came from, so a complaint can point at
/// the line that caused it.
#[derive(Debug)]
pub struct Script {
    pub commands: Vec<(usize, Command)>,
}

impl Script {
    /// Read a script, with `args` standing in for `$1`, `$2` and so on.
    ///
    /// Arguments are put in after the text has been broken into words, never
    /// before: a path holding a quote or a space is then just a path, and
    /// cannot turn into extra words or an unbalanced string.
    pub fn parse(text: &str, args: &[String]) -> Result<Self> {
        let mut commands = Vec::new();
        for (line, statement) in statements(text) {
            let tokens = tokenise(&statement, line)?;
            if tokens.is_empty() {
                continue;
            }
            let tokens = substitute(tokens, args, line)?;
            let command = parse_statement(tokens)
                .map_err(|e| anyhow::anyhow!("line {line}: {e}"))?;
            commands.push((line, command));
        }
        Ok(Self { commands })
    }
}

impl fmt::Display for Script {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (_, command) in &self.commands {
            writeln!(f, "{command}")?;
        }
        Ok(())
    }
}

/// Statements, with the line each began on. A newline or a `;` ends one; a `#`
/// outside quotes starts a comment.
fn statements(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        // A shebang is for the shell, not for us.
        if line == 1 && raw.starts_with("#!") {
            continue;
        }
        let mut current = String::new();
        let mut quoted = false;
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    quoted = !quoted;
                    current.push(c);
                }
                '\\' if quoted => {
                    current.push(c);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                '#' if !quoted => break,
                ';' if !quoted => {
                    out.push((line, std::mem::take(&mut current)));
                }
                _ => current.push(c),
            }
        }
        out.push((line, current));
    }
    out.retain(|(_, s)| !s.trim().is_empty());
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Text(String),
    Regex(String),
    Range,
    Equals,
}

impl Token {
    /// A token used as a name: a bare word or a quoted string, since a column
    /// may well be called `External ID`.
    fn name(&self) -> Option<&str> {
        match self {
            Token::Word(w) | Token::Text(w) => Some(w),
            _ => None,
        }
    }
}

fn tokenise(statement: &str, line: usize) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = statement.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '"' => {
                let mut text = String::new();
                i += 1;
                loop {
                    let Some(&c) = chars.get(i) else {
                        bail!("line {line}: a quoted value is not closed");
                    };
                    i += 1;
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(&next) = chars.get(i) {
                                text.push(next);
                                i += 1;
                            }
                        }
                        _ => text.push(c),
                    }
                }
                tokens.push(Token::Text(text));
            }
            '/' => {
                let mut text = String::new();
                i += 1;
                loop {
                    let Some(&c) = chars.get(i) else {
                        bail!("line {line}: a /pattern/ is not closed");
                    };
                    i += 1;
                    match c {
                        '/' => break,
                        '\\' if chars.get(i) == Some(&'/') => {
                            text.push('/');
                            i += 1;
                        }
                        _ => text.push(c),
                    }
                }
                tokens.push(Token::Regex(text));
            }
            '=' => {
                tokens.push(Token::Equals);
                i += 1;
            }
            '.' if chars.get(i + 1) == Some(&'.') => {
                tokens.push(Token::Range);
                i += 2;
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.get(i) {
                    // A word runs to whitespace or to something that is a token
                    // in its own right. A single `.` stays in — `meta.sample` is
                    // one word — but `..` ends it.
                    if c.is_whitespace() || matches!(c, '"' | '/' | '=') {
                        break;
                    }
                    if c == '.' && chars.get(i + 1) == Some(&'.') {
                        break;
                    }
                    word.push(c);
                    i += 1;
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

/// Put the run's arguments in for `$1`, `$2`, … A whole token is replaced, so
/// what arrives is a value and never more syntax.
fn substitute(tokens: Vec<Token>, args: &[String], line: usize) -> Result<Vec<Token>> {
    let value = |name: &str| -> Result<String> {
        let n: usize = name[1..]
            .parse()
            .map_err(|_| anyhow::anyhow!("line {line}: {name} is not an argument number"))?;
        match n.checked_sub(1).and_then(|i| args.get(i)) {
            Some(arg) => Ok(arg.clone()),
            None => bail!("line {line}: {name} was not given — this script takes {n} argument(s)"),
        }
    };
    tokens
        .into_iter()
        .map(|token| match &token {
            Token::Word(w) | Token::Text(w) if w.starts_with('$') && w.len() > 1 => {
                Ok(Token::Text(value(w)?))
            }
            _ => Ok(token),
        })
        .collect()
}

/// A statement's tokens, taken from the front as it is read.
struct Cursor {
    tokens: std::vec::IntoIter<Token>,
    peeked: Option<Token>,
}

impl Cursor {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens: tokens.into_iter(),
            peeked: None,
        }
    }

    fn next(&mut self) -> Option<Token> {
        self.peeked.take().or_else(|| self.tokens.next())
    }

    fn peek(&mut self) -> Option<&Token> {
        if self.peeked.is_none() {
            self.peeked = self.tokens.next();
        }
        self.peeked.as_ref()
    }

    fn name(&mut self, what: &str) -> Result<String> {
        match self.next() {
            Some(token) => match token.name() {
                Some(name) => Ok(name.to_string()),
                None => bail!("expected {what}"),
            },
            None => bail!("expected {what}"),
        }
    }

    fn number(&mut self, what: &str) -> Result<usize> {
        let text = self.name(what)?;
        text.parse()
            .map_err(|_| anyhow::anyhow!("{what} should be a number, not {text}"))
    }

    fn regex(&mut self) -> Result<String> {
        match self.next() {
            Some(Token::Regex(re)) => Ok(re),
            Some(Token::Text(re)) => Ok(re),
            _ => bail!("expected a /pattern/"),
        }
    }

    /// Take `word` if it is next, and say whether it was.
    fn took(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(w)) if w == word) {
            self.next();
            return true;
        }
        false
    }

    fn expect(&mut self, word: &str) -> Result<()> {
        match self.took(word) {
            true => Ok(()),
            false => bail!("expected `{word}`"),
        }
    }

    /// Names up to `stop`, which is left for the caller.
    fn names_until(&mut self, stop: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        while let Some(token) = self.peek() {
            if matches!(token, Token::Word(w) if w == stop) {
                break;
            }
            names.push(self.name("a name")?);
        }
        Ok(names)
    }

    /// Every remaining name, for the commands that take a list.
    fn rest(&mut self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        while self.peek().is_some() {
            names.push(self.name("a name")?);
        }
        match names.is_empty() {
            true => bail!("expected at least one name"),
            false => Ok(names),
        }
    }

    fn done(&mut self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(token) => bail!("unexpected {}", describe(token)),
        }
    }
}

fn describe(token: &Token) -> String {
    match token {
        Token::Word(w) => format!("`{w}`"),
        Token::Text(t) => format!("\"{t}\""),
        Token::Regex(r) => format!("/{r}/"),
        Token::Range => "`..`".to_string(),
        Token::Equals => "`=`".to_string(),
    }
}

fn parse_statement(tokens: Vec<Token>) -> Result<Command> {
    let mut c = Cursor::new(tokens);
    let verb = c.name("a command")?;
    let command = match verb.as_str() {
        "open" => {
            let path = c.name("a file to open")?;
            let sheet = c.took("sheet").then(|| c.name("a sheet name")).transpose()?;
            c.expect("as")?;
            Command::Open {
                path,
                sheet,
                handle: c.name("a name for it")?,
            }
        }
        "header" => {
            let handle = c.name("a tab")?;
            let row = match c.took("none") {
                true => None,
                false => {
                    c.expect("row")?;
                    Some(c.number("a row number")?)
                }
            };
            Command::Header { handle, row }
        }
        "extract" => {
            let handle = c.name("a tab")?;
            let name = c.name("a name for the new column")?;
            c.expect("from")?;
            let from = c.name("the column to read")?;
            Command::Extract {
                handle,
                name,
                from,
                pattern: c.regex()?,
            }
        }
        "compute" => {
            let handle = c.name("a tab")?;
            let name = c.name("a name for the new column")?;
            match c.next() {
                Some(Token::Equals) => {}
                _ => bail!("expected `=` and a formula"),
            }
            Command::Compute {
                handle,
                name,
                formula: c.name("a formula")?,
            }
        }
        "rename" => {
            let handle = c.name("a tab")?;
            let from = c.name("the column to rename")?;
            c.expect("to")?;
            Command::Rename {
                handle,
                from,
                to: c.name("its new name")?,
            }
        }
        "columns" => {
            let handle = c.name("a tab")?;
            Command::Columns {
                handle,
                names: c.rest()?,
            }
        }
        "hide" => {
            let handle = c.name("a tab")?;
            Command::Hide {
                handle,
                names: c.rest()?,
            }
        }
        "sort" => {
            let handle = c.name("a tab")?;
            let column = c.name("a column")?;
            let descending = match c.took("desc") {
                true => true,
                false => {
                    c.took("asc");
                    false
                }
            };
            let mut key = None;
            if c.took("chars") {
                let from = c.number("the first character")?;
                match c.next() {
                    Some(Token::Range) => {}
                    _ => bail!("expected `..` between the character offsets"),
                }
                let to = c.number("the last character")?;
                let method = match c.peek() {
                    Some(Token::Word(_)) => c.name("how to compare")?,
                    _ => "abc".to_string(),
                };
                if !matches!(method.as_str(), "abc" | "num" | "nat") {
                    bail!("`{method}` is not a comparison — abc, num or nat");
                }
                key = Some((from, to, method));
            }
            Command::Sort {
                handle,
                column,
                descending,
                key,
            }
        }
        "filter" => Command::Filter {
            handle: c.name("a tab")?,
            regex: c.regex()?,
        },
        "mask" => {
            let handle = c.name("a tab")?;
            match c.took("rows") {
                true => {
                    let mut rows = Vec::new();
                    while c.peek().is_some() {
                        rows.push(c.number("a row number")?);
                    }
                    if rows.is_empty() {
                        bail!("expected at least one row number");
                    }
                    Command::MaskRows { handle, rows }
                }
                false => {
                    let column = c.name("a column")?;
                    Command::MaskValues {
                        handle,
                        column,
                        values: c.rest()?,
                    }
                }
            }
        }
        "join" => {
            let left = side(&mut c)?;
            c.expect("with")?;
            let right = side(&mut c)?;
            c.expect("as")?;
            Command::Join {
                left,
                right,
                handle: c.name("a name for it")?,
            }
        }
        "crop" => {
            let source = c.name("a tab")?;
            c.expect("rows")?;
            let rows = crop_rows(&mut c)?;
            let cols = match c.took("cols") {
                false => Vec::new(),
                true => c.names_until("as")?,
            };
            c.expect("as")?;
            Command::Crop {
                source,
                rows,
                cols,
                handle: c.name("a name for it")?,
            }
        }
        "transpose" => Command::Transpose {
            handle: c.name("a tab")?,
        },
        "export" => {
            let handle = c.name("a tab")?;
            c.expect("to")?;
            Command::Export {
                handle,
                path: c.name("a file to write")?,
            }
        }
        other => bail!("`{other}` is not a command"),
    };
    c.done()?;
    Ok(command)
}

/// `handle.column`, one side of a join.
fn side(c: &mut Cursor) -> Result<(String, String)> {
    let text = c.name("a tab and column, as tab.column")?;
    // `reads."sample name"`: a quoted column arrives as a token of its own,
    // leaving the tab and its dot behind.
    if let Some(tab) = text.strip_suffix('.')
        && !tab.is_empty()
        && matches!(c.peek(), Some(Token::Text(_)))
    {
        let column = c.name("a column")?;
        return Ok((tab.to_string(), column));
    }
    match text.split_once('.') {
        Some((tab, col)) if !tab.is_empty() && !col.is_empty() => {
            Ok((tab.to_string(), col.to_string()))
        }
        _ => bail!("`{text}` should be tab.column"),
    }
}

fn crop_rows(c: &mut Cursor) -> Result<CropRows> {
    // `rows 2..6` counts rows; `rows sample "a".."b"` reads values.
    let first = c.name("a column or a row number")?;
    if let Some(Token::Range) = c.peek() {
        c.next();
        // The first is read first, so a complaint names the end that is wrong.
        let from = first
            .parse()
            .map_err(|_| anyhow::anyhow!("`{first}` is not a row number"))?;
        return Ok(CropRows::Numbers {
            from,
            to: c.number("the last row")?,
        });
    }
    let from = c.name("the value the run starts at")?;
    match c.next() {
        Some(Token::Range) => {}
        _ => bail!("expected `..` between the two values"),
    }
    Ok(CropRows::Between {
        column: first,
        from,
        to: c.name("the value the run ends at")?,
    })
}

/// A name as a script would write it: bare when it can be, quoted when it holds
/// anything that would otherwise be read as more syntax.
fn quoted(text: &str) -> String {
    let plain = !text.is_empty()
        && !text.starts_with('$')
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
        && !text.contains("..");
    match plain {
        true => text.to_string(),
        false => format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")),
    }
}

fn list(names: &[String]) -> String {
    names.iter().map(|n| quoted(n)).collect::<Vec<_>>().join(" ")
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Open {
                path,
                sheet,
                handle,
            } => {
                write!(f, "open {}", quoted(path))?;
                if let Some(sheet) = sheet {
                    write!(f, " sheet {}", quoted(sheet))?;
                }
                write!(f, " as {handle}")
            }
            Command::Header { handle, row } => match row {
                Some(row) => write!(f, "header {handle} row {row}"),
                None => write!(f, "header {handle} none"),
            },
            Command::Extract {
                handle,
                name,
                from,
                pattern,
            } => write!(
                f,
                "extract {handle} {} from {} /{pattern}/",
                quoted(name),
                quoted(from)
            ),
            Command::Compute {
                handle,
                name,
                formula,
            } => write!(f, "compute {handle} {} = \"{}\"", quoted(name), formula.replace('"', "\\\"")),
            Command::Rename { handle, from, to } => {
                write!(f, "rename {handle} {} to {}", quoted(from), quoted(to))
            }
            Command::Columns { handle, names } => {
                write!(f, "columns {handle} {}", list(names))
            }
            Command::Hide { handle, names } => write!(f, "hide {handle} {}", list(names)),
            Command::Sort {
                handle,
                column,
                descending,
                key,
            } => {
                write!(f, "sort {handle} {}", quoted(column))?;
                if *descending {
                    write!(f, " desc")?;
                }
                match key {
                    Some((from, to, method)) => write!(f, " chars {from}..{to} {method}"),
                    None => Ok(()),
                }
            }
            Command::Filter { handle, regex } => write!(f, "filter {handle} /{regex}/"),
            Command::MaskValues {
                handle,
                column,
                values,
            } => write!(f, "mask {handle} {} {}", quoted(column), list(values)),
            Command::MaskRows { handle, rows } => {
                let rows: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
                write!(f, "mask {handle} rows {}", rows.join(" "))
            }
            Command::Join {
                left,
                right,
                handle,
            } => write!(
                f,
                "join {}.{} with {}.{} as {handle}",
                left.0,
                quoted(&left.1),
                right.0,
                quoted(&right.1)
            ),
            Command::Crop {
                source,
                rows,
                cols,
                handle,
            } => {
                write!(f, "crop {source} rows ")?;
                match rows {
                    CropRows::Between { column, from, to } => write!(
                        f,
                        "{} {}..{}",
                        quoted(column),
                        quoted(from),
                        quoted(to)
                    )?,
                    CropRows::Numbers { from, to } => write!(f, "{from}..{to}")?,
                }
                if !cols.is_empty() {
                    write!(f, " cols {}", list(cols))?;
                }
                write!(f, " as {handle}")
            }
            Command::Transpose { handle } => write!(f, "transpose {handle}"),
            Command::Export { handle, path } => {
                write!(f, "export {handle} to {}", quoted(path))
            }
        }
    }
}


/// What a script asks for, in the shape the viewer already knows how to build:
/// a session, plus the files to write once it stands.
pub struct Plan {
    pub session: Session,
    /// Which tab each handle became, in the order they were named.
    pub handles: Vec<(String, usize)>,
    /// Exports to carry out, in the order the script asked for them.
    pub exports: Vec<(usize, String)>,
}

impl Script {
    /// Work out what the script describes.
    ///
    /// A script and a session say the same things, so this translates rather
    /// than interprets: what comes out is fed to the very code that reopens a
    /// session, and the two therefore cannot drift apart.
    pub fn plan(&self, folder: &str) -> Result<Plan> {
        let mut session = Session {
            folder: folder.to_string(),
            tabs: Vec::new(),
            current: 0,
        };
        let mut handles: Vec<(String, usize)> = Vec::new();
        let mut exports = Vec::new();
        let find = |handles: &[(String, usize)], name: &str, line: usize| -> Result<usize> {
            handles
                .iter()
                .find(|(h, _)| h == name)
                .map(|(_, i)| *i)
                .ok_or_else(|| anyhow::anyhow!("line {line}: no tab called `{name}`"))
        };
        for (line, command) in &self.commands {
            let line = *line;
            // Everything but `open`, `join` and `crop` shapes a tab that is
            // already named, so the handle is looked up once here.
            let handle = match command {
                Command::Open { .. } => None,
                // A join names two tabs and neither is "the" one; they are
                // looked up again where the join is made.
                Command::Join { .. } => None,
                Command::Crop { source, .. } => Some(find(&handles, source, line)?),
                Command::Header { handle, .. }
                | Command::Extract { handle, .. }
                | Command::Compute { handle, .. }
                | Command::Rename { handle, .. }
                | Command::Columns { handle, .. }
                | Command::Hide { handle, .. }
                | Command::Sort { handle, .. }
                | Command::Filter { handle, .. }
                | Command::MaskValues { handle, .. }
                | Command::MaskRows { handle, .. }
                | Command::Transpose { handle }
                | Command::Export { handle, .. } => Some(find(&handles, handle, line)?),
            };
            let fresh = |session: &mut Session,
                             handles: &mut Vec<(String, usize)>,
                             name: &str,
                             tab: SessionTab|
             -> Result<()> {
                if handles.iter().any(|(h, _)| h == name) {
                    bail!("line {line}: `{name}` already names a tab");
                }
                handles.push((name.to_string(), session.tabs.len()));
                session.tabs.push(tab);
                Ok(())
            };
            match command {
                Command::Open {
                    path,
                    sheet,
                    handle,
                } => fresh(
                    &mut session,
                    &mut handles,
                    handle,
                    SessionTab {
                        file: path.clone(),
                        sheet: sheet.clone(),
                        ..SessionTab::default()
                    },
                )?,
                Command::Join {
                    left,
                    right,
                    handle: name,
                } => {
                    let sides = (find(&handles, &left.0, line)?, find(&handles, &right.0, line)?);
                    fresh(
                        &mut session,
                        &mut handles,
                        name,
                        SessionTab {
                            join: Some(SavedJoin {
                                left: sides.0,
                                right: sides.1,
                                left_key: left.1.clone(),
                                right_key: right.1.clone(),
                            }),
                            ..SessionTab::default()
                        },
                    )?
                }
                Command::Crop {
                    rows,
                    cols,
                    handle: name,
                    ..
                } => {
                    let source = handle.expect("looked up above");
                    let (column, from, to) = match rows {
                        CropRows::Between { column, from, to } => {
                            (column.clone(), from.clone(), to.clone())
                        }
                        CropRows::Numbers { from, to } => {
                            (String::new(), from.to_string(), to.to_string())
                        }
                    };
                    fresh(
                        &mut session,
                        &mut handles,
                        name,
                        SessionTab {
                            crop: Some(SavedCrop {
                                source,
                                column,
                                from,
                                to,
                                // No count: a script is a rule, and the point of
                                // running it again is that the data has moved on.
                                rows: 0,
                                columns: cols.clone(),
                            }),
                            ..SessionTab::default()
                        },
                    )?
                }
                Command::Export { path, .. } => {
                    exports.push((handle.expect("looked up above"), path.clone()))
                }
                shaping => {
                    let tab = handle.expect("looked up above");
                    shape(&mut session.tabs[tab], shaping)?;
                }
            }
        }
        Ok(Plan {
            session,
            handles,
            exports,
        })
    }
}

/// Fold one shaping command into the arrangement its tab will be given.
fn shape(tab: &mut SessionTab, command: &Command) -> Result<()> {
    let view: &mut Pattern = &mut tab.view;
    match command {
        Command::Header { row, .. } => {
            view.header = Some(match row {
                Some(row) => SavedHeader {
                    skip: row.saturating_sub(1),
                    named: true,
                },
                None => SavedHeader {
                    skip: 0,
                    named: false,
                },
            })
        }
        Command::Extract {
            name,
            from,
            pattern,
            ..
        } => view.computed.push(SavedColumn {
            name: name.clone(),
            from: Some(from.clone()),
            extract: Some(pattern.clone()),
            formula: None,
        }),
        Command::Compute { name, formula, .. } => view.computed.push(SavedColumn {
            name: name.clone(),
            from: None,
            extract: None,
            formula: Some(formula.clone()),
        }),
        Command::Rename { from, to, .. } => {
            view.renamed.insert(from.clone(), to.clone());
        }
        Command::Columns { names, .. } => view.columns = names.clone(),
        Command::Hide { names, .. } => view.hidden = names.clone(),
        Command::Sort {
            column,
            descending,
            key,
            ..
        } => {
            view.sort = Some(SavedSort {
                column: column.clone(),
                descending: *descending,
                key: key.as_ref().map(|(from, to, method)| SavedSortKey {
                    from: *from,
                    to: *to,
                    method: method.clone(),
                }),
            })
        }
        Command::Filter { regex, .. } => view.filter = Some(regex.clone()),
        Command::MaskValues { column, values, .. } => {
            view.masked = Some(SavedMasks {
                column: column.clone(),
                values: values.clone(),
            })
        }
        Command::MaskRows { rows, .. } => {
            view.masked = Some(SavedMasks {
                column: String::new(),
                values: rows.iter().map(|r| r.to_string()).collect(),
            })
        }
        Command::Transpose { .. } => tab.transposed = true,
        _ => unreachable!("not a shaping command"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<Command> {
        Script::parse(text, &[])
            .unwrap_or_else(|e| panic!("{text}: {e}"))
            .commands
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    /// Everything a session can say, written out and read back.
    #[test]
    fn every_command_survives_being_written_and_read() {
        let text = r#"
# a comment, and a blank line above
open "S18572/meta.xlsx" sheet "Sheet 1" as meta
header meta row 2
open reads.csv as reads; header reads none
extract reads sample_name from "External ID" /^[0-9]-SSH(.*)/
compute reads ratio = "{reads} / {depth} * 100"
rename reads "External ID" to id
columns reads id sample_name ratio
hide meta junk scratch
sort meta sample desc chars 1..4 nat
filter meta /^S1[0-9]/
mask meta batch "A" "B"
mask reads rows 2 7
join meta.sample with reads."sample name" as joined
crop joined rows sample "S1".."S9" cols first "middle one" last as cut
crop joined rows 2..6 as head
transpose cut
export cut to "out.parquet"
"#;
        let commands = parse(text);
        assert_eq!(commands.len(), 18, "{commands:#?}");

        // Written back out, every one of them reads as itself again.
        let written: String = commands
            .iter()
            .map(|c| format!("{c}\n"))
            .collect::<String>();
        assert_eq!(parse(&written), commands, "{written}");
    }

    #[test]
    fn arguments_stand_in_for_files() {
        let commands = parse_with("open $1 as in; export in to $2", &["a b.csv", "out.csv"]);
        assert_eq!(
            commands[0],
            Command::Open {
                path: "a b.csv".to_string(),
                sheet: None,
                handle: "in".to_string()
            }
        );
        // A value goes in whole: a space in a path makes one argument, never
        // two words, and a quote in it cannot break the line open.
        let commands = parse_with("open $1 as in", &[r#"we"ird.csv"#]);
        assert_eq!(
            commands[0],
            Command::Open {
                path: r#"we"ird.csv"#.to_string(),
                sheet: None,
                handle: "in".to_string()
            }
        );

        let missing = Script::parse("open $2 as in", &["only-one".to_string()])
            .expect_err("not given");
        assert!(missing.to_string().contains("$2 was not given"), "{missing}");
    }

    fn parse_with(text: &str, args: &[&str]) -> Vec<Command> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Script::parse(text, &args)
            .unwrap_or_else(|e| panic!("{e}"))
            .commands
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    #[test]
    fn a_shebang_is_for_the_shell_and_a_hash_starts_a_comment() {
        let commands = parse("#!/usr/bin/env lambris\nopen a.csv as x  # trailing\n");
        assert_eq!(commands.len(), 1);
        // …but not inside quotes, where it is just a character.
        let commands = parse(r#"open "od#d.csv" as x"#);
        assert_eq!(
            commands[0],
            Command::Open {
                path: "od#d.csv".to_string(),
                sheet: None,
                handle: "x".to_string()
            }
        );
    }

    #[test]
    fn a_complaint_says_which_line_and_what() {
        for (text, expected) in [
            ("open a.csv as x\nwibble y", "line 2: `wibble` is not a command"),
            ("open a.csv", "line 1: expected `as`"),
            ("sort x col sideways", "line 1: unexpected `sideways`"),
            ("sort x col chars 1..2 backwards", "not a comparison"),
            ("crop x rows a..b as y", "not a row number"),
            (r#"open "unclosed as x"#, "not closed"),
        ] {
            let e = Script::parse(text, &[]).expect_err(text);
            assert!(e.to_string().contains(expected), "{text}: got {e}");
        }
    }
}
