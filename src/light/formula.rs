//! Effect formulas: the small language custom effects ([`Shape::Custom`](super::effects::Shape))
//! are written in. OpenQ works a formula out into an [`EffectTable`] itself: it is arithmetic
//! only (no loops, no recursion, no files or anything else), so every formula ends, and its cost
//! is known and capped before it runs ([`MAX_WORK`]).
//!
//! ```text
//! # A rainbow running across eight fixtures, with a slider for the saturation.
//! param saturation 1 0..1
//! effect "Rainbow run" steps 64 width 8 smooth
//! red, green, blue = hsv(t + row / width, saturation, 1)
//! ```
//!
//! One statement per line, `#` starts a comment, names are case-insensitive:
//! * `effect "<name>" steps <1..4096> width <1..512> [smooth]` starts an effect (a source can hold
//!   several). For every pattern row and step the lines below it are worked out in order.
//! * `param <name> <default> [<min>..<max>] [int]` declares a slider (range 0..1 when left out);
//!   sliders go before the first effect and every effect of the source can use them.
//! * `let <name> = <expr>` keeps a value for the lines below it (in the same effect).
//! * `<attribute>[, <attribute>…] = <expr>[, <expr>…]` sets attributes (each once per effect);
//!   the effect plays the attributes it sets, in that order. Levels are clamped to 0..1.
//!
//! Expressions are numbers: `+ - * / % ^`, comparisons (1 or 0), `and or not`, parentheses.
//! Variables: `t` (step / steps, 0 ≤ t < 1), `step`, `steps`, `row`, `width`, params and lets.
//! Functions: see [`Func`]; `hsv(h, s, v)` gives three values (red, green, blue) and is only
//! allowed as an item of an assignment. Division by zero gives 0, `%` is floored, anything not
//! finite becomes 0.
//!
//! Formulas are data: they are kept in the show and in effect files, and checked on every read.

use super::effects::{EffectParam, EffectTable, MAX_PARAMS};
use super::model::Attribute;
use std::sync::mpsc::{self, Receiver};

/// Longest source.
pub const MAX_SOURCE: usize = 64 << 10;
/// Most effects in one source.
pub const MAX_EFFECTS: usize = 64;
/// Most expression parts (numbers, names, operators, calls) in one effect.
pub const MAX_NODES: usize = 1024;
/// Deepest nesting of an expression.
pub const MAX_DEPTH: usize = 64;
/// Most work for a source: expression parts × steps × rows, over all its effects (well under a
/// second).
pub const MAX_WORK: u64 = 64 << 20;

/// The example a new formula effect starts from.
pub const EXAMPLE: &str = "# A rainbow running across the fixtures: 8 pattern rows, each a step further round the wheel.
effect \"Rainbow\" steps 64 width 8 smooth
red, green, blue = hsv(t + row / width)
";

/// A mistake in a formula, with where it is (1-based line and column; column 0 = the whole line).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    /// Line.
    pub line: usize,
    /// Column (characters).
    pub col: usize,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.col > 0 {
            write!(f, "line {}, column {}: {}", self.line, self.col, self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for Error {}

/// A function formulas can call. All angles are in turns (1 = a whole cycle), like `t`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Func {
    /// `sine(x)`: 0 → 1 → 0 over one cycle, smooth (the Sine shape).
    Sine,
    /// `triangle(x)`: 0 → 1 → 0, linear.
    Triangle,
    /// `square(x, duty)`: 1 for the first `duty` of the cycle (0.5 when left out), then 0.
    Square,
    /// `saw(x)`: ramp 0 → 1, then snap back.
    Saw,
    /// `ramp_down(x)`: snap to 1, then ramp down to 0.
    RampDown,
    /// `wave(x)`: the sine of `x` turns, −1 … 1 (`wave(x + 0.25)` is the cosine).
    Wave,
    /// `abs(x)`.
    Abs,
    /// `floor(x)`.
    Floor,
    /// `round(x)`.
    Round,
    /// `frac(x)`: `x − floor(x)`.
    Frac,
    /// `min(a, b, …)`.
    Min,
    /// `max(a, b, …)`.
    Max,
    /// `clamp(x, lo, hi)` (`lo`, `hi` 0 and 1 when left out).
    Clamp,
    /// `lerp(a, b, k)`: `a + (b − a)·k`.
    Lerp,
    /// `smoothstep(x)`: eased 0..1.
    Smoothstep,
    /// `if(c, a, b)`: `a` when `c` isn't 0, else `b`.
    If,
    /// `pick(i, a, b, …)`: the value at `floor(i)` (counting from 0, wrapping round).
    Pick,
    /// `noise(n, seed)`: a fixed pseudo-random 0..1 for whole numbers `n` and `seed` (0 when left
    /// out); the same on every computer and every run.
    Noise,
}

impl Func {
    /// Every function: (name, function, fewest and most arguments).
    pub const ALL: [(&'static str, Func, usize, usize); 18] = [
        ("sine", Func::Sine, 1, 1),
        ("triangle", Func::Triangle, 1, 1),
        ("square", Func::Square, 1, 2),
        ("saw", Func::Saw, 1, 1),
        ("ramp_down", Func::RampDown, 1, 1),
        ("wave", Func::Wave, 1, 1),
        ("abs", Func::Abs, 1, 1),
        ("floor", Func::Floor, 1, 1),
        ("round", Func::Round, 1, 1),
        ("frac", Func::Frac, 1, 1),
        ("min", Func::Min, 1, 16),
        ("max", Func::Max, 1, 16),
        ("clamp", Func::Clamp, 1, 3),
        ("lerp", Func::Lerp, 3, 3),
        ("smoothstep", Func::Smoothstep, 1, 1),
        ("if", Func::If, 3, 3),
        ("pick", Func::Pick, 2, 64),
        ("noise", Func::Noise, 1, 2),
    ];

    fn find(name: &str) -> Option<(Func, usize, usize)> {
        Self::ALL.iter().find(|f| f.0 == name).map(|f| (f.1, f.2, f.3))
    }

    fn call(self, a: &[f64]) -> f64 {
        let frac = |x: f64| x - x.floor();
        match self {
            Func::Sine => 0.5 - 0.5 * (std::f64::consts::TAU * a[0]).cos(),
            Func::Triangle => 1.0 - (2.0 * frac(a[0]) - 1.0).abs(),
            Func::Square => (frac(a[0]) < a.get(1).copied().unwrap_or(0.5)) as u8 as f64,
            Func::Saw => frac(a[0]),
            Func::RampDown => 1.0 - frac(a[0]),
            Func::Wave => (std::f64::consts::TAU * a[0]).sin(),
            Func::Abs => a[0].abs(),
            Func::Floor => a[0].floor(),
            Func::Round => a[0].round(),
            Func::Frac => frac(a[0]),
            Func::Min => a.iter().copied().fold(f64::INFINITY, f64::min),
            Func::Max => a.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            Func::Clamp => {
                let (lo, hi) = (a.get(1).copied().unwrap_or(0.0), a.get(2).copied().unwrap_or(1.0));
                if lo <= hi { a[0].clamp(lo, hi) } else { lo }
            }
            Func::Lerp => a[0] + (a[1] - a[0]) * a[2],
            Func::Smoothstep => {
                let x = a[0].clamp(0.0, 1.0);
                x * x * (3.0 - 2.0 * x)
            }
            Func::If => if a[0] != 0.0 { a[1] } else { a[2] },
            Func::Pick => {
                let n = (a.len() - 1) as i64;
                a[1 + whole(a[0]).rem_euclid(n) as usize]
            }
            Func::Noise => noise(whole(a[0]), a.get(1).map_or(0, |s| whole(*s))),
        }
    }
}

/// A number as a whole number (floored; NaN → 0, saturating).
fn whole(x: f64) -> i64 {
    x.floor() as i64
}

/// Integer hash (SplitMix64) of `n` and `seed` → 0..1.
fn noise(n: i64, seed: i64) -> f64 {
    let mut h = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (seed as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ 0x2545_F491_4F6C_DD1D;
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Red, green, blue for hue `h` (turns, wraps), saturation `s` and value `v`.
fn hsv(h: f64, s: f64, v: f64) -> [f64; 3] {
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let h = (h - h.floor()) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, r) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as i64 {
        0 => [v, r, p],
        1 => [q, v, p],
        2 => [p, v, r],
        3 => [p, q, v],
        4 => [r, p, v],
        _ => [v, p, q],
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Num(f64),
    Var(u16),
    Neg,
    Not,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    Call(Func, u8),
}

/// One value (or three, for `hsv`) of an assignment, as postfix code.
#[derive(Clone, Debug)]
struct Item {
    code: Vec<Op>,
    hsv: bool,
}

#[derive(Clone, Debug)]
enum Line {
    Let(u16, Vec<Op>),
    /// Attribute columns and the items giving them.
    Set(Vec<usize>, Vec<Item>),
}

#[derive(Clone, Debug)]
struct Def {
    name: String,
    steps: u32,
    width: u32,
    smooth: bool,
    attributes: Vec<Attribute>,
    lines: Vec<Line>,
    /// Variable slots: built-ins, params, then this effect's lets.
    slots: usize,
    nodes: usize,
    line: usize,
}

/// A checked formula source, ready to work out ([`Program::run`]).
#[derive(Clone, Debug)]
pub struct Program {
    params: Vec<EffectParam>,
    effects: Vec<Def>,
}

/// What a source makes: its effects (name and table) and its sliders.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Output {
    /// Effects, in source order.
    pub effects: Vec<(String, EffectTable)>,
    /// Sliders, with the values used.
    pub params: Vec<EffectParam>,
}

const BUILTINS: [&str; 5] = ["t", "step", "steps", "row", "width"];
const KEYWORDS: [&str; 9] = ["effect", "smooth", "param", "int", "let", "and", "or", "not", "hsv"];

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Num(f64),
    Str(String),
    Word(String),
    Sym(&'static str),
    End,
}

fn err(line: usize, col: usize, message: impl Into<String>) -> Error {
    Error { line, col, message: message.into() }
}

/// Split one line into tokens with their columns (comments dropped).
fn lex(text: &str, line: usize) -> Result<Vec<(Tok, usize)>, Error> {
    const SYMS: [&str; 17] = ["..", "<=", ">=", "==", "!=", "+", "-", "*", "/", "%", "^", "(", ")", ",", "=", "<", ">"];
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let col = i + 1;
        if c.is_whitespace() {
            i += 1;
        } else if c == '#' {
            break;
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            // A fraction only with digits after the point ("0..1" is 0, "..", 1).
            if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let s: String = chars[start..i].iter().collect();
            let n: f64 = s.parse().map_err(|_| err(line, col, format!("bad number {s}")))?;
            out.push((Tok::Num(n), col));
        } else if c == '"' {
            let end = chars[i + 1..].iter().position(|&d| d == '"').ok_or_else(|| err(line, col, "text without its closing \""))?;
            out.push((Tok::Str(chars[i + 1..i + 1 + end].iter().collect()), col));
            i += end + 2;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push((Tok::Word(chars[start..i].iter().collect::<String>().to_ascii_lowercase()), col));
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let sym = SYMS.iter().find(|s| rest.starts_with(**s)).ok_or_else(|| err(line, col, format!("unexpected \"{c}\"")))?;
            out.push((Tok::Sym(sym), col));
            i += sym.len();
        }
    }
    out.push((Tok::End, chars.len() + 1));
    Ok(out)
}

/// An attribute by the name a formula uses: case-insensitive, underscores and spaces ignored,
/// `extra1` … `extra255`.
pub fn attribute(name: &str) -> Option<Attribute> {
    let key: String = name.chars().filter(|c| *c != '_' && *c != ' ').collect::<String>().to_ascii_lowercase();
    if let Some(n) = key.strip_prefix("extra").and_then(|n| n.parse::<u8>().ok()).filter(|n| *n >= 1) {
        return Some(Attribute::Extra(n));
    }
    Attribute::ALL.into_iter().find(|a| a.name().to_ascii_lowercase() == key)
}

/// Names in scope while parsing an effect.
struct Scope<'a> {
    params: &'a [EffectParam],
    lets: &'a [String],
}

impl Scope<'_> {
    fn slot(&self, name: &str) -> Option<u16> {
        if let Some(i) = BUILTINS.iter().position(|b| *b == name) {
            return Some(i as u16);
        }
        if let Some(i) = self.params.iter().position(|p| p.name == name) {
            return Some((BUILTINS.len() + i) as u16);
        }
        self.lets.iter().position(|l| l == name).map(|i| (BUILTINS.len() + self.params.len() + i) as u16)
    }
}

struct Parser<'a> {
    toks: Vec<(Tok, usize)>,
    i: usize,
    line: usize,
    scope: Scope<'a>,
    nodes: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].0
    }

    fn col(&self) -> usize {
        self.toks[self.i].1
    }

    fn next(&mut self) -> (Tok, usize) {
        let t = self.toks[self.i].clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(err(self.line, self.col(), message))
    }

    fn sym(&mut self, s: &str) -> Result<(), Error> {
        if self.is_sym(s) {
            self.next();
            Ok(())
        } else {
            self.fail(format!("\"{s}\" expected"))
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Sym(x) if *x == s)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Word(x) if x == w)
    }

    fn end(&mut self) -> Result<(), Error> {
        match self.peek() {
            Tok::End => Ok(()),
            t => self.fail(format!("unexpected {}", describe(t))),
        }
    }

    fn word(&mut self, what: &str) -> Result<String, Error> {
        match self.next() {
            (Tok::Word(w), _) => Ok(w),
            (t, col) => Err(err(self.line, col, format!("{what} expected, found {}", describe(&t)))),
        }
    }

    /// A number with an optional minus sign (settings, not expressions).
    fn number(&mut self) -> Result<f64, Error> {
        let neg = self.is_sym("-");
        if neg {
            self.next();
        }
        match self.next() {
            (Tok::Num(n), _) => Ok(if neg { -n } else { n }),
            (t, col) => Err(err(self.line, col, format!("number expected, found {}", describe(&t)))),
        }
    }

    fn whole(&mut self, lo: u32, hi: u32, what: &str) -> Result<u32, Error> {
        let col = self.col();
        let n = self.number()?;
        if n.fract() != 0.0 || n < lo as f64 || n > hi as f64 {
            return Err(err(self.line, col, format!("{what} must be a whole number from {lo} to {hi}")));
        }
        Ok(n as u32)
    }

    fn node(&mut self) -> Result<(), Error> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return self.fail(format!("the effect's formulas are too long (more than {MAX_NODES} parts)"));
        }
        Ok(())
    }

    fn deeper(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fail("nested too deep");
        }
        Ok(())
    }

    fn expr(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        self.deeper()?;
        self.and(out)?;
        while self.is_word("or") {
            self.next();
            self.node()?;
            self.and(out)?;
            out.push(Op::Or);
        }
        self.depth -= 1;
        Ok(())
    }

    fn and(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        self.not(out)?;
        while self.is_word("and") {
            self.next();
            self.node()?;
            self.not(out)?;
            out.push(Op::And);
        }
        Ok(())
    }

    fn not(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        if self.is_word("not") {
            self.next();
            self.node()?;
            self.deeper()?;
            self.not(out)?;
            self.depth -= 1;
            out.push(Op::Not);
            return Ok(());
        }
        self.compare(out)
    }

    fn compare(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        self.sum(out)?;
        let op = match self.peek() {
            Tok::Sym("<") => Op::Lt,
            Tok::Sym("<=") => Op::Le,
            Tok::Sym(">") => Op::Gt,
            Tok::Sym(">=") => Op::Ge,
            Tok::Sym("==") => Op::Eq,
            Tok::Sym("!=") => Op::Ne,
            _ => return Ok(()),
        };
        self.next();
        self.node()?;
        self.sum(out)?;
        out.push(op);
        Ok(())
    }

    fn sum(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        self.product(out)?;
        loop {
            let op = match self.peek() {
                Tok::Sym("+") => Op::Add,
                Tok::Sym("-") => Op::Sub,
                _ => return Ok(()),
            };
            self.next();
            self.node()?;
            self.product(out)?;
            out.push(op);
        }
    }

    fn product(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        self.unary(out)?;
        loop {
            let op = match self.peek() {
                Tok::Sym("*") => Op::Mul,
                Tok::Sym("/") => Op::Div,
                Tok::Sym("%") => Op::Rem,
                _ => return Ok(()),
            };
            self.next();
            self.node()?;
            self.unary(out)?;
            out.push(op);
        }
    }

    fn unary(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        if self.is_sym("-") {
            self.next();
            self.node()?;
            self.deeper()?;
            self.unary(out)?;
            self.depth -= 1;
            out.push(Op::Neg);
            return Ok(());
        }
        self.atom(out)?;
        if self.is_sym("^") {
            self.next();
            self.node()?;
            self.deeper()?;
            self.unary(out)?;
            self.depth -= 1;
            out.push(Op::Pow);
        }
        Ok(())
    }

    /// Arguments of a call, after its name.
    fn args(&mut self, out: &mut Vec<Op>) -> Result<usize, Error> {
        self.sym("(")?;
        let mut n = 0;
        if !self.is_sym(")") {
            loop {
                self.expr(out)?;
                n += 1;
                if !self.is_sym(",") {
                    break;
                }
                self.next();
            }
        }
        self.sym(")")?;
        Ok(n)
    }

    fn atom(&mut self, out: &mut Vec<Op>) -> Result<(), Error> {
        self.node()?;
        let col = self.col();
        match self.next() {
            (Tok::Num(n), _) => out.push(Op::Num(n)),
            (Tok::Sym("("), _) => {
                self.expr(out)?;
                self.sym(")")?;
            }
            (Tok::Word(w), _) if self.is_sym("(") => {
                if w == "hsv" {
                    return Err(err(self.line, col, "hsv(…) gives three values: use it on its own in an assignment (red, green, blue = hsv(…))"));
                }
                let Some((f, lo, hi)) = Func::find(&w) else {
                    return Err(err(self.line, col, format!("unknown function \"{w}\"")));
                };
                let n = self.args(out)?;
                if n < lo || n > hi {
                    let count = if lo == hi { lo.to_string() } else { format!("{lo} to {hi}") };
                    return Err(err(self.line, col, format!("{w}() takes {count} values, not {n}")));
                }
                out.push(Op::Call(f, n as u8));
            }
            (Tok::Word(w), _) => match self.scope.slot(&w) {
                Some(s) => out.push(Op::Var(s)),
                None => return Err(err(self.line, col, format!("unknown name \"{w}\""))),
            },
            (t, _) => return Err(err(self.line, col, format!("unexpected {}", describe(&t)))),
        }
        Ok(())
    }

    /// One item of an assignment's right side: an expression, or `hsv(…)` (three values).
    fn item(&mut self) -> Result<Item, Error> {
        let mut code = Vec::new();
        if self.is_word("hsv") && matches!(self.toks.get(self.i + 1), Some((Tok::Sym("("), _))) {
            let col = self.col();
            self.next();
            self.node()?;
            let n = self.args(&mut code)?;
            if !(1..=3).contains(&n) {
                return Err(err(self.line, col, format!("hsv() takes 1 to 3 values, not {n}")));
            }
            for _ in n..3 {
                code.push(Op::Num(1.0));
            }
            if !(self.is_sym(",") || *self.peek() == Tok::End) {
                return self.fail("hsv(…) gives three values: use it on its own in an assignment (red, green, blue = hsv(…))");
            }
            return Ok(Item { code, hsv: true });
        }
        self.expr(&mut code)?;
        Ok(Item { code, hsv: false })
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Num(n) => format!("number {n}"),
        Tok::Str(s) => format!("text \"{s}\""),
        Tok::Word(w) => format!("\"{w}\""),
        Tok::Sym(s) => format!("\"{s}\""),
        Tok::End => "end of line".into(),
    }
}

/// Read and check a source.
pub fn parse(src: &str) -> Result<Program, Error> {
    if src.len() > MAX_SOURCE {
        return Err(err(1, 0, format!("the formula is longer than {} KB", MAX_SOURCE >> 10)));
    }
    let mut params: Vec<EffectParam> = Vec::new();
    let mut effects: Vec<Def> = Vec::new();
    let mut lets: Vec<String> = Vec::new();
    let mut work: u64 = 0;
    for (n, text) in src.lines().enumerate() {
        let line = n + 1;
        let toks = lex(text, line)?;
        if toks.len() == 1 {
            continue;
        }
        let first = match &toks[0].0 {
            Tok::Word(w) => w.clone(),
            t => return Err(err(line, toks[0].1, format!("unexpected {}", describe(t)))),
        };
        let mut p = Parser { toks, i: 0, line, scope: Scope { params: &params, lets: &lets }, nodes: effects.last().map_or(0, |d| d.nodes), depth: 0 };
        // `effect = …` sets the Effect attribute; a header has the name in quotes.
        let header = first == "effect" && matches!(p.toks.get(1), Some((Tok::Str(_), _)));
        match first.as_str() {
            "effect" if header => {
                p.next();
                let name = match p.next() {
                    (Tok::Str(s), _) => s.chars().filter(|c| !c.is_control()).take(200).collect::<String>(),
                    (t, col) => return Err(err(line, col, format!("the effect's name in quotes expected, found {}", describe(&t)))),
                };
                if !p.is_word("steps") {
                    return p.fail("\"steps\" expected");
                }
                p.next();
                let steps = p.whole(1, EffectTable::MAX_STEPS, "steps")?;
                if !p.is_word("width") {
                    return p.fail("\"width\" expected");
                }
                p.next();
                let width = p.whole(1, EffectTable::MAX_WIDTH, "width")?;
                let smooth = p.is_word("smooth");
                if smooth {
                    p.next();
                }
                p.end()?;
                if effects.len() >= MAX_EFFECTS {
                    return Err(err(line, 0, format!("at most {MAX_EFFECTS} effects in one formula")));
                }
                finish(effects.last(), &mut work)?;
                lets.clear();
                effects.push(Def { name, steps, width, smooth, attributes: Vec::new(), lines: Vec::new(), slots: 0, nodes: 0, line });
            }
            "param" => {
                if !effects.is_empty() {
                    return Err(err(line, 1, "sliders (param) go before the first effect"));
                }
                p.next();
                let col = p.col();
                let name = p.word("the slider's name")?;
                if reserved(&name) || params.iter().any(|q| q.name == name) || lets.contains(&name) {
                    return Err(err(line, col, format!("\"{name}\" is already a name")));
                }
                let value = p.number()?;
                let (mut min, mut max) = (0.0, 1.0);
                if matches!(p.peek(), Tok::Num(_) | Tok::Sym("-")) {
                    min = p.number()?;
                    p.sym("..")?;
                    max = p.number()?;
                }
                let integer = p.is_word("int");
                if integer {
                    p.next();
                }
                p.end()?;
                if min >= max {
                    return Err(err(line, col, "the slider's range must go from low to high"));
                }
                if !(min..=max).contains(&value) {
                    return Err(err(line, col, "the starting value must be inside the range"));
                }
                if params.len() >= MAX_PARAMS {
                    return Err(err(line, col, format!("at most {MAX_PARAMS} sliders")));
                }
                params.push(EffectParam { name, min, max, value, integer });
            }
            _ => {
                let Some(def) = effects.last_mut() else {
                    return Err(err(line, 1, "an effect line (effect \"name\" steps … width …) must come first"));
                };
                if first == "let" {
                    p.next();
                    let col = p.col();
                    let name = p.word("a name")?;
                    if reserved(&name) || p.scope.slot(&name).is_some() || attribute(&name).is_some() {
                        return Err(err(line, col, format!("\"{name}\" is already a name")));
                    }
                    p.sym("=")?;
                    let mut code = Vec::new();
                    p.expr(&mut code)?;
                    p.end()?;
                    let slot = (BUILTINS.len() + params.len() + lets.len()) as u16;
                    def.lines.push(Line::Let(slot, code));
                    def.nodes = p.nodes;
                    lets.push(name);
                } else {
                    let mut cols = Vec::new();
                    loop {
                        let col = p.col();
                        let name = p.word("an attribute")?;
                        let a = attribute(&name).ok_or_else(|| err(line, col, format!("unknown attribute \"{name}\"")))?;
                        if def.attributes.contains(&a) {
                            return Err(err(line, col, format!("{} is set twice", a.name())));
                        }
                        if def.attributes.len() >= EffectTable::MAX_ATTRIBUTES {
                            return Err(err(line, col, format!("at most {} attributes in an effect", EffectTable::MAX_ATTRIBUTES)));
                        }
                        def.attributes.push(a);
                        cols.push(def.attributes.len() - 1);
                        if !p.is_sym(",") {
                            break;
                        }
                        p.next();
                    }
                    p.sym("=")?;
                    let mut items = vec![p.item()?];
                    while p.is_sym(",") {
                        p.next();
                        items.push(p.item()?);
                    }
                    p.end()?;
                    let count: usize = items.iter().map(|i| if i.hsv { 3 } else { 1 }).sum();
                    if count != cols.len() {
                        return Err(err(line, 0, format!("{} attributes but {count} values", cols.len())));
                    }
                    def.lines.push(Line::Set(cols, items));
                    def.nodes = p.nodes;
                }
                def.slots = BUILTINS.len() + params.len() + lets.len();
            }
        }
    }
    finish(effects.last(), &mut work)?;
    if effects.is_empty() {
        return Err(err(1, 0, "no effect: start one with  effect \"name\" steps 64 width 1"));
    }
    for d in &mut effects {
        d.slots = d.slots.max(BUILTINS.len() + params.len());
    }
    Ok(Program { params, effects })
}

/// Checks on an effect once all its lines are read.
fn finish(def: Option<&Def>, work: &mut u64) -> Result<(), Error> {
    let Some(d) = def else { return Ok(()) };
    if d.attributes.is_empty() {
        return Err(err(d.line, 0, format!("\"{}\" sets no attribute (e.g. dimmer = sine(t))", d.name)));
    }
    let cells = d.steps as u64 * d.width as u64;
    if d.attributes.len() as u64 * cells > EffectTable::MAX_VALUES as u64 {
        return Err(err(d.line, 0, format!("too many values: attributes × steps × width is more than {}", EffectTable::MAX_VALUES)));
    }
    *work += (d.nodes as u64 + d.attributes.len() as u64) * cells;
    if *work > MAX_WORK {
        return Err(err(d.line, 0, "too much to work out: use fewer steps, rows or shorter formulas"));
    }
    Ok(())
}

fn reserved(name: &str) -> bool {
    BUILTINS.contains(&name) || KEYWORDS.contains(&name) || Func::find(name).is_some()
}

fn eval(code: &[Op], vars: &[f64], stack: &mut Vec<f64>) -> f64 {
    stack.clear();
    for op in code {
        let v = match *op {
            Op::Num(n) => n,
            Op::Var(s) => vars[s as usize],
            Op::Neg => -stack.pop().unwrap_or(0.0),
            Op::Not => (stack.pop().unwrap_or(0.0) == 0.0) as u8 as f64,
            Op::Call(f, n) => {
                let at = stack.len() - n as usize;
                let r = f.call(&stack[at..]);
                stack.truncate(at);
                r
            }
            op => {
                let b = stack.pop().unwrap_or(0.0);
                let a = stack.pop().unwrap_or(0.0);
                match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => if b == 0.0 { 0.0 } else { a / b },
                    Op::Rem => if b == 0.0 { 0.0 } else { a - b * (a / b).floor() },
                    Op::Pow => a.powf(b),
                    Op::Lt => (a < b) as u8 as f64,
                    Op::Le => (a <= b) as u8 as f64,
                    Op::Gt => (a > b) as u8 as f64,
                    Op::Ge => (a >= b) as u8 as f64,
                    Op::Eq => (a == b) as u8 as f64,
                    Op::Ne => (a != b) as u8 as f64,
                    Op::And => (a != 0.0 && b != 0.0) as u8 as f64,
                    Op::Or => (a != 0.0 || b != 0.0) as u8 as f64,
                    _ => unreachable!("handled above"),
                }
            }
        };
        stack.push(if v.is_finite() { v } else { 0.0 });
    }
    stack.pop().unwrap_or(0.0)
}

impl Program {
    /// The sliders the source declares, at their starting values.
    pub fn params(&self) -> &[EffectParam] {
        &self.params
    }

    /// Work the effects out. `values` are slider values by name (others keep their starting
    /// value; values are brought into the slider's range).
    pub fn run(&self, values: &[EffectParam]) -> Output {
        let params: Vec<EffectParam> = self
            .params
            .iter()
            .map(|p| {
                let mut p = p.clone();
                if let Some(v) = values.iter().find(|v| v.name == p.name).map(|v| v.value).filter(|v| v.is_finite()) {
                    p.value = v.clamp(p.min, p.max);
                }
                if p.integer {
                    p.value = p.value.round();
                }
                p
            })
            .collect();
        let mut stack = Vec::with_capacity(MAX_NODES);
        let effects = self
            .effects
            .iter()
            .map(|d| {
                let n = d.attributes.len();
                let mut values = vec![0.0f32; n * d.steps as usize * d.width as usize];
                let mut vars = vec![0.0f64; d.slots];
                for (i, p) in params.iter().enumerate() {
                    vars[BUILTINS.len() + i] = p.value;
                }
                vars[2] = d.steps as f64;
                vars[4] = d.width as f64;
                for row in 0..d.width {
                    vars[3] = row as f64;
                    for step in 0..d.steps {
                        vars[0] = step as f64 / d.steps as f64;
                        vars[1] = step as f64;
                        for line in &d.lines {
                            match line {
                                Line::Let(slot, code) => vars[*slot as usize] = eval(code, &vars, &mut stack),
                                Line::Set(cols, items) => {
                                    let mut c = cols.iter();
                                    for item in items {
                                        let v = eval(&item.code, &vars, &mut stack);
                                        let got = if item.hsv {
                                            let (s, h) = (stack.pop().unwrap_or(0.0), stack.pop().unwrap_or(0.0));
                                            hsv(h, s, v)
                                        } else {
                                            [v, 0.0, 0.0]
                                        };
                                        for v in &got[..if item.hsv { 3 } else { 1 }] {
                                            let col = *c.next().expect("counted when parsed");
                                            let at = ((row as usize * n) + col) * d.steps as usize + step as usize;
                                            values[at] = if v.is_finite() { v.clamp(0.0, 1.0) as f32 } else { 0.0 };
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                (d.name.clone(), EffectTable { attributes: d.attributes.clone(), steps: d.steps, width: d.width, smooth: d.smooth, values })
            })
            .collect();
        Output { effects, params }
    }
}

/// Parse and work out a source with these slider values.
pub fn evaluate(src: &str, values: &[EffectParam]) -> Result<Output, Error> {
    Ok(parse(src)?.run(values))
}

/// A formula being worked out on its own thread (big tables take a moment).
pub struct Run {
    rx: Receiver<Result<Output, Error>>,
}

impl Run {
    /// Start working `src` out.
    pub fn start(src: String, values: Vec<EffectParam>) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("openq-formula".into()).spawn({
            let tx = tx.clone();
            move || {
                let r = crate::recovery::catch(|| Ok(evaluate(&src, &values))).unwrap_or_else(|e| Err(err(1, 0, e.to_string())));
                let _ = tx.send(r);
            }
        });
        if let Err(e) = spawned {
            let _ = tx.send(Err(err(1, 0, e.to_string())));
        }
        Self { rx }
    }

    /// The result, once there is one.
    pub fn poll(&self) -> Option<Result<Output, Error>> {
        self.rx.try_recv().ok()
    }

    /// Wait for the result.
    pub fn wait(&self) -> Result<Output, Error> {
        self.rx.recv().unwrap_or_else(|_| Err(err(1, 0, "the formula stopped")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(src: &str) -> EffectTable {
        let mut o = evaluate(src, &[]).unwrap_or_else(|e| panic!("{e}"));
        o.effects.remove(0).1
    }

    fn error(src: &str) -> String {
        evaluate(src, &[]).expect_err(src).to_string()
    }

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    /// The built-in table effects written as formulas give the same tables.
    #[test]
    fn builtins_as_formulas() {
        let builtins = super::super::effects::builtins();
        let rust = |name: &str| builtins.iter().find(|b| b.effect.name == name).and_then(|b| b.effect.table.clone()).unwrap();
        let cases = [
            ("Odd / even", "effect \"x\" steps 2 width 2\ndimmer = if(step == row, 1, 0)"),
            ("Rainbow", "effect \"x\" steps 36 width 1 smooth\nred, green, blue = hsv(t)"),
            ("Red / blue", "effect \"x\" steps 2 width 1\nred, green, blue = pick(step, 1, 0), 0, pick(step, 0, 1)"),
            ("Warm / cool drift", "effect \"x\" steps 32 width 1 smooth\nlet k = sine(t)\nred, green, blue = 1 - 0.6 * k, 0.55 + 0.15 * k, 0.15 + 0.85 * k"),
            ("Scissors", "effect \"x\" steps 64 width 2 smooth\npan = 0.5 + pick(row, 0.5, -0.5) * wave(t)"),
            ("Figure eight", "effect \"x\" steps 64 width 1 smooth\npan, tilt = 0.5 + 0.5 * wave(t), 0.5 + 0.5 * wave(2 * t)"),
            ("Square path", "effect \"x\" steps 4 width 1 smooth\npan, tilt = pick(step, 0, 1, 1, 0), pick(step, 0, 0, 1, 1)"),
        ];
        for (name, src) in cases {
            let (want, got) = (rust(name), table(src));
            assert_eq!((&got.attributes, got.steps, got.width, got.smooth), (&want.attributes, want.steps, want.width, want.smooth), "{name}");
            assert!(close(&got.values, &want.values), "{name}: {:?} vs {:?}", got.values, want.values);
        }
    }

    #[test]
    fn several_effects_and_params() {
        let src = "param lit 2 1..8 int\nparam hue 0.25\n\
                   effect \"Chase\" steps 8 width 1\ndimmer, extra3 = if(t * 8 < lit, 1, 0), hue\n\
                   effect \"Second\" steps 2 width 1  # comment\nRed = 1 - t";
        let o = evaluate(src, &[EffectParam { name: "lit".into(), value: 3.4, ..Default::default() }]).unwrap();
        assert_eq!(o.effects.len(), 2);
        let (name, t) = &o.effects[0];
        assert_eq!((name.as_str(), t.attributes.as_slice()), ("Chase", [Attribute::Dimmer, Attribute::Extra(3)].as_slice()));
        assert_eq!(&t.values[..8], [1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0], "lit rounded to 3");
        assert!(t.values[8..].iter().all(|v| *v == 0.25));
        assert_eq!(o.effects[1].1.values, [1.0, 0.5]);
        // `effect = …` is the Effect attribute, not a new effect.
        assert_eq!(table("effect \"x\" steps 1 width 1\neffect, effect2 = 0.5, 1").attributes, [Attribute::Effect, Attribute::Effect2]);
        assert_eq!(o.params.iter().map(|p| (p.name.as_str(), p.value, p.integer)).collect::<Vec<_>>(), [("lit", 3.0, true), ("hue", 0.25, false)]);
        let o = evaluate(src, &[EffectParam { name: "lit".into(), value: 99.0, ..Default::default() }]).unwrap();
        assert_eq!(o.params[0].value, 8.0, "clamped to the range");
    }

    #[test]
    fn arithmetic() {
        let v = |e: &str| table(&format!("effect \"x\" steps 1 width 1\nlet v = {e}\ndimmer = v / 32 + 0.5")).values[0] as f64 * 32.0 - 16.0;
        for (e, want) in [
            ("1 + 2 * 3", 7.0),
            ("(1 + 2) * 3 / 9", 1.0),
            ("2 ^ 3 ^ 0", 2.0),
            ("-2 ^ 2", -4.0),
            ("-1.5 % 1", 0.5),
            ("1 / 0 + 3 % 0", 0.0),
            ("1 < 2 and not 0", 1.0),
            ("0 or 0 == 1", 0.0),
            ("min(3, 1, 2) + max(1, 2)", 3.0),
            ("clamp(5) + clamp(-1, -2, 0)", 0.0),
            ("lerp(1, 3, 0.5)", 2.0),
            ("pick(-1, 1, 2, 3)", 3.0),
            ("square(0.3) + square(0.3, 0.2)", 1.0),
            ("round(2.5) - floor(-0.5)", 4.0),
            ("0 ^ -1", 0.0),
        ] {
            assert!((v(e) - want).abs() < 1e-4, "{e} = {} (want {want})", v(e));
        }
    }

    #[test]
    fn noise_is_fixed() {
        let a = table("effect \"x\" steps 8 width 2\ndimmer = noise(step, row)");
        assert_eq!(a.values, table("effect \"x\" steps 8 width 2\ndimmer = noise(step + 0.9, row)").values, "whole numbers");
        assert!(a.values.iter().all(|v| (0.0..1.0).contains(v)));
        assert_ne!(a.values[0], a.values[1]);
        // The same numbers on every computer.
        assert_eq!([noise(0, 0), noise(1, 0), noise(0, 1)].map(|v| (v * 1e9).round()), [582_749_662.0, 660_891_265.0, 322_304_823.0], "{:?}", [noise(0, 0), noise(1, 0), noise(0, 1)]);
    }

    #[test]
    fn mistakes_are_reported_with_their_place() {
        let base = "effect \"x\" steps 4 width 1\n";
        for (src, want) in [
            (format!("{base}dimmer = sinus(t)"), "line 2, column 10: unknown function \"sinus\""),
            (format!("{base}red, green = hsv(t)"), "line 2: 2 attributes but 3 values"),
            (format!("{base}dimmer = t\ndimmer = 1"), "line 3, column 1: Dimmer is set twice"),
            (format!("{base}foo = 1"), "unknown attribute \"foo\""),
            (format!("{base}dimmer = 1 + hsv(t)"), "use it on its own"),
            (format!("{base}dimmer = q"), "unknown name \"q\""),
            (format!("{base}dimmer = lerp(1, 2)"), "lerp() takes 3 values, not 2"),
            (format!("{base}dimmer = ({}t{}", "(".repeat(70), ")".repeat(71)), "nested too deep"),
            (format!("{base}dimmer = {}", "-".repeat(70) + "t"), "nested too deep"),
            (format!("{base}dimmer = {}", vec!["t"; 600].join(" + ")), "too long"),
            (format!("{base}let t = 1\ndimmer = t"), "already a name"),
            (format!("{base}dimmer = 1 2"), "unexpected number 2"),
            ("dimmer = 1".into(), "must come first"),
            ("effect \"x\" steps 0 width 1\ndimmer = 1".into(), "steps must be a whole number from 1 to 4096"),
            ("effect \"x\" steps 4 width 1".into(), "sets no attribute"),
            (format!("effect \"x\" steps 4096 width 256\ndimmer = {}", vec!["t"; 40].join(" + ")), "too much to work out"),
            ("effect \"x\" steps 4096 width 512\ndimmer = 1".into(), "too many values"),
            ("effect \"x\" steps 1 width 1\ndimmer = 1\nparam a 0.5".into(), "go before the first effect"),
            ("param a 2 0..1\neffect \"x\" steps 1 width 1\ndimmer = a".into(), "inside the range"),
            ("param a 0.5 1..0\neffect \"x\" steps 1 width 1\ndimmer = a".into(), "low to high"),
            ("effect \"x steps 1 width 1".into(), "closing"),
            ("".into(), "no effect"),
            (format!("{base}dimmer = 1 ? 2"), "unexpected \"?\""),
        ] {
            let got = error(&src);
            assert!(got.contains(want), "{src:?}: {got}");
        }
    }

    /// Nothing a source holds makes the parser or the evaluator panic or run long.
    #[test]
    fn garbage_is_refused_quickly() {
        let mut rng = fastrand::Rng::with_seed(5);
        let alphabet: Vec<char> = "effect\"steps width smooth param int let and or not hsv sine wave pick noise if dimmer red t row ()+-*/%^,=<>.. 0123456789\n#".chars().collect();
        let start = std::time::Instant::now();
        for _ in 0..3000 {
            let len = rng.usize(..200);
            let src: String = (0..len).map(|_| alphabet[rng.usize(..alphabet.len())]).collect();
            let _ = evaluate(&format!("effect \"x\" steps 64 width 8\n{src}"), &[]);
            let _ = evaluate(&src, &[]);
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn runs_on_its_own_thread() {
        let run = Run::start(EXAMPLE.into(), Vec::new());
        let o = run.wait().unwrap();
        assert_eq!(o.effects[0].0, "Rainbow");
        o.effects[0].1.validate().unwrap();
        assert_eq!(attribute("gobo_rotation"), Some(Attribute::GoboRotation));
        assert_eq!(attribute("EXTRA255"), Some(Attribute::Extra(255)));
        assert_eq!(attribute("extra0"), None);
    }
}
