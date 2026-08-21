//! The little expression language behind a computed column.
//!
//! Column references, string and number literals, `+ - * / **`, parentheses and
//! a handful of maths functions. `+` joins text when either side is text and
//! adds when both are numbers, which is what makes `{sample} + ".sat"` and
//! `{a} / {b} * 100` both read the way you would expect.
//!
//! A general-purpose expression crate was the obvious alternative, but they
//! reject `1 + "a"` rather than joining it, and rejecting is the wrong answer
//! for a viewer: a cell that will not parse should leave a gap in the column,
//! not refuse the whole formula. Nor do they say *where* a formula went wrong,
//! which is most of what makes one fixable.

use std::fmt::Write as _;

/// What went wrong, and where — so the formula can be shown back with the spot
/// marked rather than described.
#[derive(Clone, Debug)]
pub struct FormulaError {
    /// Character offset into the formula. `None` when the trouble is the whole
    /// thing rather than one place in it.
    pub at: Option<usize>,
    /// One line, in plain words.
    pub message: String,
    /// What was expected there, when saying so helps.
    pub hint: Option<String>,
}

impl FormulaError {
    fn at(at: usize, message: impl Into<String>) -> Self {
        Self {
            at: Some(at),
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for FormulaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FormulaError {}

/// What a formula may call. All take numbers and give numbers; anything that
/// cannot be worked out — a log of a negative, a division by zero — comes back
/// as a gap rather than an error, the same as any other missing value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Ln,
    Log,
    Log2,
    Exp,
    Sqrt,
    Abs,
    Round,
    Floor,
    Ceil,
    Sin,
    Cos,
    Tan,
    Min,
    Max,
}

impl Func {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "ln" => Func::Ln,
            "log" | "log10" => Func::Log,
            "log2" => Func::Log2,
            "exp" => Func::Exp,
            "sqrt" => Func::Sqrt,
            "abs" => Func::Abs,
            "round" => Func::Round,
            "floor" => Func::Floor,
            "ceil" => Func::Ceil,
            "sin" => Func::Sin,
            "cos" => Func::Cos,
            "tan" => Func::Tan,
            "min" => Func::Min,
            "max" => Func::Max,
            _ => return None,
        })
    }

    /// How many arguments it takes: `(fewest, most)`.
    fn arity(self) -> (usize, usize) {
        match self {
            // `round(x)` to a whole number, `round(x, 2)` to two decimals.
            Func::Round => (1, 2),
            Func::Min | Func::Max => (2, 2),
            _ => (1, 1),
        }
    }

    /// Every name a formula may call, for the help text.
    pub const NAMES: &'static str =
        "ln log log2 exp sqrt abs round floor ceil sin cos tan min max";
}

/// One cell's worth of value while a formula is being worked out.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
    /// A null cell, or a sum that could not be made. Anything touching it comes
    /// out empty too, so a gap in the data stays a gap rather than becoming a
    /// zero or the text "null".
    Empty,
}

impl Value {
    /// The value as text, or `None` when there is nothing to show.
    pub fn text(&self) -> Option<String> {
        match self {
            Value::Number(n) => Some(number_text(*n)),
            Value::Text(t) => Some(t.clone()),
            Value::Empty => None,
        }
    }

    /// A finite number, or a gap.
    fn number(value: f64) -> Value {
        match value.is_finite() {
            true => Value::Number(value),
            false => Value::Empty,
        }
    }
}

/// A number as a person would write it: no trailing `.0`, and no exponent for
/// the sizes a table holds.
fn number_text(value: f64) -> String {
    if !value.is_finite() {
        return String::new();
    }
    let mut out = String::new();
    if value == value.trunc() && value.abs() < 1e15 {
        let _ = write!(out, "{value:.0}");
    } else {
        let _ = write!(out, "{value}");
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

impl Op {
    fn symbol(self) -> &'static str {
        match self {
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Pow => "**",
        }
    }
}

#[derive(Clone, Debug)]
enum Expr {
    Number(f64),
    Text(String),
    /// A column reference, as an index into [`Formula::refs`].
    Ref(usize),
    Neg(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
}

/// A parsed formula, plus the columns it mentions.
///
/// References are resolved to slots at parse time, so evaluating a row is a
/// walk over the tree with no name lookups.
#[derive(Clone, Debug)]
pub struct Formula {
    root: Expr,
    /// The columns referenced, in slot order, each with where it appeared — so
    /// a name the table hasn't got can be pointed at.
    pub refs: Vec<(String, usize)>,
}

impl Formula {
    pub fn parse(text: &str) -> Result<Self, FormulaError> {
        let tokens = lex(text)?;
        let mut parser = Parser {
            tokens: &tokens,
            at: 0,
            refs: Vec::new(),
        };
        let root = parser.expression()?;
        if let Some(extra) = parser.tokens.get(parser.at) {
            return Err(FormulaError::at(
                extra.at,
                format!("{} is left over at the end", extra.token.describe()),
            )
            .with_hint("two values in a row need an operator between them".to_string()));
        }
        Ok(Self {
            root,
            refs: parser.refs,
        })
    }

    /// Work out this formula for one row. `slots` holds the referenced columns'
    /// values, in the order of [`Formula::refs`].
    pub fn eval(&self, slots: &[Value]) -> Value {
        eval(&self.root, slots)
    }
}

fn eval(expr: &Expr, slots: &[Value]) -> Value {
    match expr {
        Expr::Number(n) => Value::Number(*n),
        Expr::Text(t) => Value::Text(t.clone()),
        Expr::Ref(slot) => slots.get(*slot).cloned().unwrap_or(Value::Empty),
        Expr::Neg(inner) => match eval(inner, slots) {
            Value::Number(n) => Value::Number(-n),
            _ => Value::Empty,
        },
        Expr::Call(func, args) => {
            // Every function takes numbers, so anything else is a gap.
            let mut numbers = Vec::with_capacity(args.len());
            for arg in args {
                match eval(arg, slots) {
                    Value::Number(n) => numbers.push(n),
                    _ => return Value::Empty,
                }
            }
            call(*func, &numbers)
        }
        Expr::Binary(op, left, right) => {
            let (left, right) = (eval(left, slots), eval(right, slots));
            // A gap on either side leaves a gap in the answer.
            if left == Value::Empty || right == Value::Empty {
                return Value::Empty;
            }
            match (op, &left, &right) {
                (Op::Add, Value::Number(a), Value::Number(b)) => Value::number(a + b),
                // Anything else added is joined, which is what `+` means when
                // one side is text.
                (Op::Add, _, _) => Value::Text(format!(
                    "{}{}",
                    left.text().unwrap_or_default(),
                    right.text().unwrap_or_default()
                )),
                (_, Value::Number(a), Value::Number(b)) => match op {
                    Op::Sub => Value::number(a - b),
                    Op::Mul => Value::number(a * b),
                    // Dividing by nothing leaves a gap rather than an infinity.
                    Op::Div if *b != 0.0 => Value::number(a / b),
                    Op::Pow => Value::number(a.powf(*b)),
                    _ => Value::Empty,
                },
                // Arithmetic on text that is not a number: nothing to say.
                _ => Value::Empty,
            }
        }
    }
}

fn call(func: Func, args: &[f64]) -> Value {
    let first = args.first().copied().unwrap_or(f64::NAN);
    Value::number(match func {
        Func::Ln => first.ln(),
        Func::Log => first.log10(),
        Func::Log2 => first.log2(),
        Func::Exp => first.exp(),
        Func::Sqrt => first.sqrt(),
        Func::Abs => first.abs(),
        Func::Round => match args.get(1) {
            Some(places) => {
                let scale = 10f64.powf(places.trunc());
                (first * scale).round() / scale
            }
            None => first.round(),
        },
        Func::Floor => first.floor(),
        Func::Ceil => first.ceil(),
        Func::Sin => first.sin(),
        Func::Cos => first.cos(),
        Func::Tan => first.tan(),
        Func::Min => first.min(args[1]),
        Func::Max => first.max(args[1]),
    })
}

#[derive(Debug, PartialEq)]
enum Token {
    Number(f64),
    Text(String),
    Ref(String),
    Name(String),
    Op(Op),
    Open,
    Close,
    Comma,
}

impl Token {
    /// How to name this in a message.
    fn describe(&self) -> String {
        match self {
            Token::Number(n) => format!("the number {}", number_text(*n)),
            Token::Text(_) => "a piece of text".to_string(),
            Token::Ref(name) => format!("the column {{{name}}}"),
            Token::Name(name) => format!("`{name}`"),
            Token::Op(op) => format!("`{}`", op.symbol()),
            Token::Open => "`(`".to_string(),
            Token::Close => "`)`".to_string(),
            Token::Comma => "`,`".to_string(),
        }
    }
}

/// A token and where it started, in characters.
#[derive(Debug)]
struct Spanned {
    token: Token,
    at: usize,
}

/// What a formula may hold, for the message when something else turns up.
const VOCABULARY: &str =
    "a formula holds {columns}, \"text\", numbers, + - * / ** ( ) and functions";

/// Split the formula into tokens. Column names are taken whole between braces,
/// so a name with spaces, accents or punctuation needs no escaping.
fn lex(text: &str) -> Result<Vec<Spanned>, FormulaError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let c = chars[i];
        i += 1;
        let token = match c {
            c if c.is_whitespace() => continue,
            '(' => Token::Open,
            ')' => Token::Close,
            ',' => Token::Comma,
            '+' => Token::Op(Op::Add),
            '-' => Token::Op(Op::Sub),
            '/' => Token::Op(Op::Div),
            // `**` for a power, and `^` too — one is the programmer's habit and
            // the other the spreadsheet's.
            '^' => Token::Op(Op::Pow),
            '*' => {
                if chars.get(i) == Some(&'*') {
                    i += 1;
                    Token::Op(Op::Pow)
                } else {
                    Token::Op(Op::Mul)
                }
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.get(i) {
                        Some('}') => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            name.push(*c);
                            i += 1;
                        }
                        None => {
                            return Err(FormulaError::at(start, "this `{` never closes")
                                .with_hint(
                                    "a column reference looks like {name}".to_string(),
                                ))
                        }
                    }
                }
                if name.trim().is_empty() {
                    return Err(FormulaError::at(start, "this reference has no name")
                        .with_hint("a column reference looks like {name}".to_string()));
                }
                Token::Ref(name.trim().to_string())
            }
            '"' | '\'' => {
                let quote = c;
                let mut literal = String::new();
                loop {
                    match chars.get(i) {
                        Some(c) if *c == quote => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            literal.push(*c);
                            i += 1;
                        }
                        None => {
                            return Err(FormulaError::at(
                                start,
                                format!("this {quote} never closes"),
                            ))
                        }
                    }
                }
                Token::Text(literal)
            }
            c if c.is_ascii_digit() || c == '.' => {
                let mut number = String::from(c);
                while let Some(c) = chars.get(i) {
                    if c.is_ascii_digit() || *c == '.' {
                        number.push(*c);
                        i += 1;
                    } else {
                        break;
                    }
                }
                match number.parse::<f64>() {
                    Ok(parsed) => Token::Number(parsed),
                    Err(_) => {
                        return Err(FormulaError::at(
                            start,
                            format!("`{number}` is not a number"),
                        ))
                    }
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut name = String::from(c);
                while let Some(c) = chars.get(i) {
                    if c.is_alphanumeric() || *c == '_' {
                        name.push(*c);
                        i += 1;
                    } else {
                        break;
                    }
                }
                Token::Name(name)
            }
            other => {
                return Err(FormulaError::at(
                    start,
                    format!("`{other}` is not something a formula can use"),
                )
                .with_hint(VOCABULARY.to_string()))
            }
        };
        tokens.push(Spanned { token, at: start });
    }
    if tokens.is_empty() {
        return Err(FormulaError {
            at: None,
            message: "there is nothing to work out".to_string(),
            hint: Some(VOCABULARY.to_string()),
        });
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Spanned],
    at: usize,
    refs: Vec<(String, usize)>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|s| &s.token)
    }

    /// Where the end of the formula is, for pointing at what is missing.
    fn end(&self) -> usize {
        self.tokens
            .last()
            .map(|s| s.at + 1)
            .unwrap_or(0)
    }

    /// `unary (('+' | '-') unary)*`
    fn expression(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.term()?;
        while let Some(Token::Op(op @ (Op::Add | Op::Sub))) = self.peek() {
            let op = *op;
            self.at += 1;
            let right = self.term()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `unary (('*' | '/') unary)*` — so `*` and `/` bind tighter than `+`.
    fn term(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.unary()?;
        while let Some(Token::Op(op @ (Op::Mul | Op::Div))) = self.peek() {
            let op = *op;
            self.at += 1;
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `'-' unary | power` — so `-2**2` is `-(2**2)`, as it is written on paper.
    fn unary(&mut self) -> Result<Expr, FormulaError> {
        if let Some(Token::Op(Op::Sub)) = self.peek() {
            self.at += 1;
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.power()
    }

    /// `primary ('**' unary)?` — binds tightest, and to the right, so
    /// `2**3**2` is `2**(3**2)`.
    fn power(&mut self) -> Result<Expr, FormulaError> {
        let base = self.primary()?;
        if let Some(Token::Op(Op::Pow)) = self.peek() {
            self.at += 1;
            let exponent = self.unary()?;
            return Ok(Expr::Binary(Op::Pow, Box::new(base), Box::new(exponent)));
        }
        Ok(base)
    }

    fn primary(&mut self) -> Result<Expr, FormulaError> {
        let Some(spanned) = self.tokens.get(self.at) else {
            return Err(FormulaError::at(
                self.end(),
                "the formula stops here, with something still missing",
            )
            .with_hint("an operator needs a value on both sides".to_string()));
        };
        let at = spanned.at;
        self.at += 1;
        Ok(match &spanned.token {
            Token::Number(n) => Expr::Number(*n),
            Token::Text(t) => Expr::Text(t.clone()),
            Token::Ref(name) => {
                // One slot per distinct column, so a name used twice is read
                // once per row.
                let slot = match self.refs.iter().position(|(r, _)| r == name) {
                    Some(slot) => slot,
                    None => {
                        self.refs.push((name.clone(), at));
                        self.refs.len() - 1
                    }
                };
                Expr::Ref(slot)
            }
            Token::Name(name) => return self.call(name, at),
            Token::Open => {
                let inner = self.expression()?;
                match self.peek() {
                    Some(Token::Close) => self.at += 1,
                    _ => return Err(FormulaError::at(at, "this `(` never closes")),
                }
                inner
            }
            Token::Close => {
                return Err(FormulaError::at(at, "this `)` has nothing open before it"))
            }
            Token::Comma => {
                return Err(FormulaError::at(at, "this `,` is not inside a function call"))
            }
            Token::Op(op) => {
                return Err(FormulaError::at(
                    at,
                    format!("`{}` needs a value before it", op.symbol()),
                )
                .with_hint(match op {
                    Op::Pow => "`{col} ** 2` raises to a power".to_string(),
                    _ => "an operator goes between two values".to_string(),
                }))
            }
        })
    }

    /// A function call: `name(a, b)`.
    fn call(&mut self, name: &str, at: usize) -> Result<Expr, FormulaError> {
        let Some(func) = Func::from_name(name) else {
            return Err(FormulaError::at(at, format!("there is no function `{name}`"))
                .with_hint(format!("the ones there are: {}", Func::NAMES)));
        };
        match self.peek() {
            Some(Token::Open) => self.at += 1,
            _ => {
                return Err(
                    FormulaError::at(at, format!("`{name}` needs its arguments in `( )`"))
                        .with_hint(format!("like `{name}({{column}})`")),
                )
            }
        }
        let mut args = Vec::new();
        if self.peek() != Some(&Token::Close) {
            loop {
                args.push(self.expression()?);
                match self.peek() {
                    Some(Token::Comma) => self.at += 1,
                    _ => break,
                }
            }
        }
        match self.peek() {
            Some(Token::Close) => self.at += 1,
            _ => return Err(FormulaError::at(at, format!("`{name}(` never closes"))),
        }
        let (fewest, most) = func.arity();
        if args.len() < fewest || args.len() > most {
            let wanted = match (fewest, most) {
                (a, b) if a == b => format!("{a}"),
                (a, b) => format!("{a} or {b}"),
            };
            return Err(FormulaError::at(
                at,
                format!("`{name}` takes {wanted} arguments, not {}", args.len()),
            ));
        }
        Ok(Expr::Call(func, args))
    }
}
