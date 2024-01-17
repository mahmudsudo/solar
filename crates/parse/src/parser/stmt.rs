use super::item::VarFlags;
use crate::{parser::SeqSep, PResult, Parser};
use sulk_ast::{ast::*, token::*};
use sulk_interface::{kw, Ident, Span};

impl<'a> Parser<'a> {
    /// Parses a statement.
    pub fn parse_stmt(&mut self) -> PResult<'a, Stmt> {
        let docs = self.parse_doc_comments()?;
        self.parse_spanned(Self::parse_stmt_kind).map(|(span, kind)| Stmt { docs, kind, span })
    }

    /// Parses a statement kind.
    fn parse_stmt_kind(&mut self) -> PResult<'a, StmtKind> {
        let mut semi = true;
        let kind = if self.eat_keyword(kw::If) {
            semi = false;
            self.parse_stmt_if()
        } else if self.eat_keyword(kw::While) {
            semi = false;
            self.parse_stmt_while()
        } else if self.eat_keyword(kw::Do) {
            self.parse_stmt_do_while()
        } else if self.eat_keyword(kw::For) {
            semi = false;
            self.parse_stmt_for()
        } else if self.eat_keyword(kw::Unchecked) {
            semi = false;
            self.parse_block().map(StmtKind::UncheckedBlock)
        } else if self.check(&TokenKind::OpenDelim(Delimiter::Brace)) {
            semi = false;
            self.parse_block().map(StmtKind::Block)
        } else if self.eat_keyword(kw::Continue) {
            Ok(StmtKind::Continue)
        } else if self.eat_keyword(kw::Break) {
            Ok(StmtKind::Break)
        } else if self.eat_keyword(kw::Return) {
            let expr = if self.check(&TokenKind::Semi) { None } else { Some(self.parse_expr()?) };
            Ok(StmtKind::Return(expr))
        } else if self.eat_keyword(kw::Throw) {
            let msg = "`throw` statements have been removed; use `revert`, `require`, or `assert` instead";
            Err(self.dcx().err(msg).span(self.prev_token.span))
        } else if self.eat_keyword(kw::Try) {
            semi = false;
            self.parse_stmt_try().map(StmtKind::Try)
        } else if self.eat_keyword(kw::Assembly) {
            semi = false;
            self.parse_stmt_assembly().map(StmtKind::Assembly)
        } else if self.eat_keyword(kw::Emit) {
            self.parse_path_call().map(|(path, params)| StmtKind::Emit(path, params))
        } else if self.check_keyword(kw::Revert) && self.look_ahead(1).is_ident() {
            self.bump(); // `revert`
            self.parse_path_call().map(|(path, params)| StmtKind::Revert(path, params))
        } else {
            self.parse_simple_stmt_kind()
        };
        if semi && kind.is_ok() {
            self.expect_semi()?;
        }
        kind
    }

    /// Parses a block of statements.
    pub(super) fn parse_block(&mut self) -> PResult<'a, Block> {
        self.parse_delim_seq(Delimiter::Brace, SeqSep::none(), true, Self::parse_stmt)
            .map(|(x, _)| x)
    }

    /// Parses an if statement.
    fn parse_stmt_if(&mut self) -> PResult<'a, StmtKind> {
        self.expect(&TokenKind::OpenDelim(Delimiter::Parenthesis))?;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::CloseDelim(Delimiter::Parenthesis))?;
        let true_stmt = self.parse_stmt()?;
        let else_stmt =
            if self.eat_keyword(kw::Else) { Some(Box::new(self.parse_stmt()?)) } else { None };
        Ok(StmtKind::If(expr, Box::new(true_stmt), else_stmt))
    }

    /// Parses a while statement.
    fn parse_stmt_while(&mut self) -> PResult<'a, StmtKind> {
        self.expect(&TokenKind::OpenDelim(Delimiter::Parenthesis))?;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::CloseDelim(Delimiter::Parenthesis))?;
        let stmt = self.parse_stmt()?;
        Ok(StmtKind::While(expr, Box::new(stmt)))
    }

    /// Parses a do-while statement.
    fn parse_stmt_do_while(&mut self) -> PResult<'a, StmtKind> {
        let block = self.parse_block()?;
        self.expect_keyword(kw::While)?;
        self.expect(&TokenKind::OpenDelim(Delimiter::Parenthesis))?;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::CloseDelim(Delimiter::Parenthesis))?;
        Ok(StmtKind::DoWhile(block, expr))
    }

    /// Parses a for statement.
    fn parse_stmt_for(&mut self) -> PResult<'a, StmtKind> {
        self.expect(&TokenKind::OpenDelim(Delimiter::Parenthesis))?;

        let init =
            if self.check(&TokenKind::Semi) { None } else { Some(self.parse_simple_stmt()?) };
        self.expect(&TokenKind::Semi)?;

        let cond = if self.check(&TokenKind::Semi) { None } else { Some(self.parse_expr()?) };
        self.expect_semi()?;

        let next = if self.check_noexpect(&TokenKind::CloseDelim(Delimiter::Parenthesis)) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect(&TokenKind::CloseDelim(Delimiter::Parenthesis))?;
        let body = Box::new(self.parse_stmt()?);
        Ok(StmtKind::For { init: init.map(Box::new), cond, next, body })
    }

    /// Parses a try statement.
    fn parse_stmt_try(&mut self) -> PResult<'a, StmtTry> {
        let expr = self.parse_expr()?;
        let returns = if self.eat_keyword(kw::Returns) {
            self.parse_parameter_list(false, VarFlags::FUNCTION)?
        } else {
            Vec::new()
        };
        let block = self.parse_block()?;

        let mut catch = Vec::new();
        self.expect_keyword(kw::Catch)?;
        loop {
            let name = self.parse_ident_opt()?;
            let args = if self.check(&TokenKind::OpenDelim(Delimiter::Parenthesis)) {
                self.parse_parameter_list(false, VarFlags::FUNCTION)?
            } else {
                Vec::new()
            };
            let block = self.parse_block()?;
            catch.push(CatchClause { name, args, block });
            if !self.eat_keyword(kw::Catch) {
                break;
            }
        }

        Ok(StmtTry { expr, returns, block, catch })
    }

    /// Parses an assembly block.
    fn parse_stmt_assembly(&mut self) -> PResult<'a, StmtAssembly> {
        let dialect = self.parse_str_lit_opt();
        let flags = if self.check(&TokenKind::OpenDelim(Delimiter::Parenthesis)) {
            self.parse_paren_comma_seq(false, Self::parse_str_lit)?.0
        } else {
            Vec::new()
        };
        let block = self.parse_yul_block()?;
        Ok(StmtAssembly { dialect, flags, block })
    }

    /// Parses a simple statement. These are just variable declarations and expressions.
    fn parse_simple_stmt(&mut self) -> PResult<'a, Stmt> {
        let docs = self.parse_doc_comments()?;
        self.parse_spanned(Self::parse_simple_stmt_kind).map(|(span, kind)| Stmt {
            docs,
            kind,
            span,
        })
    }

    /// Parses a simple statement kind. These are just variable declarations and expressions.
    ///
    /// Also used in the for loop initializer. Does not parse the trailing semicolon.
    fn parse_simple_stmt_kind(&mut self) -> PResult<'a, StmtKind> {
        let lo = self.token.span;
        if self.eat(&TokenKind::OpenDelim(Delimiter::Parenthesis)) {
            let mut empty_components = 0;
            while self.eat(&TokenKind::Comma) {
                empty_components += 1;
            }

            let (statement_type, iap) = self.try_parse_iap()?;
            match statement_type {
                LookAheadInfo::VariableDeclaration => {
                    let mut variables = vec![None; empty_components];
                    let ty = iap.into_ty(self);
                    variables
                        .push(Some(self.parse_variable_definition_with(VarFlags::FUNCTION, ty)?));
                    self.parse_optional_items_seq_required(
                        Delimiter::Parenthesis,
                        &mut variables,
                        |this| this.parse_variable_definition(VarFlags::FUNCTION),
                    )?;
                    self.expect(&TokenKind::Eq)?;
                    let expr = self.parse_expr()?;
                    Ok(StmtKind::DeclMulti(variables, expr))
                }
                LookAheadInfo::Expression => {
                    let mut components = vec![None; empty_components];
                    let expr = iap.into_expr(self);
                    components.push(Some(self.parse_expr_with(expr)?));
                    self.parse_optional_items_seq_required(
                        Delimiter::Parenthesis,
                        &mut components,
                        Self::parse_expr,
                    )?;
                    let partially_parsed = Expr {
                        span: lo.to(self.prev_token.span),
                        kind: ExprKind::Tuple(components),
                    };
                    self.parse_expr_with(Some(Box::new(partially_parsed))).map(StmtKind::Expr)
                }
                LookAheadInfo::IndexAccessStructure => unreachable!(),
            }
        } else {
            let (statement_type, iap) = self.try_parse_iap()?;
            match statement_type {
                LookAheadInfo::VariableDeclaration => {
                    let ty = iap.into_ty(self);
                    self.parse_variable_definition_with(VarFlags::VAR, ty).map(StmtKind::DeclSingle)
                }
                LookAheadInfo::Expression => {
                    let expr = iap.into_expr(self);
                    self.parse_expr_with(expr).map(StmtKind::Expr)
                }
                LookAheadInfo::IndexAccessStructure => unreachable!(),
            }
        }
    }

    /// Parses a `delim`-delimited, comma-separated list of maybe-optional items.
    /// E.g. `(a, b) => [Some, Some]`, `(, a,, b,) => [None, Some, None, Some, None]`.
    pub(super) fn parse_optional_items_seq<T>(
        &mut self,
        delim: Delimiter,
        mut f: impl FnMut(&mut Self) -> PResult<'a, T>,
    ) -> PResult<'a, Vec<Option<T>>> {
        self.expect(&TokenKind::OpenDelim(delim))?;
        let mut out = Vec::new();
        while self.eat(&TokenKind::Comma) {
            out.push(None);
        }
        if !self.check(&TokenKind::CloseDelim(delim)) {
            out.push(Some(f(self)?));
        }
        self.parse_optional_items_seq_required(delim, &mut out, f).map(|()| out)
    }

    fn parse_optional_items_seq_required<T>(
        &mut self,
        delim: Delimiter,
        out: &mut Vec<Option<T>>,
        mut f: impl FnMut(&mut Self) -> PResult<'a, T>,
    ) -> PResult<'a, ()> {
        let close = TokenKind::CloseDelim(delim);
        while !self.eat(&close) {
            self.expect(&TokenKind::Comma)?;
            if self.check(&TokenKind::Comma) || self.check(&close) {
                out.push(None);
            } else {
                out.push(Some(f(self)?));
            }
        }
        Ok(())
    }

    /// Parses a path and a list of call arguments.
    fn parse_path_call(&mut self) -> PResult<'a, (Path, CallArgs)> {
        let path = self.parse_path()?;
        let params = self.parse_call_args()?;
        Ok((path, params))
    }

    /// Never returns `LookAheadInfo::IndexAccessStructure`.
    fn try_parse_iap(&mut self) -> PResult<'a, (LookAheadInfo, IndexAccessedPath)> {
        // https://github.com/ethereum/solidity/blob/194b114664c7daebc2ff68af3c573272f5d28913/libsolidity/parsing/Parser.cpp#L1961
        if let ty @ (LookAheadInfo::VariableDeclaration | LookAheadInfo::Expression) =
            self.peek_statement_type()
        {
            return Ok((ty, IndexAccessedPath::default()));
        }

        let iap = self.parse_iap()?;
        let ty = if self.token.is_non_reserved_ident(self.in_yul)
            || self.token.is_location_specifier()
        {
            // `a.b memory`, `a[b] c`
            LookAheadInfo::VariableDeclaration
        } else {
            LookAheadInfo::Expression
        };
        Ok((ty, iap))
    }

    fn peek_statement_type(&mut self) -> LookAheadInfo {
        // https://github.com/ethereum/solidity/blob/194b114664c7daebc2ff68af3c573272f5d28913/libsolidity/parsing/Parser.cpp#L2528
        if self.token.is_keyword_any(&[kw::Mapping, kw::Function]) {
            return LookAheadInfo::VariableDeclaration;
        }

        if self.check_nr_ident() || self.check_elementary_type() {
            let next = self.look_ahead(1);
            if self.token.is_elementary_type() && next.is_ident_where(|id| id.name == kw::Payable) {
                return LookAheadInfo::VariableDeclaration;
            }
            if next.is_non_reserved_ident(self.in_yul) || next.is_location_specifier() {
                return LookAheadInfo::VariableDeclaration;
            }
            if matches!(next.kind, TokenKind::OpenDelim(Delimiter::Bracket) | TokenKind::Dot) {
                return LookAheadInfo::IndexAccessStructure;
            }
        }
        LookAheadInfo::Expression
    }

    fn parse_iap(&mut self) -> PResult<'a, IndexAccessedPath> {
        // https://github.com/ethereum/solidity/blob/194b114664c7daebc2ff68af3c573272f5d28913/libsolidity/parsing/Parser.cpp#L2559
        let mut path = Vec::new();
        if self.check_nr_ident() {
            path.push(IapKind::Member(self.parse_ident()?));
            while self.eat(&TokenKind::Dot) {
                let id = self.ident_or_err(true)?;
                if id.name != kw::Address && id.is_reserved(self.in_yul) {
                    self.expected_ident_found_err().emit();
                }
                self.bump(); // `id`
                path.push(IapKind::Member(id));
            }
        } else {
            let (span, kind) = self.parse_spanned(Self::parse_elementary_type)?;
            path.push(IapKind::MemberTy(span, kind));
        }
        let n_idents = path.len();

        while self.check(&TokenKind::OpenDelim(Delimiter::Bracket)) {
            let (span, kind) = self.parse_spanned(Self::parse_expr_index_kind)?;
            path.push(IapKind::Index(span, kind));
        }

        Ok(IndexAccessedPath { path, n_idents })
    }
}

#[derive(Debug)]
enum LookAheadInfo {
    /// `a.b`, `a[b]`
    IndexAccessStructure,
    VariableDeclaration,
    Expression,
}

#[derive(Debug)]
enum IapKind {
    /// `[...]`
    Index(Span, IndexKind),
    /// `<ident>` or `.<ident>`
    Member(Ident),
    /// `<ty>`
    MemberTy(Span, TyKind),
}

#[derive(Debug, Default)]
struct IndexAccessedPath {
    path: Vec<IapKind>,
    /// The number of elements in `path` that are `IapKind::Member[Ty]` at the start.
    n_idents: usize,
}

impl IndexAccessedPath {
    fn into_ty(self, parser: &mut Parser<'_>) -> Option<Ty> {
        // https://github.com/ethereum/solidity/blob/194b114664c7daebc2ff68af3c573272f5d28913/libsolidity/parsing/Parser.cpp#L2617
        let [first, ..] = &self.path[..] else { return None };

        let mut ty = if let IapKind::MemberTy(span, kind) = first {
            debug_assert_eq!(self.n_idents, 1);
            Ty { span: *span, kind: kind.clone() }
        } else {
            debug_assert!(self.n_idents >= 1);
            let path: Path = self
                .path
                .iter()
                .map(|x| match x {
                    IapKind::Member(id) => *id,
                    kind => unreachable!("{kind:?}"),
                })
                .take(self.n_idents)
                .collect();
            Ty { span: path.span(), kind: TyKind::Custom(path) }
        };

        for index in self.path.into_iter().skip(self.n_idents) {
            let IapKind::Index(span, kind) = index else { panic!("parsed too much") };
            let size = match kind {
                IndexKind::Index(expr) => expr,
                IndexKind::Range(l, r) => {
                    let msg = "expected array length, got range expression";
                    parser.dcx().err(msg).span(span).emit();
                    l.or(r)
                }
            };
            let span = ty.span.to(span);
            ty = Ty { span, kind: TyKind::Array(Box::new(TypeArray { element: ty, size })) };
        }

        Some(ty)
    }

    fn into_expr(self, _parser: &mut Parser<'_>) -> Option<Box<Expr>> {
        // https://github.com/ethereum/solidity/blob/194b114664c7daebc2ff68af3c573272f5d28913/libsolidity/parsing/Parser.cpp#L2658
        let mut path = self.path.into_iter();

        let mut expr = Box::new(match path.next()? {
            IapKind::Member(ident) => Expr::from_ident(ident),
            IapKind::MemberTy(span, kind) => Expr { span, kind: ExprKind::Type(Ty { span, kind }) },
            IapKind::Index(..) => panic!("should not happen"),
        });
        for index in path {
            expr = Box::new(match index {
                IapKind::Member(ident) => {
                    Expr { span: expr.span.to(ident.span), kind: ExprKind::Member(expr, ident) }
                }
                IapKind::MemberTy(..) => panic!("should not happen"),
                IapKind::Index(span, kind) => {
                    Expr { span: expr.span.to(span), kind: ExprKind::Index(expr, kind) }
                }
            });
        }
        Some(expr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sulk_interface::{source_map::FileName, Session};

    #[test]
    fn optional_items_seq() {
        fn check(tests: &[(&str, &[Option<&str>])]) {
            sulk_interface::enter(|| {
                let sess = Session::with_test_emitter(false);
                for (i, &(s, results)) in tests.iter().enumerate() {
                    let name = i.to_string();
                    let mut parser =
                        Parser::from_source_code(&sess, FileName::Custom(name), s.into());

                    let list = parser
                        .parse_optional_items_seq(Delimiter::Parenthesis, Parser::parse_ident)
                        .map_err(|e| e.emit())
                        .unwrap_or_else(|_| panic!("src: {s:?}"));
                    sess.dcx.has_errors().unwrap();
                    let formatted: Vec<_> =
                        list.iter().map(|o| o.as_ref().map(|i| i.as_str())).collect();
                    assert_eq!(formatted.as_slice(), results, "{s:?}");
                }
            })
            .unwrap();
        }

        check(&[
            ("()", &[]),
            ("(a)", &[Some("a")]),
            // ("(,)", &[None, None]),
            ("(a,)", &[Some("a"), None]),
            ("(,b)", &[None, Some("b")]),
            ("(a,b)", &[Some("a"), Some("b")]),
            ("(a,b,)", &[Some("a"), Some("b"), None]),
            // ("(,,)", &[None, None, None]),
            ("(a,,)", &[Some("a"), None, None]),
            ("(a,b,)", &[Some("a"), Some("b"), None]),
            ("(a,b,c)", &[Some("a"), Some("b"), Some("c")]),
            ("(,b,c)", &[None, Some("b"), Some("c")]),
            ("(,,c)", &[None, None, Some("c")]),
            ("(a,,c)", &[Some("a"), None, Some("c")]),
        ]);
    }
}
