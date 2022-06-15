use crate::assert::expr_if_not;
use rustc_ast::{
    attr,
    ptr::P,
    token,
    tokenstream::{DelimSpan, TokenStream, TokenTree},
    BorrowKind, Expr, ExprKind, ItemKind, MacArgs, MacCall, MacDelimiter, Mutability, Path,
    PathSegment, Stmt, UseTree, UseTreeKind, DUMMY_NODE_ID,
};
use rustc_ast_pretty::pprust;
use rustc_data_structures::fx::FxHashSet;
use rustc_expand::base::ExtCtxt;
use rustc_span::{
    symbol::{sym, Ident, Symbol},
    Span,
};

pub(super) struct Context<'cx, 'a> {
    // Top-level `let captureN = Capture::new()` statements
    capture_decls: Vec<Capture>,
    cx: &'cx ExtCtxt<'a>,
    // Formatting string used for debugging
    fmt_string: String,
    // Top-level `let __local_bindN = &expr` statements
    local_bind_decls: Vec<Stmt>,
    // Used to avoid capturing duplicated paths
    //
    // ```rust
    // let a = 1i32;
    // assert!(add(a, a) == 3);
    // ```
    paths: FxHashSet<Ident>,
    span: Span,
}

impl<'cx, 'a> Context<'cx, 'a> {
    pub(super) fn new(cx: &'cx ExtCtxt<'a>, span: Span) -> Self {
        Self {
            capture_decls: <_>::default(),
            cx,
            fmt_string: <_>::default(),
            local_bind_decls: <_>::default(),
            paths: <_>::default(),
            span,
        }
    }

    /// Builds the whole `assert!` expression. For example, `let elem = 1; assert!(elem == 1);` expands to:
    ///
    /// ```rust
    /// let elem = 1;
    /// {
    ///   #[allow(unused_imports)]
    ///   use ::core::asserting::{TryCaptureGeneric, TryCapturePrintable};
    ///   let mut __capture0 = ::core::asserting::Capture::new();
    ///   let __local_bind0 = &elem;
    ///   if !(
    ///     *{
    ///       (&::core::asserting::Wrapper(__local_bind0)).try_capture(&mut __capture0);
    ///       __local_bind0
    ///     } == 1
    ///   ) {
    ///     panic!("Assertion failed: elem == 1\nWith captures:\n  elem = {}", __capture0)
    ///   }
    /// }
    /// ```
    pub(super) fn build(mut self, mut cond_expr: P<Expr>, panic_path: Path) -> P<Expr> {
        let expr_str = pprust::expr_to_string(&cond_expr);
        self.manage_cond_expr(&mut cond_expr);
        let initial_imports = self.build_initial_imports();
        let panic = self.build_panic(&expr_str, panic_path);

        let Self { capture_decls, cx, local_bind_decls, span, .. } = self;

        let mut stmts = Vec::with_capacity(4);
        stmts.push(initial_imports);
        stmts.extend(capture_decls.into_iter().map(|c| c.decl));
        stmts.extend(local_bind_decls);
        stmts.push(cx.stmt_expr(expr_if_not(cx, span, cond_expr, panic, None)));
        cx.expr_block(cx.block(span, stmts))
    }

    /// Initial **trait** imports
    ///
    /// use ::core::asserting::{ ... };
    fn build_initial_imports(&self) -> Stmt {
        let nested_tree = |this: &Self, sym| {
            (
                UseTree {
                    prefix: this.cx.path(this.span, vec![Ident::with_dummy_span(sym)]),
                    kind: UseTreeKind::Simple(None, DUMMY_NODE_ID, DUMMY_NODE_ID),
                    span: this.span,
                },
                DUMMY_NODE_ID,
            )
        };
        self.cx.stmt_item(
            self.span,
            self.cx.item(
                self.span,
                Ident::empty(),
                vec![self.cx.attribute(attr::mk_list_item(
                    Ident::new(sym::allow, self.span),
                    vec![attr::mk_nested_word_item(Ident::new(sym::unused_imports, self.span))],
                ))],
                ItemKind::Use(UseTree {
                    prefix: self.cx.path(self.span, self.cx.std_path(&[sym::asserting])),
                    kind: UseTreeKind::Nested(vec![
                        nested_tree(self, sym::TryCaptureGeneric),
                        nested_tree(self, sym::TryCapturePrintable),
                    ]),
                    span: self.span,
                }),
            ),
        )
    }

    /// The necessary custom `panic!(...)` expression.
    ///
    /// panic!(
    ///     "Assertion failed: ... \n With expansion: ...",
    ///     __capture0,
    ///     ...
    /// );
    fn build_panic(&self, expr_str: &str, panic_path: Path) -> P<Expr> {
        let escaped_expr_str = escape_to_fmt(expr_str);
        let initial = [
            TokenTree::token(
                token::Literal(token::Lit {
                    kind: token::LitKind::Str,
                    symbol: Symbol::intern(&if self.fmt_string.is_empty() {
                        format!("Assertion failed: {escaped_expr_str}")
                    } else {
                        format!(
                            "Assertion failed: {escaped_expr_str}\nWith captures:\n{}",
                            &self.fmt_string
                        )
                    }),
                    suffix: None,
                }),
                self.span,
            ),
            TokenTree::token(token::Comma, self.span),
        ];
        let captures = self.capture_decls.iter().flat_map(|cap| {
            [
                TokenTree::token(token::Ident(cap.ident.name, false), cap.ident.span),
                TokenTree::token(token::Comma, self.span),
            ]
        });
        self.cx.expr(
            self.span,
            ExprKind::MacCall(MacCall {
                path: panic_path,
                args: P(MacArgs::Delimited(
                    DelimSpan::from_single(self.span),
                    MacDelimiter::Parenthesis,
                    initial.into_iter().chain(captures).collect::<TokenStream>(),
                )),
                prior_type_ascription: None,
            }),
        )
    }

    /// Recursive function called until `cond_expr` and `fmt_str` are fully modified.
    ///
    /// See [Self::manage_initial_capture] and [Self::manage_try_capture]
    fn manage_cond_expr(&mut self, expr: &mut P<Expr>) {
        match (*expr).kind {
            ExprKind::Binary(_, ref mut lhs, ref mut rhs) => {
                self.manage_cond_expr(lhs);
                self.manage_cond_expr(rhs);
            }
            ExprKind::Path(_, Path { ref segments, .. }) if let &[ref path_segment] = &segments[..] => {
                let path_ident = path_segment.ident;
                self.manage_initial_capture(expr, path_ident);
            }
            _ => {}
        }
    }

    /// Pushes the top-level declarations and modifies `expr` to try capturing variables.
    ///
    /// `fmt_str`, the formatting string used for debugging, is constructed to show possible
    /// captured variables.
    fn manage_initial_capture(&mut self, expr: &mut P<Expr>, path_ident: Ident) {
        if self.paths.contains(&path_ident) {
            return;
        } else {
            self.fmt_string.push_str("  ");
            self.fmt_string.push_str(path_ident.as_str());
            self.fmt_string.push_str(" = {:?}\n");
            let _ = self.paths.insert(path_ident);
        }
        let curr_capture_idx = self.capture_decls.len();
        let capture_string = format!("__capture{curr_capture_idx}");
        let ident = Ident::new(Symbol::intern(&capture_string), self.span);
        let init_std_path = self.cx.std_path(&[sym::asserting, sym::Capture, sym::new]);
        let init = self.cx.expr_call(
            self.span,
            self.cx.expr_path(self.cx.path(self.span, init_std_path)),
            vec![],
        );
        let capture = Capture { decl: self.cx.stmt_let(self.span, true, ident, init), ident };
        self.capture_decls.push(capture);
        self.manage_try_capture(ident, curr_capture_idx, expr);
    }

    /// Tries to copy `__local_bindN` into `__captureN`.
    ///
    /// *{
    ///    (&Wrapper(__local_bindN)).try_capture(&mut __captureN);
    ///    __local_bindN
    /// }
    fn manage_try_capture(&mut self, capture: Ident, curr_capture_idx: usize, expr: &mut P<Expr>) {
        let local_bind_string = format!("__local_bind{curr_capture_idx}");
        let local_bind = Ident::new(Symbol::intern(&local_bind_string), self.span);
        self.local_bind_decls.push(self.cx.stmt_let(
            self.span,
            false,
            local_bind,
            self.cx.expr_addr_of(self.span, expr.clone()),
        ));
        let wrapper = self.cx.expr_call(
            self.span,
            self.cx.expr_path(
                self.cx.path(self.span, self.cx.std_path(&[sym::asserting, sym::Wrapper])),
            ),
            vec![self.cx.expr_path(Path::from_ident(local_bind))],
        );
        let try_capture_call = self
            .cx
            .stmt_expr(expr_method_call(
                self.cx,
                PathSegment {
                    args: None,
                    id: DUMMY_NODE_ID,
                    ident: Ident::new(sym::try_capture, self.span),
                },
                vec![
                    expr_paren(self.cx, self.span, self.cx.expr_addr_of(self.span, wrapper)),
                    expr_addr_of_mut(
                        self.cx,
                        self.span,
                        self.cx.expr_path(Path::from_ident(capture)),
                    ),
                ],
                self.span,
            ))
            .add_trailing_semicolon();
        let local_bind_path = self.cx.expr_path(Path::from_ident(local_bind));
        let ret = self.cx.stmt_expr(local_bind_path);
        let block = self.cx.expr_block(self.cx.block(self.span, vec![try_capture_call, ret]));
        *expr = self.cx.expr_deref(self.span, block);
    }
}

/// Information about a captured element.
#[derive(Debug)]
struct Capture {
    // Generated indexed `Capture` statement.
    //
    // `let __capture{} = Capture::new();`
    decl: Stmt,
    // The name of the generated indexed `Capture` variable.
    //
    // `__capture{}`
    ident: Ident,
}

/// Escapes to use as a formatting string.
fn escape_to_fmt(s: &str) -> String {
    let mut rslt = String::with_capacity(s.len());
    for c in s.chars() {
        rslt.extend(c.escape_debug());
        match c {
            '{' | '}' => rslt.push(c),
            _ => {}
        }
    }
    rslt
}

fn expr_addr_of_mut(cx: &ExtCtxt<'_>, sp: Span, e: P<Expr>) -> P<Expr> {
    cx.expr(sp, ExprKind::AddrOf(BorrowKind::Ref, Mutability::Mut, e))
}

fn expr_method_call(
    cx: &ExtCtxt<'_>,
    path: PathSegment,
    args: Vec<P<Expr>>,
    span: Span,
) -> P<Expr> {
    cx.expr(span, ExprKind::MethodCall(path, args, span))
}

fn expr_paren(cx: &ExtCtxt<'_>, sp: Span, e: P<Expr>) -> P<Expr> {
    cx.expr(sp, ExprKind::Paren(e))
}
