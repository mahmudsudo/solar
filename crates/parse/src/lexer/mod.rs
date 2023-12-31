//! Solidity lexer.

use sulk_ast::{
    ast::Base,
    token::{BinOpToken, CommentKind, Delimiter, Lit, LitKind, Token, TokenKind},
};
use sulk_interface::{diagnostics::DiagCtxt, sym, BytePos, Pos, Span, Symbol};

mod cursor;
pub use cursor::{is_id_continue, is_id_start, is_ident, is_whitespace};
use cursor::{Cursor, RawLiteralKind, RawToken, RawTokenKind};

pub mod unescape;

mod unicode_chars;

mod utf8;

/// Solidity lexer.
pub struct Lexer<'a> {
    /// The diagnostic context.
    dcx: &'a DiagCtxt,

    /// Initial position, read-only.
    start_pos: BytePos,

    /// The absolute offset within the source_map of the current character.
    pos: BytePos,

    /// Source text to tokenize.
    src: &'a str,

    /// Cursor for getting lexer tokens.
    cursor: Cursor<'a>,

    /// The current token which has not been processed by `next_token` yet.
    token: Token,

    override_span: Option<Span>,

    /// When a "unknown start of token: \u{a0}" has already been emitted earlier
    /// in this file, it's safe to treat further occurrences of the non-breaking
    /// space character as whitespace.
    nbsp_is_whitespace: bool,
}

impl<'a> Lexer<'a> {
    /// Creates a new `Lexer` for the given source string.
    pub fn new(
        dcx: &'a DiagCtxt,
        src: &'a str,
        start_pos: BytePos,
        override_span: Option<Span>,
    ) -> Self {
        let mut lexer = Self {
            dcx,
            start_pos,
            pos: start_pos,
            src,
            cursor: Cursor::new(src),
            token: Token::DUMMY,
            override_span,
            nbsp_is_whitespace: false,
        };
        (lexer.token, _) = lexer.bump();
        lexer
    }

    /// Returns a reference to the diagnostic context.
    #[inline]
    pub fn dcx(&self) -> &'a DiagCtxt {
        self.dcx
    }

    /// Consumes the lexer and collects the remaining tokens into a vector.
    pub fn into_tokens(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token();
            if token.is_eof() {
                break;
            }
            tokens.push(token);
        }
        tokens
    }

    /// Returns the next token, advancing the lexer.
    pub fn next_token(&mut self) -> Token {
        let mut next_token;
        loop {
            let preceded_by_whitespace;
            (next_token, preceded_by_whitespace) = self.bump();
            if preceded_by_whitespace {
                break;
            } else if let Some(glued) = self.token.glue(&next_token) {
                self.token = glued;
            } else {
                break;
            }
        }
        std::mem::replace(&mut self.token, next_token)
    }

    fn bump(&mut self) -> (Token, bool) {
        let mut preceded_by_whitespace = false;
        let mut swallow_next_invalid = 0;
        loop {
            let RawToken { kind: raw_kind, len } = self.cursor.advance_token();
            let start = self.pos;
            self.pos += len;

            // Now "cook" the token, converting the simple `RawTokenKind` into a rich `TokenKind`.
            // This turns strings into interned symbols and runs additional validation.
            let kind = match raw_kind {
                RawTokenKind::LineComment { is_doc } => {
                    // Skip non-doc comments.
                    if !is_doc {
                        preceded_by_whitespace = true;
                        continue;
                    }

                    // Opening delimiter of the length 3 is not included into the symbol.
                    let content_start = start + BytePos(3);
                    let content = self.str_from(content_start);
                    self.cook_doc_comment(content_start, content, CommentKind::Line)
                }
                RawTokenKind::BlockComment { is_doc, terminated } => {
                    if !terminated {
                        self.report_unterminated_block_comment(start, is_doc);
                    }

                    // Skip non-doc comments.
                    if !is_doc {
                        preceded_by_whitespace = true;
                        continue;
                    }

                    // Opening delimiter of the length 3 and closing delimiter of the length 2
                    // are not included into the symbol.
                    let content_start = start + BytePos(3);
                    let content_end = self.pos - (terminated as u32) * 2;
                    let content = self.str_from_to(content_start, content_end);
                    self.cook_doc_comment(content_start, content, CommentKind::Block)
                }
                RawTokenKind::Whitespace => {
                    preceded_by_whitespace = true;
                    continue;
                }
                RawTokenKind::Ident => {
                    let sym = self.symbol_from(start);
                    TokenKind::Ident(sym)
                }
                RawTokenKind::UnknownPrefix => {
                    self.report_unknown_prefix(start);
                    let sym = self.symbol_from(start);
                    TokenKind::Ident(sym)
                }
                RawTokenKind::Literal { kind } => {
                    let (kind, symbol) = self.cook_literal(start, self.pos, kind);
                    TokenKind::Literal(Lit { kind, symbol })
                }

                RawTokenKind::Semi => TokenKind::Semi,
                RawTokenKind::Comma => TokenKind::Comma,
                RawTokenKind::Dot => TokenKind::Dot,
                RawTokenKind::OpenParen => TokenKind::OpenDelim(Delimiter::Parenthesis),
                RawTokenKind::CloseParen => TokenKind::CloseDelim(Delimiter::Parenthesis),
                RawTokenKind::OpenBrace => TokenKind::OpenDelim(Delimiter::Brace),
                RawTokenKind::CloseBrace => TokenKind::CloseDelim(Delimiter::Brace),
                RawTokenKind::OpenBracket => TokenKind::OpenDelim(Delimiter::Bracket),
                RawTokenKind::CloseBracket => TokenKind::CloseDelim(Delimiter::Bracket),
                RawTokenKind::Tilde => TokenKind::Tilde,
                RawTokenKind::Question => TokenKind::Question,
                RawTokenKind::Colon => TokenKind::Colon,
                RawTokenKind::Eq => TokenKind::Eq,
                RawTokenKind::Bang => TokenKind::Not,
                RawTokenKind::Lt => TokenKind::Lt,
                RawTokenKind::Gt => TokenKind::Gt,
                RawTokenKind::Minus => TokenKind::BinOp(BinOpToken::Minus),
                RawTokenKind::And => TokenKind::BinOp(BinOpToken::And),
                RawTokenKind::Or => TokenKind::BinOp(BinOpToken::Or),
                RawTokenKind::Plus => TokenKind::BinOp(BinOpToken::Plus),
                RawTokenKind::Star => TokenKind::BinOp(BinOpToken::Star),
                RawTokenKind::Slash => TokenKind::BinOp(BinOpToken::Slash),
                RawTokenKind::Caret => TokenKind::BinOp(BinOpToken::Caret),
                RawTokenKind::Percent => TokenKind::BinOp(BinOpToken::Percent),

                RawTokenKind::Unknown => {
                    // Don't emit diagnostics for sequences of the same invalid token
                    if swallow_next_invalid > 0 {
                        swallow_next_invalid -= 1;
                        continue;
                    }
                    let mut it = self.str_from_to_end(start).chars();
                    let c = it.next().unwrap();
                    if c == '\u{00a0}' {
                        // If an error has already been reported on non-breaking
                        // space characters earlier in the file, treat all
                        // subsequent occurrences as whitespace.
                        if self.nbsp_is_whitespace {
                            preceded_by_whitespace = true;
                            continue;
                        }
                        self.nbsp_is_whitespace = true;
                    }

                    let repeats = it.take_while(|c1| *c1 == c).count();
                    swallow_next_invalid = repeats;

                    let span = self
                        .new_span(start, self.pos + BytePos::from_usize(repeats * c.len_utf8()));
                    let escaped = escaped_char(c);
                    let message = format!("unknown start of token: {escaped}");
                    let mut diag = self.dcx().err(message).span(span);
                    if c == '\0' {
                        let help = "source files must contain UTF-8 encoded text, unexpected null bytes might occur when a different encoding is used";
                        diag = diag.help(help);
                    }
                    if repeats > 0 {
                        let note = match repeats {
                            1 => "once more".to_string(),
                            _ => format!("{repeats} more times"),
                        };
                        diag = diag.note(format!("character repeats {note}"));
                    }
                    diag.emit();

                    preceded_by_whitespace = true;
                    continue;
                    // TODO
                    /*
                    let (token, _sugg) =
                        unicode_chars::check_for_substitution(self, start, c, repeats + 1);

                    self.sess.emit_err(errors::UnknownTokenStart {
                        span,
                        escaped: escaped_char(c),
                        sugg,
                        null: if c == '\x00' { Some(errors::UnknownTokenNull) } else { None },
                        repeat: if repeats > 0 {
                            Some(errors::UnknownTokenRepeat { repeats })
                        } else {
                            None
                        },
                    });
                    if let Some(token) = token {
                        token
                    } else {
                        preceded_by_whitespace = true;
                        continue;
                    }
                    */
                }

                RawTokenKind::Eof => TokenKind::Eof,
            };
            let span = self.new_span(start, self.pos);
            return (Token::new(kind, span), preceded_by_whitespace);
        }
    }

    fn cook_doc_comment(
        &self,
        content_start: BytePos,
        content: &str,
        comment_kind: CommentKind,
    ) -> TokenKind {
        if content.contains('\r') {
            for (idx, _) in content.char_indices().filter(|&(_, c)| c == '\r') {
                let span = self.new_span(
                    content_start + BytePos(idx as u32),
                    content_start + BytePos(idx as u32 + 1),
                );
                let block = if matches!(comment_kind, CommentKind::Block) { "block " } else { "" };
                let msg = format!("bare CR not allowed in {block}doc-comment");
                self.dcx().err(msg).span(span).emit();
            }
        }

        TokenKind::DocComment(comment_kind, Symbol::intern(content))
    }

    fn cook_literal(
        &self,
        start: BytePos,
        end: BytePos,
        kind: RawLiteralKind,
    ) -> (LitKind, Symbol) {
        match kind {
            RawLiteralKind::Str { terminated, unicode } => {
                if !terminated {
                    let span = self.new_span(start, end);
                    self.dcx().fatal("unterminated string").span(span).emit();
                }
                let kind = if unicode { LitKind::UnicodeStr } else { LitKind::Str };
                let prefix_len = if unicode { 7 } else { 0 }; // `unicode`
                self.cook_quoted(kind, start, end, prefix_len)
            }
            RawLiteralKind::HexStr { terminated } => {
                if !terminated {
                    let span = self.new_span(start, end);
                    self.dcx().fatal("unterminated hex string").span(span).emit();
                }
                let prefix_len = 3; // `hex`
                self.cook_quoted(LitKind::HexStr, start, end, prefix_len)
            }
            RawLiteralKind::Int { base, empty_int } => {
                if empty_int {
                    let span = self.new_span(start, end);
                    self.dcx().err("no valid digits found for number").span(span).emit();
                    (LitKind::Integer, sym::integer(0))
                } else {
                    if matches!(base, Base::Binary | Base::Octal) {
                        let start = start + 2;
                        // TODO: enable if binary and octal literals are ever supported.
                        /*
                        let base = base as u32;
                        let s = self.str_from_to(start, end);
                        for (i, c) in s.char_indices() {
                            if c != '_' && c.to_digit(base).is_none() {
                                let msg = format!("invalid digit for a base {base} literal");
                                let lo = start + BytePos::from_usize(i);
                                let hi = lo + BytePos::from_usize(c.len_utf8());
                                let span = self.new_span(lo, hi);
                                self.dcx().err(msg).span(span).emit();
                            }
                        }
                        */
                        let msg = format!("integers in base {base} are not supported");
                        self.dcx().err(msg).span(self.new_span(start, end)).emit();
                    }
                    (LitKind::Integer, self.symbol_from_to(start, end))
                }
            }
            RawLiteralKind::Rational { base, empty_exponent } => {
                if empty_exponent {
                    let span = self.new_span(start, self.pos);
                    self.dcx().err("expected at least one digit in exponent").span(span).emit();
                }

                let unsupported_base =
                    matches!(base, Base::Binary | Base::Octal | Base::Hexadecimal);
                if unsupported_base {
                    let msg = format!("{base} rational numbers are not supported");
                    self.dcx().err(msg).span(self.new_span(start, end)).emit();
                }

                (LitKind::Rational, self.symbol_from_to(start, end))
            }
        }
    }

    fn cook_quoted(
        &self,
        kind: LitKind,
        start: BytePos,
        end: BytePos,
        prefix_len: u32,
    ) -> (LitKind, Symbol) {
        let mode = match kind {
            LitKind::Str => unescape::Mode::Str,
            LitKind::UnicodeStr => unescape::Mode::UnicodeStr,
            LitKind::HexStr => unescape::Mode::HexStr,
            _ => unreachable!(),
        };

        // Account for quote (`"` or `'`) and prefix.
        let content_start = start + 1 + BytePos(prefix_len);
        let content_end = end - 1;
        let lit_content = self.str_from_to(content_start, content_end);

        let mut has_fatal_err = false;
        unescape::unescape_literal(lit_content, mode, |range, result| {
            // Here we only check for errors. The actual unescaping is done later.
            if let Err(err) = result {
                has_fatal_err = true;
                let (start, end) = (range.start as u32, range.end as u32);
                let lo = content_start + BytePos(start);
                let hi = lo + BytePos(end - start);
                let span = self.new_span(lo, hi);
                unescape::emit_unescape_error(self.dcx(), lit_content, span, range, err);
            }
        });

        // We normally exclude the quotes for the symbol, but for errors we
        // include it because it results in clearer error messages.
        if has_fatal_err {
            (LitKind::Err, self.symbol_from_to(start, end))
        } else {
            (kind, Symbol::intern(lit_content))
        }
    }

    fn new_span(&self, lo: BytePos, hi: BytePos) -> Span {
        self.override_span.unwrap_or_else(|| Span::new(lo, hi))
    }

    #[inline]
    fn src_index(&self, pos: BytePos) -> usize {
        (pos - self.start_pos).to_usize()
    }

    /// Slice of the source text from `start` up to but excluding `self.pos`,
    /// meaning the slice does not include the character `self.ch`.
    fn symbol_from(&self, start: BytePos) -> Symbol {
        self.symbol_from_to(start, self.pos)
    }

    /// Slice of the source text from `start` up to but excluding `self.pos`,
    /// meaning the slice does not include the character `self.ch`.
    fn str_from(&self, start: BytePos) -> &'a str {
        self.str_from_to(start, self.pos)
    }

    /// Same as `symbol_from`, with an explicit endpoint.
    fn symbol_from_to(&self, start: BytePos, end: BytePos) -> Symbol {
        // debug!("taking an ident from {:?} to {:?}", start, end);
        Symbol::intern(self.str_from_to(start, end))
    }

    /// Slice of the source text spanning from `start` up to but excluding `end`.
    fn str_from_to(&self, start: BytePos, end: BytePos) -> &'a str {
        &self.src[self.src_index(start)..self.src_index(end)]
    }

    /// Slice of the source text spanning from `start` until the end.
    fn str_from_to_end(&self, start: BytePos) -> &'a str {
        &self.src[self.src_index(start)..]
    }

    fn report_unterminated_block_comment(&self, start: BytePos, is_doc: bool) {
        let msg =
            if is_doc { "unterminated block doc-comment" } else { "unterminated block comment" };
        self.dcx().fatal(msg).span(self.new_span(start, self.pos)).emit();
    }

    fn report_unknown_prefix(&self, start: BytePos) {
        let prefix = self.str_from_to(start, self.pos);
        let msg = format!("prefix {prefix} is unknown");
        self.dcx().err(msg).span(self.new_span(start, self.pos)).emit();
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    #[inline]
    fn next(&mut self) -> Option<Token> {
        let token = self.next_token();
        if token.is_eof() {
            None
        } else {
            Some(token)
        }
    }
}

impl std::iter::FusedIterator for Lexer<'_> {}

/// Pushes a character to a message string for error reporting
fn escaped_char(c: char) -> String {
    match c {
        '\u{20}'..='\u{7e}' => {
            // Don't escape \, ' or " for user-facing messages
            c.to_string()
        }
        _ => c.escape_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;
    use BinOpToken::*;
    use TokenKind::*;

    type Expected<'a> = &'a [(Range<usize>, TokenKind)];

    fn check(src: &str, expected: Expected<'_>) {
        let dcx = DiagCtxt::with_test_emitter(false);
        let tokens: Vec<_> = Lexer::new(&dcx, src, BytePos(0), None)
            .map(|t| (t.span.lo().to_usize()..t.span.hi().to_usize(), t.kind))
            .collect();
        assert_eq!(tokens, expected, "{src:?}");
    }

    fn checks(tests: &[(&str, Expected<'_>)]) {
        for &(src, expected) in tests {
            check(src, expected);
        }
    }

    fn lit(kind: LitKind, symbol: &str) -> TokenKind {
        Literal(Lit { kind, symbol: sym(symbol) })
    }

    fn id(symbol: &str) -> TokenKind {
        Ident(sym(symbol))
    }

    fn sym(s: &str) -> Symbol {
        Symbol::intern(s)
    }

    #[test]
    fn empty() {
        checks(&[
            ("", &[]),
            (" ", &[]),
            (" \n", &[]),
            ("\n", &[]),
            ("\n\t", &[]),
            ("\n \t", &[]),
            ("\n \t ", &[]),
            (" \n \t \t", &[]),
        ]);
    }

    #[test]
    fn literals() {
        use LitKind::*;
        sulk_interface::create_session_globals_then(|| {
            checks(&[
                ("\"\"", &[(0..2, lit(Str, ""))]),
                ("\"\"\"\"", &[(0..2, lit(Str, "")), (2..4, lit(Str, ""))]),
                ("\"\" \"\"", &[(0..2, lit(Str, "")), (3..5, lit(Str, ""))]),
                ("\"\\\"\"", &[(0..4, lit(Str, "\\\""))]),
                ("unicode\"\"", &[(0..9, lit(UnicodeStr, ""))]),
                ("unicode \"\"", &[(0..7, id("unicode")), (8..10, lit(Str, ""))]),
                ("hex\"\"", &[(0..5, lit(HexStr, ""))]),
                ("hex \"\"", &[(0..3, id("hex")), (4..6, lit(Str, ""))]),
                //
                ("0", &[(0..1, lit(Integer, "0"))]),
                ("0a", &[(0..1, lit(Integer, "0")), (1..2, id("a"))]),
                ("0xa", &[(0..3, lit(Integer, "0xa"))]),
                ("0.", &[(0..2, lit(Rational, "0."))]),
                ("0.e1", &[(0..1, lit(Integer, "0")), (1..2, Dot), (2..4, id("e1"))]),
                (
                    "0.e-1",
                    &[
                        (0..1, lit(Integer, "0")),
                        (1..2, Dot),
                        (2..3, id("e")),
                        (3..4, BinOp(Minus)),
                        (4..5, lit(Integer, "1")),
                    ],
                ),
                ("0.0", &[(0..3, lit(Rational, "0.0"))]),
                ("0.0e1", &[(0..5, lit(Rational, "0.0e1"))]),
                ("0.0e-1", &[(0..6, lit(Rational, "0.0e-1"))]),
                ("0e1", &[(0..3, lit(Rational, "0e1"))]),
                ("0e1.", &[(0..3, lit(Rational, "0e1")), (3..4, Dot)]),
            ]);
        });
    }

    #[test]
    fn idents() {
        sulk_interface::create_session_globals_then(|| {
            checks(&[
                ("$", &[(0..1, id("$"))]),
                ("a$", &[(0..2, id("a$"))]),
                ("a_$123_", &[(0..7, id("a_$123_"))]),
                ("   b", &[(3..4, id("b"))]),
                (" c\t ", &[(1..2, id("c"))]),
                (" \td ", &[(2..3, id("d"))]),
                (" \t\nef ", &[(3..5, id("ef"))]),
                (" \t\n\tghi ", &[(4..7, id("ghi"))]),
            ]);
        });
    }

    #[test]
    fn doc_comments() {
        use CommentKind::*;
        sulk_interface::create_session_globals_then(|| {
            checks(&[
                ("// line comment", &[]),
                ("// / line comment", &[]),
                ("// ! line comment", &[]),
                ("// /* line comment", &[]), // */ <-- aaron-bond.better-comments doesn't like this
                ("/// line doc-comment", &[(0..20, DocComment(Line, sym(" line doc-comment")))]),
                ("//// invalid doc-comment", &[]),
                ("///// invalid doc-comment", &[]),
                //
                ("/**/", &[]),
                ("/***/", &[]),
                ("/****/", &[]),
                ("/*/*/", &[]),
                ("/* /*/", &[]),
                ("/*/**/", &[]),
                ("/* /**/", &[]),
                ("/* normal block comment */", &[]),
                ("/* /* normal block comment */", &[]),
                (
                    "/** block doc-comment */",
                    &[(0..24, DocComment(Block, sym(" block doc-comment ")))],
                ),
                (
                    "/** /* block doc-comment */",
                    &[(0..27, DocComment(Block, sym(" /* block doc-comment ")))],
                ),
                (
                    "/** block doc-comment /*/",
                    &[(0..25, DocComment(Block, sym(" block doc-comment /")))],
                ),
            ]);
        });
    }

    #[test]
    fn operators() {
        use Delimiter::*;
        // From Solc `TOKEN_LIST`: https://github.com/ethereum/solidity/blob/194b114664c7daebc2ff68af3c573272f5d28913/liblangutil/Token.h#L67
        checks(&[
            (")", &[(0..1, CloseDelim(Parenthesis))]),
            ("(", &[(0..1, OpenDelim(Parenthesis))]),
            ("[", &[(0..1, OpenDelim(Bracket))]),
            ("]", &[(0..1, CloseDelim(Bracket))]),
            ("{", &[(0..1, OpenDelim(Brace))]),
            ("}", &[(0..1, CloseDelim(Brace))]),
            (":", &[(0..1, Colon)]),
            (";", &[(0..1, Semi)]),
            (".", &[(0..1, Dot)]),
            ("?", &[(0..1, Question)]),
            ("=>", &[(0..2, FatArrow)]),
            ("->", &[(0..2, Arrow)]),
            ("=", &[(0..1, Eq)]),
            ("|=", &[(0..2, BinOpEq(Or))]),
            ("^=", &[(0..2, BinOpEq(Caret))]),
            ("&=", &[(0..2, BinOpEq(And))]),
            ("<<=", &[(0..3, BinOpEq(Shl))]),
            (">>=", &[(0..3, BinOpEq(Shr))]),
            (">>>=", &[(0..4, BinOpEq(Sar))]),
            ("+=", &[(0..2, BinOpEq(Plus))]),
            ("-=", &[(0..2, BinOpEq(Minus))]),
            ("*=", &[(0..2, BinOpEq(Star))]),
            ("/=", &[(0..2, BinOpEq(Slash))]),
            ("%=", &[(0..2, BinOpEq(Percent))]),
            (",", &[(0..1, Comma)]),
            ("||", &[(0..2, OrOr)]),
            ("&&", &[(0..2, AndAnd)]),
            ("|", &[(0..1, BinOp(Or))]),
            ("^", &[(0..1, BinOp(Caret))]),
            ("&", &[(0..1, BinOp(And))]),
            ("<<", &[(0..2, BinOp(Shl))]),
            (">>", &[(0..2, BinOp(Shr))]),
            (">>>", &[(0..3, BinOp(Sar))]),
            ("+", &[(0..1, BinOp(Plus))]),
            ("-", &[(0..1, BinOp(Minus))]),
            ("*", &[(0..1, BinOp(Star))]),
            ("/", &[(0..1, BinOp(Slash))]),
            ("%", &[(0..1, BinOp(Percent))]),
            ("**", &[(0..2, StarStar)]),
            ("==", &[(0..2, EqEq)]),
            ("!=", &[(0..2, Ne)]),
            ("<", &[(0..1, Lt)]),
            (">", &[(0..1, Gt)]),
            ("<=", &[(0..2, Le)]),
            (">=", &[(0..2, Ge)]),
            ("!", &[(0..1, Not)]),
            ("~", &[(0..1, Tilde)]),
            ("++", &[(0..2, PlusPlus)]),
            ("--", &[(0..2, MinusMinus)]),
            (":=", &[(0..2, Walrus)]),
        ]);
    }

    #[test]
    fn glueing() {
        checks(&[
            ("=", &[(0..1, Eq)]),
            ("==", &[(0..2, EqEq)]),
            ("= =", &[(0..1, Eq), (2..3, Eq)]),
            ("===", &[(0..2, EqEq), (2..3, Eq)]),
            ("== =", &[(0..2, EqEq), (3..4, Eq)]),
            ("= ==", &[(0..1, Eq), (2..4, EqEq)]),
            ("====", &[(0..2, EqEq), (2..4, EqEq)]),
            ("== ==", &[(0..2, EqEq), (3..5, EqEq)]),
            ("= ===", &[(0..1, Eq), (2..4, EqEq), (4..5, Eq)]),
            ("=====", &[(0..2, EqEq), (2..4, EqEq), (4..5, Eq)]),
            //
            (" <", &[(1..2, Lt)]),
            (" <=", &[(1..3, Le)]),
            (" < =", &[(1..2, Lt), (3..4, Eq)]),
            (" <<", &[(1..3, BinOp(Shl))]),
            (" <<=", &[(1..4, BinOpEq(Shl))]),
            //
            (" >", &[(1..2, Gt)]),
            (" >=", &[(1..3, Ge)]),
            (" > =", &[(1..2, Gt), (3..4, Eq)]),
            (" >>", &[(1..3, BinOp(Shr))]),
            (" >>>", &[(1..4, BinOp(Sar))]),
            (" >>>=", &[(1..5, BinOpEq(Sar))]),
            //
            ("+", &[(0..1, BinOp(Plus))]),
            ("++", &[(0..2, PlusPlus)]),
            ("+++", &[(0..2, PlusPlus), (2..3, BinOp(Plus))]),
            ("+ =", &[(0..1, BinOp(Plus)), (2..3, Eq)]),
            ("+ +=", &[(0..1, BinOp(Plus)), (2..4, BinOpEq(Plus))]),
            ("+++=", &[(0..2, PlusPlus), (2..4, BinOpEq(Plus))]),
            ("+ +", &[(0..1, BinOp(Plus)), (2..3, BinOp(Plus))]),
            //
            ("-", &[(0..1, BinOp(Minus))]),
            ("--", &[(0..2, MinusMinus)]),
            ("---", &[(0..2, MinusMinus), (2..3, BinOp(Minus))]),
            ("- =", &[(0..1, BinOp(Minus)), (2..3, Eq)]),
            ("- -=", &[(0..1, BinOp(Minus)), (2..4, BinOpEq(Minus))]),
            ("---=", &[(0..2, MinusMinus), (2..4, BinOpEq(Minus))]),
            ("- -", &[(0..1, BinOp(Minus)), (2..3, BinOp(Minus))]),
        ]);
    }
}