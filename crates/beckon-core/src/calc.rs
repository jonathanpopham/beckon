//! Fixed-point calculator: arithmetic, unit conversion, base conversion.
//!
//! No floats anywhere. Values are decimal fixed-point on `i128` with
//! [`SCALE`] = 10^12, so "0.1 + 0.2" is exactly "0.3" and the same input
//! produces the same bytes on every platform.
//!
//! Rounding rules (documented and tested):
//!   - Division, multiplication of fractional values, percent, and unit
//!     conversion round the 12th fractional digit half away from zero.
//!     "2/3" displays "0.666666666667".
//!   - Exponentiation is repeated squaring; each intermediate multiply
//!     applies the same rounding.
//!   - Overflow anywhere is a typed error, never a wrap. Intermediates use
//!     a 256-bit multiply-divide so representable results never overflow
//!     spuriously.
//!   - Decimal literals may carry at most 12 fractional digits; more is a
//!     typed error rather than silent truncation.
//!
//! Grammar:
//!   query   := expr (unit)? (("in" | "to") target)?
//!   expr    := add
//!   add     := mul (("+" | "-") mul)*
//!   mul     := unary (("*" | "/" | "%") unary)*
//!   unary   := "-" unary | power
//!   power   := postfix ("^" unary)?          (right associative)
//!   postfix := primary ("%")*                (percent, see below)
//!   primary := number | "(" expr ")"
//!
//! "%" is modulo when a number or "(" follows ("10 % 3" is 1), otherwise
//! a postfix percent ("200 * 15%" is 30; "15%" alone is 0.15).
//!
//! Unit conversion uses offline built-in tables with exact integer ratios:
//! length (mm cm m km in ft yd mi), mass (mg g kg oz lb), time (ms s min
//! h d), data (b kb mb gb tb and kib mib gib tib, both bases; "b" means
//! byte), and temperature (c f k, affine, converted through kelvin).
//! Base conversion handles 0x/0b/0o literals and whole-number targets:
//! "255 in hex", "0xff in dec", "0b1010 in dec".

use std::fmt;

/// Fixed-point scale: 12 decimal digits of fraction.
pub const SCALE: i128 = 1_000_000_000_000;

/// Fractional digits carried by [`SCALE`].
pub const FRACTION_DIGITS: usize = 12;

/// Parenthesis nesting the parser accepts before giving up. Guards the
/// recursive-descent parser against stack exhaustion on garbage input.
const MAX_DEPTH: usize = 64;

/// Largest exponent magnitude accepted by `^`. Anything bigger either
/// overflows anyway or is not a launcher-bar query.
const MAX_EXPONENT: u64 = 100_000;

/// A successful evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalcResult {
    /// The value scaled by [`SCALE`]. For base conversions this is still
    /// the scaled numeric value; only the display changes radix.
    pub value: i128,
    /// Human-facing rendering: trailing zeros trimmed ("0.3", never
    /// "0.300000000000"), unit suffix for conversions, radix prefix for
    /// base conversions.
    pub display: String,
}

/// Typed evaluation failure. Never a panic: garbage in, error out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalcError {
    /// Empty or whitespace-only input.
    Empty,
    /// A byte that cannot appear in an expression.
    UnexpectedChar { pos: usize },
    /// A token in a position where it makes no sense.
    UnexpectedToken { pos: usize },
    /// Input ended mid-expression.
    UnexpectedEof,
    /// A malformed numeric literal.
    InvalidNumber { pos: usize },
    /// A decimal literal with more than [`FRACTION_DIGITS`] fractional digits.
    TooManyFractionDigits { pos: usize },
    /// Division or modulo by zero (including `0 ^ -n`).
    DivideByZero,
    /// A value or intermediate exceeded the representable range.
    Overflow,
    /// `^` with a fractional exponent.
    NonIntegerExponent,
    /// `^` with an exponent magnitude past [`MAX_EXPONENT`].
    ExponentTooLarge,
    /// A unit name not in the built-in tables.
    UnknownUnit(String),
    /// Conversion between different unit categories.
    IncompatibleUnits,
    /// "5 in mi" style query: a unit target needs a source unit.
    MissingSourceUnit,
    /// Base conversion of a non-whole value.
    NonIntegerBaseValue,
    /// Parentheses nested past [`MAX_DEPTH`].
    TooDeep,
    /// Tokens left over after a complete query.
    TrailingInput { pos: usize },
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::Empty => write!(f, "empty expression"),
            CalcError::UnexpectedChar { pos } => write!(f, "unexpected character at offset {pos}"),
            CalcError::UnexpectedToken { pos } => write!(f, "unexpected token at offset {pos}"),
            CalcError::UnexpectedEof => write!(f, "unexpected end of expression"),
            CalcError::InvalidNumber { pos } => write!(f, "invalid number at offset {pos}"),
            CalcError::TooManyFractionDigits { pos } => {
                write!(
                    f,
                    "more than {FRACTION_DIGITS} fractional digits at offset {pos}"
                )
            }
            CalcError::DivideByZero => write!(f, "division by zero"),
            CalcError::Overflow => write!(f, "value out of range"),
            CalcError::NonIntegerExponent => write!(f, "exponent must be a whole number"),
            CalcError::ExponentTooLarge => write!(f, "exponent too large"),
            CalcError::UnknownUnit(name) => write!(f, "unknown unit {name:?}"),
            CalcError::IncompatibleUnits => write!(f, "units measure different things"),
            CalcError::MissingSourceUnit => write!(f, "conversion needs a source unit"),
            CalcError::NonIntegerBaseValue => {
                write!(f, "base conversion needs a whole number")
            }
            CalcError::TooDeep => write!(f, "expression nested too deeply"),
            CalcError::TrailingInput { pos } => write!(f, "unexpected input at offset {pos}"),
        }
    }
}

impl std::error::Error for CalcError {}

/// Evaluate a calculator query: an expression, optionally followed by a
/// unit or base conversion clause.
pub fn eval(input: &str) -> Result<CalcResult, CalcError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(CalcError::Empty);
    }
    let mut parser = TokenParser {
        tokens: &tokens,
        index: 0,
    };
    let value = parser.parse_expr(0)?;
    let trailing = parser.remaining_idents()?;
    match trailing.as_slice() {
        [] => Ok(CalcResult {
            value,
            display: fmt_scaled(value),
        }),
        [(kw_pos, kw), (_, target)] => {
            if !is_conversion_keyword(kw) {
                return Err(CalcError::TrailingInput { pos: *kw_pos });
            }
            if let Some(base) = lookup_base(target) {
                return convert_base(value, base);
            }
            if lookup_unit(target).is_some() {
                return Err(CalcError::MissingSourceUnit);
            }
            Err(CalcError::UnknownUnit(target.clone()))
        }
        [(_, src), (kw_pos, kw), (_, dst)] => {
            if !is_conversion_keyword(kw) {
                return Err(CalcError::TrailingInput { pos: *kw_pos });
            }
            convert_units(value, src, dst)
        }
        [(pos, _), ..] => Err(CalcError::TrailingInput { pos: *pos }),
    }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Num(i128),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<(usize, Tok)>, CalcError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let pos = i;
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'+' => {
                tokens.push((pos, Tok::Plus));
                i += 1;
            }
            b'-' => {
                tokens.push((pos, Tok::Minus));
                i += 1;
            }
            b'*' => {
                tokens.push((pos, Tok::Star));
                i += 1;
            }
            b'/' => {
                tokens.push((pos, Tok::Slash));
                i += 1;
            }
            b'%' => {
                tokens.push((pos, Tok::Percent));
                i += 1;
            }
            b'^' => {
                tokens.push((pos, Tok::Caret));
                i += 1;
            }
            b'(' => {
                tokens.push((pos, Tok::LParen));
                i += 1;
            }
            b')' => {
                tokens.push((pos, Tok::RParen));
                i += 1;
            }
            b'0'..=b'9' | b'.' => {
                let (tok, next) = scan_number(bytes, i)?;
                tokens.push((pos, tok));
                i = next;
            }
            b'a'..=b'z' | b'A'..=b'Z' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = input[start..i].to_ascii_lowercase();
                tokens.push((pos, Tok::Ident(word)));
            }
            _ => return Err(CalcError::UnexpectedChar { pos }),
        }
    }
    Ok(tokens)
}

/// Scan a numeric literal starting at `start`: 0x/0b/0o radix integers or
/// a decimal with up to 12 fractional digits. Returns the token and the
/// index just past it.
fn scan_number(bytes: &[u8], start: usize) -> Result<(Tok, usize), CalcError> {
    let mut i = start;
    // Radix literals: 0x.., 0b.., 0o..
    if bytes[i] == b'0' && i + 1 < bytes.len() {
        let radix = match bytes[i + 1] {
            b'x' | b'X' => Some(16u32),
            b'b' | b'B' => Some(2),
            b'o' | b'O' => Some(8),
            _ => None,
        };
        if let Some(radix) = radix {
            i += 2;
            let digits_start = i;
            let mut n: i128 = 0;
            while i < bytes.len() {
                let digit = match bytes[i] {
                    b'0'..=b'9' => u32::from(bytes[i] - b'0'),
                    b'a'..=b'f' => u32::from(bytes[i] - b'a') + 10,
                    b'A'..=b'F' => u32::from(bytes[i] - b'A') + 10,
                    b'_' => {
                        i += 1;
                        continue;
                    }
                    _ => break,
                };
                if digit >= radix {
                    break;
                }
                n = n
                    .checked_mul(i128::from(radix))
                    .and_then(|m| m.checked_add(i128::from(digit)))
                    .ok_or(CalcError::Overflow)?;
                i += 1;
            }
            if i == digits_start {
                return Err(CalcError::InvalidNumber { pos: start });
            }
            let scaled = n.checked_mul(SCALE).ok_or(CalcError::Overflow)?;
            return Ok((Tok::Num(scaled), i));
        }
    }
    // Decimal literal: digits, optional "." plus 1..=12 digits.
    let int_start = i;
    let mut int_part: i128 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        int_part = int_part
            .checked_mul(10)
            .and_then(|m| m.checked_add(i128::from(bytes[i] - b'0')))
            .ok_or(CalcError::Overflow)?;
        i += 1;
    }
    let has_int = i > int_start;
    let mut frac_part: i128 = 0;
    let mut frac_digits = 0usize;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            if frac_digits == FRACTION_DIGITS {
                return Err(CalcError::TooManyFractionDigits { pos: start });
            }
            frac_part = frac_part * 10 + i128::from(bytes[i] - b'0');
            frac_digits += 1;
            i += 1;
        }
        if i == frac_start {
            // "5." or a bare "." with nothing after it.
            return Err(CalcError::InvalidNumber { pos: start });
        }
    }
    if !has_int && frac_digits == 0 {
        return Err(CalcError::InvalidNumber { pos: start });
    }
    for _ in frac_digits..FRACTION_DIGITS {
        frac_part *= 10;
    }
    let scaled = int_part
        .checked_mul(SCALE)
        .and_then(|m| m.checked_add(frac_part))
        .ok_or(CalcError::Overflow)?;
    Ok((Tok::Num(scaled), i))
}

// ---------------------------------------------------------------------------
// Parser and evaluator
// ---------------------------------------------------------------------------

struct TokenParser<'a> {
    tokens: &'a [(usize, Tok)],
    index: usize,
}

impl<'a> TokenParser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.index).map(|(_, t)| t)
    }

    fn peek_at(&self, offset: usize) -> Option<&'a Tok> {
        self.tokens.get(self.index + offset).map(|(_, t)| t)
    }

    fn pos(&self) -> usize {
        self.tokens
            .get(self.index)
            .map(|(p, _)| *p)
            .unwrap_or(usize::MAX)
    }

    fn advance(&mut self) {
        self.index += 1;
    }

    fn parse_expr(&mut self, depth: usize) -> Result<i128, CalcError> {
        if depth > MAX_DEPTH {
            return Err(CalcError::TooDeep);
        }
        let mut acc = self.parse_mul(depth)?;
        loop {
            match self.peek() {
                Some(Tok::Plus) => {
                    self.advance();
                    let rhs = self.parse_mul(depth)?;
                    acc = acc.checked_add(rhs).ok_or(CalcError::Overflow)?;
                }
                Some(Tok::Minus) => {
                    self.advance();
                    let rhs = self.parse_mul(depth)?;
                    acc = acc.checked_sub(rhs).ok_or(CalcError::Overflow)?;
                }
                _ => return Ok(acc),
            }
        }
    }

    fn parse_mul(&mut self, depth: usize) -> Result<i128, CalcError> {
        let mut acc = self.parse_unary(depth)?;
        loop {
            match self.peek() {
                Some(Tok::Star) => {
                    self.advance();
                    let rhs = self.parse_unary(depth)?;
                    acc = mul_div_round(acc, rhs, SCALE)?;
                }
                Some(Tok::Slash) => {
                    self.advance();
                    let rhs = self.parse_unary(depth)?;
                    if rhs == 0 {
                        return Err(CalcError::DivideByZero);
                    }
                    acc = mul_div_round(acc, SCALE, rhs)?;
                }
                // A "%" reaching this level is modulo: the postfix rule
                // below only leaves it here when an expression follows.
                Some(Tok::Percent) => {
                    self.advance();
                    let rhs = self.parse_unary(depth)?;
                    if rhs == 0 {
                        return Err(CalcError::DivideByZero);
                    }
                    acc = acc.checked_rem(rhs).ok_or(CalcError::Overflow)?;
                }
                _ => return Ok(acc),
            }
        }
    }

    fn parse_unary(&mut self, depth: usize) -> Result<i128, CalcError> {
        if depth > MAX_DEPTH {
            return Err(CalcError::TooDeep);
        }
        if let Some(Tok::Minus) = self.peek() {
            self.advance();
            let value = self.parse_unary(depth + 1)?;
            return value.checked_neg().ok_or(CalcError::Overflow);
        }
        self.parse_power(depth)
    }

    fn parse_power(&mut self, depth: usize) -> Result<i128, CalcError> {
        let base = self.parse_postfix(depth)?;
        if let Some(Tok::Caret) = self.peek() {
            self.advance();
            // Right associative: "2 ^ 3 ^ 2" is 2 ^ (3 ^ 2) = 512.
            let exponent = self.parse_unary(depth + 1)?;
            return pow_fixed(base, exponent);
        }
        Ok(base)
    }

    fn parse_postfix(&mut self, depth: usize) -> Result<i128, CalcError> {
        let mut value = self.parse_primary(depth)?;
        // "%" is postfix percent unless a number or "(" follows, in which
        // case it is left for parse_mul to treat as modulo.
        while let Some(Tok::Percent) = self.peek() {
            let starts_expr = matches!(self.peek_at(1), Some(Tok::Num(_)) | Some(Tok::LParen));
            if starts_expr {
                break;
            }
            self.advance();
            value = mul_div_round(value, 1, 100)?;
        }
        Ok(value)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<i128, CalcError> {
        match self.peek() {
            Some(Tok::Num(n)) => {
                let n = *n;
                self.advance();
                Ok(n)
            }
            Some(Tok::LParen) => {
                self.advance();
                let value = self.parse_expr(depth + 1)?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.advance();
                        Ok(value)
                    }
                    Some(_) => Err(CalcError::UnexpectedToken { pos: self.pos() }),
                    None => Err(CalcError::UnexpectedEof),
                }
            }
            Some(_) => Err(CalcError::UnexpectedToken { pos: self.pos() }),
            None => Err(CalcError::UnexpectedEof),
        }
    }

    /// After the expression, the only legal remainder is a run of idents
    /// forming a conversion clause. Anything else is trailing garbage.
    fn remaining_idents(&mut self) -> Result<Vec<(usize, String)>, CalcError> {
        let mut idents = Vec::new();
        while self.index < self.tokens.len() {
            let (pos, tok) = &self.tokens[self.index];
            match tok {
                Tok::Ident(word) => idents.push((*pos, word.clone())),
                _ => return Err(CalcError::TrailingInput { pos: *pos }),
            }
            self.advance();
        }
        Ok(idents)
    }
}

fn is_conversion_keyword(word: &str) -> bool {
    word == "in" || word == "to"
}

// ---------------------------------------------------------------------------
// Fixed-point arithmetic core
// ---------------------------------------------------------------------------

/// Full 128 x 128 -> 256 bit unsigned multiply, returned as (high, low).
fn mul_u128_wide(a: u128, b: u128) -> (u128, u128) {
    const MASK: u128 = (1 << 64) - 1;
    let (a1, a0) = (a >> 64, a & MASK);
    let (b1, b0) = (b >> 64, b & MASK);
    let ll = a0 * b0;
    let lh = a0 * b1;
    let hl = a1 * b0;
    let hh = a1 * b1;
    let (mid, mid_carry) = lh.overflowing_add(hl);
    let (lo, lo_carry) = ll.overflowing_add((mid & MASK) << 64);
    let hi = hh + (mid >> 64) + (u128::from(mid_carry) << 64) + u128::from(lo_carry);
    (hi, lo)
}

/// Divide the 256-bit value (hi, lo) by `d`, assuming `hi < d` so the
/// quotient fits in 128 bits. Restoring long division, bit by bit.
fn div_wide_by_u128(hi: u128, lo: u128, d: u128) -> u128 {
    debug_assert!(d != 0 && hi < d);
    let mut rem = hi;
    let mut quotient: u128 = 0;
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            quotient |= 1 << i;
        }
    }
    quotient
}

/// Compute a * b / d with the intermediate product held in 256 bits, the
/// quotient rounded half away from zero. The single rounding chokepoint
/// for multiply, divide, percent, power, and unit conversion.
fn mul_div_round(a: i128, b: i128, d: i128) -> Result<i128, CalcError> {
    if d == 0 {
        return Err(CalcError::DivideByZero);
    }
    let negative = (a < 0) ^ (b < 0) ^ (d < 0);
    let dm = d.unsigned_abs();
    let (hi, lo) = mul_u128_wide(a.unsigned_abs(), b.unsigned_abs());
    // Round half away from zero: add d/2 to the magnitude before dividing.
    let (lo, carry) = lo.overflowing_add(dm / 2);
    let hi = hi
        .checked_add(u128::from(carry))
        .ok_or(CalcError::Overflow)?;
    if hi >= dm {
        // Quotient would need more than 128 bits.
        return Err(CalcError::Overflow);
    }
    let q = div_wide_by_u128(hi, lo, dm);
    if negative {
        if q > (1u128 << 127) {
            Err(CalcError::Overflow)
        } else if q == (1u128 << 127) {
            Ok(i128::MIN)
        } else {
            Ok(-(q as i128))
        }
    } else if q > i128::MAX as u128 {
        Err(CalcError::Overflow)
    } else {
        Ok(q as i128)
    }
}

/// Fixed-point power with a whole-number exponent: repeated squaring,
/// each multiply rounded by [`mul_div_round`]. Negative exponents invert
/// at the end. `0 ^ 0` is 1 by convention.
fn pow_fixed(base: i128, exponent_scaled: i128) -> Result<i128, CalcError> {
    if exponent_scaled % SCALE != 0 {
        return Err(CalcError::NonIntegerExponent);
    }
    let exponent = exponent_scaled / SCALE;
    if exponent.unsigned_abs() > u128::from(MAX_EXPONENT) {
        return Err(CalcError::ExponentTooLarge);
    }
    let negative = exponent < 0;
    let mut n = exponent.unsigned_abs();
    let mut acc = SCALE;
    let mut factor = base;
    while n > 0 {
        if n & 1 == 1 {
            acc = mul_div_round(acc, factor, SCALE)?;
        }
        n >>= 1;
        if n > 0 {
            factor = mul_div_round(factor, factor, SCALE)?;
        }
    }
    if negative {
        if acc == 0 {
            return Err(CalcError::DivideByZero);
        }
        acc = mul_div_round(SCALE, SCALE, acc)?;
    }
    Ok(acc)
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/// Render a scaled value: integer part, then up to 12 fractional digits
/// with trailing zeros trimmed. "0.3", never "0.300000000000".
pub fn fmt_scaled(value: i128) -> String {
    let negative = value < 0;
    let magnitude = value.unsigned_abs();
    let int_part = magnitude / (SCALE as u128);
    let frac_part = magnitude % (SCALE as u128);
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&int_part.to_string());
    if frac_part != 0 {
        let mut digits = format!("{frac_part:012}");
        while digits.ends_with('0') {
            digits.pop();
        }
        out.push('.');
        out.push_str(&digits);
    }
    out
}

// ---------------------------------------------------------------------------
// Unit conversion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Length,
    Mass,
    Time,
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempScale {
    Celsius,
    Fahrenheit,
    Kelvin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    /// value_in_base_units = value * num / den, exact integer ratio.
    Linear {
        category: Category,
        num: i128,
        den: i128,
    },
    /// Affine scales converted through kelvin.
    Temp(TempScale),
}

/// Exact ratios to the category base unit (meter, gram, second, byte).
/// International inch: 0.0254 m exactly. Avoirdupois pound: 453.59237 g
/// exactly; the ounce is one sixteenth of that.
const UNITS: &[(&str, Unit)] = &[
    // Length, base meter.
    (
        "mm",
        Unit::Linear {
            category: Category::Length,
            num: 1,
            den: 1000,
        },
    ),
    (
        "cm",
        Unit::Linear {
            category: Category::Length,
            num: 1,
            den: 100,
        },
    ),
    (
        "m",
        Unit::Linear {
            category: Category::Length,
            num: 1,
            den: 1,
        },
    ),
    (
        "km",
        Unit::Linear {
            category: Category::Length,
            num: 1000,
            den: 1,
        },
    ),
    (
        "in",
        Unit::Linear {
            category: Category::Length,
            num: 127,
            den: 5000,
        },
    ),
    (
        "ft",
        Unit::Linear {
            category: Category::Length,
            num: 381,
            den: 1250,
        },
    ),
    (
        "yd",
        Unit::Linear {
            category: Category::Length,
            num: 1143,
            den: 1250,
        },
    ),
    (
        "mi",
        Unit::Linear {
            category: Category::Length,
            num: 201_168,
            den: 125,
        },
    ),
    // Mass, base gram.
    (
        "mg",
        Unit::Linear {
            category: Category::Mass,
            num: 1,
            den: 1000,
        },
    ),
    (
        "g",
        Unit::Linear {
            category: Category::Mass,
            num: 1,
            den: 1,
        },
    ),
    (
        "kg",
        Unit::Linear {
            category: Category::Mass,
            num: 1000,
            den: 1,
        },
    ),
    (
        "oz",
        Unit::Linear {
            category: Category::Mass,
            num: 45_359_237,
            den: 1_600_000,
        },
    ),
    (
        "lb",
        Unit::Linear {
            category: Category::Mass,
            num: 45_359_237,
            den: 100_000,
        },
    ),
    // Time, base second.
    (
        "ms",
        Unit::Linear {
            category: Category::Time,
            num: 1,
            den: 1000,
        },
    ),
    (
        "s",
        Unit::Linear {
            category: Category::Time,
            num: 1,
            den: 1,
        },
    ),
    (
        "min",
        Unit::Linear {
            category: Category::Time,
            num: 60,
            den: 1,
        },
    ),
    (
        "h",
        Unit::Linear {
            category: Category::Time,
            num: 3600,
            den: 1,
        },
    ),
    (
        "d",
        Unit::Linear {
            category: Category::Time,
            num: 86_400,
            den: 1,
        },
    ),
    // Data, base byte. Decimal (SI) and binary (IEC) prefixes both live
    // here; "b" means byte.
    (
        "b",
        Unit::Linear {
            category: Category::Data,
            num: 1,
            den: 1,
        },
    ),
    (
        "kb",
        Unit::Linear {
            category: Category::Data,
            num: 1000,
            den: 1,
        },
    ),
    (
        "mb",
        Unit::Linear {
            category: Category::Data,
            num: 1_000_000,
            den: 1,
        },
    ),
    (
        "gb",
        Unit::Linear {
            category: Category::Data,
            num: 1_000_000_000,
            den: 1,
        },
    ),
    (
        "tb",
        Unit::Linear {
            category: Category::Data,
            num: 1_000_000_000_000,
            den: 1,
        },
    ),
    (
        "kib",
        Unit::Linear {
            category: Category::Data,
            num: 1024,
            den: 1,
        },
    ),
    (
        "mib",
        Unit::Linear {
            category: Category::Data,
            num: 1_048_576,
            den: 1,
        },
    ),
    (
        "gib",
        Unit::Linear {
            category: Category::Data,
            num: 1_073_741_824,
            den: 1,
        },
    ),
    (
        "tib",
        Unit::Linear {
            category: Category::Data,
            num: 1_099_511_627_776,
            den: 1,
        },
    ),
    // Temperature, affine.
    ("c", Unit::Temp(TempScale::Celsius)),
    ("celsius", Unit::Temp(TempScale::Celsius)),
    ("f", Unit::Temp(TempScale::Fahrenheit)),
    ("fahrenheit", Unit::Temp(TempScale::Fahrenheit)),
    ("k", Unit::Temp(TempScale::Kelvin)),
    ("kelvin", Unit::Temp(TempScale::Kelvin)),
];

fn lookup_unit(name: &str) -> Option<Unit> {
    UNITS
        .iter()
        .find(|(unit_name, _)| *unit_name == name)
        .map(|(_, unit)| *unit)
}

/// 273.15 at [`SCALE`].
const KELVIN_OFFSET: i128 = 273_150_000_000_000;
/// 459.67 at [`SCALE`].
const FAHRENHEIT_OFFSET: i128 = 459_670_000_000_000;

fn temp_to_kelvin(value: i128, scale: TempScale) -> Result<i128, CalcError> {
    match scale {
        TempScale::Celsius => value.checked_add(KELVIN_OFFSET).ok_or(CalcError::Overflow),
        TempScale::Fahrenheit => {
            let shifted = value
                .checked_add(FAHRENHEIT_OFFSET)
                .ok_or(CalcError::Overflow)?;
            mul_div_round(shifted, 5, 9)
        }
        TempScale::Kelvin => Ok(value),
    }
}

fn temp_from_kelvin(value: i128, scale: TempScale) -> Result<i128, CalcError> {
    match scale {
        TempScale::Celsius => value.checked_sub(KELVIN_OFFSET).ok_or(CalcError::Overflow),
        TempScale::Fahrenheit => mul_div_round(value, 9, 5)?
            .checked_sub(FAHRENHEIT_OFFSET)
            .ok_or(CalcError::Overflow),
        TempScale::Kelvin => Ok(value),
    }
}

fn convert_units(value: i128, src: &str, dst: &str) -> Result<CalcResult, CalcError> {
    let src_unit = lookup_unit(src).ok_or_else(|| CalcError::UnknownUnit(src.to_string()))?;
    let dst_unit = lookup_unit(dst).ok_or_else(|| CalcError::UnknownUnit(dst.to_string()))?;
    let converted = match (src_unit, dst_unit) {
        (
            Unit::Linear {
                category: sc,
                num: sn,
                den: sd,
            },
            Unit::Linear {
                category: dc,
                num: dn,
                den: dd,
            },
        ) => {
            if sc != dc {
                return Err(CalcError::IncompatibleUnits);
            }
            // Combine both ratios so rounding happens exactly once.
            let num = sn.checked_mul(dd).ok_or(CalcError::Overflow)?;
            let den = sd.checked_mul(dn).ok_or(CalcError::Overflow)?;
            mul_div_round(value, num, den)?
        }
        (Unit::Temp(from), Unit::Temp(to)) => temp_from_kelvin(temp_to_kelvin(value, from)?, to)?,
        _ => return Err(CalcError::IncompatibleUnits),
    };
    Ok(CalcResult {
        value: converted,
        display: format!("{} {}", fmt_scaled(converted), dst),
    })
}

// ---------------------------------------------------------------------------
// Base conversion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Base {
    Hex,
    Dec,
    Bin,
    Oct,
}

fn lookup_base(name: &str) -> Option<Base> {
    match name {
        "hex" | "hexadecimal" => Some(Base::Hex),
        "dec" | "decimal" => Some(Base::Dec),
        "bin" | "binary" => Some(Base::Bin),
        "oct" | "octal" => Some(Base::Oct),
        _ => None,
    }
}

fn convert_base(value: i128, base: Base) -> Result<CalcResult, CalcError> {
    if value % SCALE != 0 {
        return Err(CalcError::NonIntegerBaseValue);
    }
    let n = value / SCALE;
    let sign = if n < 0 { "-" } else { "" };
    let magnitude = n.unsigned_abs();
    let display = match base {
        Base::Hex => format!("{sign}0x{magnitude:x}"),
        Base::Dec => format!("{sign}{magnitude}"),
        Base::Bin => format!("{sign}0b{magnitude:b}"),
        Base::Oct => format!("{sign}0o{magnitude:o}"),
    };
    Ok(CalcResult { value, display })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(input: &str) -> String {
        eval(input)
            .unwrap_or_else(|e| panic!("{input:?} should evaluate, got {e}"))
            .display
    }

    fn error(input: &str) -> CalcError {
        match eval(input) {
            Ok(r) => panic!("{input:?} should fail, got {:?}", r.display),
            Err(e) => e,
        }
    }

    #[test]
    fn golden_exactness() {
        // The headline case: no float drift, ever.
        assert_eq!(display("0.1 + 0.2"), "0.3");
        assert_eq!(display("0.1 + 0.7"), "0.8");
        assert_eq!(display("1 - 0.9"), "0.1");
        assert_eq!(display("0.3 - 0.1"), "0.2");
    }

    #[test]
    fn golden_arithmetic() {
        assert_eq!(display("2 + 3 * 4"), "14");
        assert_eq!(display("(2 + 3) * 4"), "20");
        assert_eq!(display("10 - 4 - 3"), "3");
        assert_eq!(display("100 / 8"), "12.5");
        assert_eq!(display("1.5 * 4"), "6");
        assert_eq!(display("-5 + 2"), "-3");
        assert_eq!(display("--5"), "5");
        assert_eq!(display("2 * (3 + (4 - 1))"), "12");
    }

    #[test]
    fn division_rounds_half_away_from_zero() {
        // Documented rounding: the 12th fractional digit rounds half away
        // from zero.
        assert_eq!(display("1 / 3"), "0.333333333333");
        assert_eq!(display("2 / 3"), "0.666666666667");
        assert_eq!(display("-2 / 3"), "-0.666666666667");
        assert_eq!(display("1 / 8"), "0.125");
        assert_eq!(error("1 / 0"), CalcError::DivideByZero);
    }

    #[test]
    fn golden_modulo() {
        assert_eq!(display("10 % 3"), "1");
        assert_eq!(display("7.5 % 2"), "1.5");
        assert_eq!(display("10 % (2 + 3)"), "0");
        assert_eq!(error("10 % 0"), CalcError::DivideByZero);
    }

    #[test]
    fn golden_percent() {
        assert_eq!(display("15%"), "0.15");
        assert_eq!(display("200 * 15%"), "30");
        assert_eq!(display("50% * 50%"), "0.25");
        assert_eq!(display("100 + 10%"), "100.1");
    }

    #[test]
    fn golden_power() {
        assert_eq!(display("2 ^ 10"), "1024");
        assert_eq!(display("2 ^ 0"), "1");
        assert_eq!(display("0 ^ 0"), "1");
        assert_eq!(display("2 ^ -2"), "0.25");
        assert_eq!(display("1.5 ^ 2"), "2.25");
        assert_eq!(display("2 ^ 3 ^ 2"), "512");
        // Unary minus binds looser than the exponent.
        assert_eq!(display("-3 ^ 2"), "-9");
        assert_eq!(error("2 ^ 0.5"), CalcError::NonIntegerExponent);
        assert_eq!(error("2 ^ 9999999"), CalcError::ExponentTooLarge);
        assert_eq!(error("0 ^ -1"), CalcError::DivideByZero);
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        // Larger than i128::MAX / SCALE, rejected at the literal.
        assert_eq!(error("999999999999999999999999999"), CalcError::Overflow);
        // Representable operands whose product is not.
        assert_eq!(
            error("99999999999999999999999999 * 99999999999999999999999999"),
            CalcError::Overflow
        );
        assert_eq!(
            error("170141183460469231731687303 + 170141183460469231731687303"),
            CalcError::Overflow
        );
        assert_eq!(error("10 ^ 100"), CalcError::Overflow);
    }

    #[test]
    fn wide_intermediates_do_not_overflow_spuriously() {
        // 10^10 * 10^10 = 10^20: the scaled intermediate needs more than
        // 128 bits, the result does not.
        assert_eq!(
            display("10000000000 * 10000000000"),
            "100000000000000000000"
        );
        assert_eq!(
            display("100000000000000000000 / 10000000000"),
            "10000000000"
        );
    }

    #[test]
    fn literal_fraction_digits_are_bounded() {
        assert_eq!(display("0.000000000001"), "0.000000000001");
        assert_eq!(
            error("0.0000000000001"),
            CalcError::TooManyFractionDigits { pos: 0 }
        );
    }

    #[test]
    fn golden_length_conversion() {
        assert_eq!(display("5 km in mi"), "3.106855961187 mi");
        assert_eq!(display("1 mi in km"), "1.609344 km");
        assert_eq!(display("10 in in cm"), "25.4 cm");
        assert_eq!(display("3 ft in m"), "0.9144 m");
        assert_eq!(display("1 yd in ft"), "3 ft");
        assert_eq!(display("2500 mm in m"), "2.5 m");
    }

    #[test]
    fn golden_mass_conversion() {
        assert_eq!(display("1 lb in oz"), "16 oz");
        assert_eq!(display("1 kg in lb"), "2.204622621849 lb");
        assert_eq!(display("500 g in kg"), "0.5 kg");
        assert_eq!(display("1 lb in g"), "453.59237 g");
    }

    #[test]
    fn golden_time_conversion() {
        assert_eq!(display("90 min in h"), "1.5 h");
        assert_eq!(display("1 d in min"), "1440 min");
        assert_eq!(display("1500 ms in s"), "1.5 s");
    }

    #[test]
    fn golden_data_conversion() {
        assert_eq!(display("1 kib in b"), "1024 b");
        assert_eq!(display("1 KiB in B"), "1024 b");
        assert_eq!(display("1 gib in mib"), "1024 mib");
        assert_eq!(display("1 kb in kib"), "0.9765625 kib");
        assert_eq!(display("1 tb in gb"), "1000 gb");
    }

    #[test]
    fn golden_temperature_conversion() {
        assert_eq!(display("72 f in c"), "22.222222222222 c");
        assert_eq!(display("100 c in f"), "212 f");
        assert_eq!(display("0 c in k"), "273.15 k");
        assert_eq!(display("300 k in c"), "26.85 c");
        assert_eq!(display("32 fahrenheit in celsius"), "0 celsius");
    }

    #[test]
    fn conversion_accepts_expressions_and_to_keyword() {
        assert_eq!(display("2 * 45 min in h"), "1.5 h");
        assert_eq!(display("5 km to mi"), "3.106855961187 mi");
    }

    #[test]
    fn conversion_errors_are_typed() {
        assert_eq!(
            error("5 floops in mi"),
            CalcError::UnknownUnit("floops".to_string())
        );
        assert_eq!(
            error("5 km in floops"),
            CalcError::UnknownUnit("floops".to_string())
        );
        assert_eq!(error("5 km in kg"), CalcError::IncompatibleUnits);
        assert_eq!(error("5 km in c"), CalcError::IncompatibleUnits);
        assert_eq!(error("5 in mi"), CalcError::MissingSourceUnit);
    }

    #[test]
    fn golden_base_conversion() {
        assert_eq!(display("255 in hex"), "0xff");
        assert_eq!(display("0xff in dec"), "255");
        assert_eq!(display("0b1010 in dec"), "10");
        assert_eq!(display("0o777 in dec"), "511");
        assert_eq!(display("255 in bin"), "0b11111111");
        assert_eq!(display("255 in oct"), "0o377");
        assert_eq!(display("255 in decimal"), "255");
        assert_eq!(display("-255 in hex"), "-0xff");
        // Radix literals join ordinary arithmetic.
        assert_eq!(display("0xff + 1"), "256");
        assert_eq!(display("0xff + 1 in hex"), "0x100");
        assert_eq!(error("1.5 in hex"), CalcError::NonIntegerBaseValue);
    }

    #[test]
    fn inch_versus_keyword_disambiguation() {
        // "in" the unit and "in" the keyword coexist positionally.
        assert_eq!(display("5 in in cm"), "12.7 cm");
        assert_eq!(display("5 in to cm"), "12.7 cm");
    }

    #[test]
    fn malformed_input_is_a_typed_error() {
        assert_eq!(error(""), CalcError::Empty);
        assert_eq!(error("   "), CalcError::Empty);
        assert_eq!(error("2 +"), CalcError::UnexpectedEof);
        assert_eq!(error("(2 + 3"), CalcError::UnexpectedEof);
        assert!(matches!(
            error("2 + * 3"),
            CalcError::UnexpectedToken { .. }
        ));
        assert!(matches!(error("5."), CalcError::InvalidNumber { .. }));
        assert!(matches!(error("."), CalcError::InvalidNumber { .. }));
        assert!(matches!(error("0x"), CalcError::InvalidNumber { .. }));
        assert!(matches!(error("2 @ 3"), CalcError::UnexpectedChar { .. }));
        assert!(matches!(error("2 3"), CalcError::TrailingInput { .. }));
        assert!(matches!(error("5 km mi"), CalcError::TrailingInput { .. }));
        assert!(matches!(
            error("5 km in mi extra"),
            CalcError::TrailingInput { .. }
        ));
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let mut deep = "(".repeat(MAX_DEPTH + 2);
        deep.push('1');
        deep.push_str(&")".repeat(MAX_DEPTH + 2));
        assert_eq!(error(&deep), CalcError::TooDeep);
    }

    /// Garbage never panics: hand-picked nasties plus generated noise.
    #[test]
    fn fuzzish_never_panics() {
        let nasties = [
            "%%%%",
            "^^^",
            "((((((((((",
            "))))))))))",
            "----------1",
            "0x0x0x",
            "0b2",
            "0o9",
            "in in in in",
            "km",
            "1 in",
            "1 to",
            "9999999999999999999999999999999999999999999999",
            "1/0%",
            "0^-0",
            "-0.000000000001",
            "1..2",
            ".5",
            "5 . 5",
            "\u{0}",
            "日本語",
            "1 + ü",
            "%1",
            "1%%%%%%%%",
            "(-)",
            "() + 1",
        ];
        for input in nasties {
            // Ok or Err both fine; the assertion is "no panic".
            let _ = eval(input);
        }
        // Deterministic pseudo-random byte soup over the token alphabet.
        let alphabet: &[u8] = b"0123456789.+-*/%^() abcdefkmghinoxz";
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..500 {
            let mut input = String::new();
            let len = (state % 24) as usize;
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                input.push(char::from(alphabet[(state as usize) % alphabet.len()]));
            }
            let _ = eval(&input);
        }
    }

    #[test]
    fn display_trims_trailing_zeros() {
        assert_eq!(fmt_scaled(0), "0");
        assert_eq!(fmt_scaled(SCALE), "1");
        assert_eq!(fmt_scaled(SCALE / 2), "0.5");
        assert_eq!(fmt_scaled(3 * SCALE / 10), "0.3");
        assert_eq!(fmt_scaled(-SCALE / 4), "-0.25");
        assert_eq!(fmt_scaled(1), "0.000000000001");
        assert_eq!(
            fmt_scaled(i128::MIN),
            "-170141183460469231731687303.715884105728"
        );
    }

    #[test]
    fn result_value_is_scaled() {
        assert_eq!(eval("1 + 1").expect("eval").value, 2 * SCALE);
        assert_eq!(eval("0.5").expect("eval").value, SCALE / 2);
        assert_eq!(eval("255 in hex").expect("eval").value, 255 * SCALE);
    }

    #[test]
    fn whitespace_and_case_are_forgiving() {
        assert_eq!(display("  2+2 "), "4");
        assert_eq!(display("5KM IN MI"), "3.106855961187 mi");
        assert_eq!(display("0XFF in DEC"), "255");
    }
}
