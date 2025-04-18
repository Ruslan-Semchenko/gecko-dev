use alloc::{
    borrow::ToOwned,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::num::NonZeroU32;

use crate::common::wgsl::TypeContext;
use crate::front::wgsl::error::{Error, ExpectedToken, InvalidAssignmentType};
use crate::front::wgsl::index::Index;
use crate::front::wgsl::parse::number::Number;
use crate::front::wgsl::parse::{ast, conv};
use crate::front::wgsl::Result;
use crate::front::Typifier;
use crate::proc::{
    ensure_block_returns, Alignment, ConstantEvaluator, Emitter, Layouter, ResolveContext,
};
use crate::{Arena, FastHashMap, FastIndexMap, Handle, Span};

mod construction;
mod conversion;

/// Resolves the inner type of a given expression.
///
/// Expects a &mut [`ExpressionContext`] and a [`Handle<Expression>`].
///
/// Returns a &[`crate::TypeInner`].
///
/// Ideally, we would simply have a function that takes a `&mut ExpressionContext`
/// and returns a `&TypeResolution`. Unfortunately, this leads the borrow checker
/// to conclude that the mutable borrow lasts for as long as we are using the
/// `&TypeResolution`, so we can't use the `ExpressionContext` for anything else -
/// like, say, resolving another operand's type. Using a macro that expands to
/// two separate calls, only the first of which needs a `&mut`,
/// lets the borrow checker see that the mutable borrow is over.
macro_rules! resolve_inner {
    ($ctx:ident, $expr:expr) => {{
        $ctx.grow_types($expr)?;
        $ctx.typifier()[$expr].inner_with(&$ctx.module.types)
    }};
}
pub(super) use resolve_inner;

/// Resolves the inner types of two given expressions.
///
/// Expects a &mut [`ExpressionContext`] and two [`Handle<Expression>`]s.
///
/// Returns a tuple containing two &[`crate::TypeInner`].
///
/// See the documentation of [`resolve_inner!`] for why this macro is necessary.
macro_rules! resolve_inner_binary {
    ($ctx:ident, $left:expr, $right:expr) => {{
        $ctx.grow_types($left)?;
        $ctx.grow_types($right)?;
        (
            $ctx.typifier()[$left].inner_with(&$ctx.module.types),
            $ctx.typifier()[$right].inner_with(&$ctx.module.types),
        )
    }};
}

/// Resolves the type of a given expression.
///
/// Expects a &mut [`ExpressionContext`] and a [`Handle<Expression>`].
///
/// Returns a &[`TypeResolution`].
///
/// See the documentation of [`resolve_inner!`] for why this macro is necessary.
///
/// [`TypeResolution`]: crate::proc::TypeResolution
macro_rules! resolve {
    ($ctx:ident, $expr:expr) => {{
        $ctx.grow_types($expr)?;
        &$ctx.typifier()[$expr]
    }};
}
pub(super) use resolve;

/// State for constructing a `crate::Module`.
pub struct GlobalContext<'source, 'temp, 'out> {
    /// The `TranslationUnit`'s expressions arena.
    ast_expressions: &'temp Arena<ast::Expression<'source>>,

    /// The `TranslationUnit`'s types arena.
    types: &'temp Arena<ast::Type<'source>>,

    // Naga IR values.
    /// The map from the names of module-scope declarations to the Naga IR
    /// `Handle`s we have built for them, owned by `Lowerer::lower`.
    globals: &'temp mut FastHashMap<&'source str, LoweredGlobalDecl>,

    /// The module we're constructing.
    module: &'out mut crate::Module,

    const_typifier: &'temp mut Typifier,

    layouter: &'temp mut Layouter,

    global_expression_kind_tracker: &'temp mut crate::proc::ExpressionKindTracker,
}

impl<'source> GlobalContext<'source, '_, '_> {
    fn as_const(&mut self) -> ExpressionContext<'source, '_, '_> {
        ExpressionContext {
            ast_expressions: self.ast_expressions,
            globals: self.globals,
            types: self.types,
            module: self.module,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            expr_type: ExpressionContextType::Constant(None),
            global_expression_kind_tracker: self.global_expression_kind_tracker,
        }
    }

    fn as_override(&mut self) -> ExpressionContext<'source, '_, '_> {
        ExpressionContext {
            ast_expressions: self.ast_expressions,
            globals: self.globals,
            types: self.types,
            module: self.module,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            expr_type: ExpressionContextType::Override,
            global_expression_kind_tracker: self.global_expression_kind_tracker,
        }
    }

    fn ensure_type_exists(
        &mut self,
        name: Option<String>,
        inner: crate::TypeInner,
    ) -> Handle<crate::Type> {
        self.module
            .types
            .insert(crate::Type { inner, name }, Span::UNDEFINED)
    }
}

/// State for lowering a statement within a function.
pub struct StatementContext<'source, 'temp, 'out> {
    // WGSL AST values.
    /// A reference to [`TranslationUnit::expressions`] for the translation unit
    /// we're lowering.
    ///
    /// [`TranslationUnit::expressions`]: ast::TranslationUnit::expressions
    ast_expressions: &'temp Arena<ast::Expression<'source>>,

    /// A reference to [`TranslationUnit::types`] for the translation unit
    /// we're lowering.
    ///
    /// [`TranslationUnit::types`]: ast::TranslationUnit::types
    types: &'temp Arena<ast::Type<'source>>,

    // Naga IR values.
    /// The map from the names of module-scope declarations to the Naga IR
    /// `Handle`s we have built for them, owned by `Lowerer::lower`.
    globals: &'temp mut FastHashMap<&'source str, LoweredGlobalDecl>,

    /// A map from each `ast::Local` handle to the Naga expression
    /// we've built for it:
    ///
    /// - WGSL function arguments become Naga [`FunctionArgument`] expressions.
    ///
    /// - WGSL `var` declarations become Naga [`LocalVariable`] expressions.
    ///
    /// - WGSL `let` declararations become arbitrary Naga expressions.
    ///
    /// This always borrows the `local_table` local variable in
    /// [`Lowerer::function`].
    ///
    /// [`LocalVariable`]: crate::Expression::LocalVariable
    /// [`FunctionArgument`]: crate::Expression::FunctionArgument
    local_table:
        &'temp mut FastHashMap<Handle<ast::Local>, Declared<Typed<Handle<crate::Expression>>>>,

    const_typifier: &'temp mut Typifier,
    typifier: &'temp mut Typifier,
    layouter: &'temp mut Layouter,
    function: &'out mut crate::Function,
    /// Stores the names of expressions that are assigned in `let` statement
    /// Also stores the spans of the names, for use in errors.
    named_expressions: &'out mut FastIndexMap<Handle<crate::Expression>, (String, Span)>,
    module: &'out mut crate::Module,

    /// Which `Expression`s in `self.naga_expressions` are const expressions, in
    /// the WGSL sense.
    ///
    /// According to the WGSL spec, a const expression must not refer to any
    /// `let` declarations, even if those declarations' initializers are
    /// themselves const expressions. So this tracker is not simply concerned
    /// with the form of the expressions; it is also tracking whether WGSL says
    /// we should consider them to be const. See the use of `force_non_const` in
    /// the code for lowering `let` bindings.
    local_expression_kind_tracker: &'temp mut crate::proc::ExpressionKindTracker,
    global_expression_kind_tracker: &'temp mut crate::proc::ExpressionKindTracker,
}

impl<'a, 'temp> StatementContext<'a, 'temp, '_> {
    fn as_const<'t>(
        &'t mut self,
        block: &'t mut crate::Block,
        emitter: &'t mut Emitter,
    ) -> ExpressionContext<'a, 't, 't>
    where
        'temp: 't,
    {
        ExpressionContext {
            globals: self.globals,
            types: self.types,
            ast_expressions: self.ast_expressions,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            global_expression_kind_tracker: self.global_expression_kind_tracker,
            module: self.module,
            expr_type: ExpressionContextType::Constant(Some(LocalExpressionContext {
                local_table: self.local_table,
                function: self.function,
                block,
                emitter,
                typifier: self.typifier,
                local_expression_kind_tracker: self.local_expression_kind_tracker,
            })),
        }
    }

    fn as_expression<'t>(
        &'t mut self,
        block: &'t mut crate::Block,
        emitter: &'t mut Emitter,
    ) -> ExpressionContext<'a, 't, 't>
    where
        'temp: 't,
    {
        ExpressionContext {
            globals: self.globals,
            types: self.types,
            ast_expressions: self.ast_expressions,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            global_expression_kind_tracker: self.global_expression_kind_tracker,
            module: self.module,
            expr_type: ExpressionContextType::Runtime(LocalExpressionContext {
                local_table: self.local_table,
                function: self.function,
                block,
                emitter,
                typifier: self.typifier,
                local_expression_kind_tracker: self.local_expression_kind_tracker,
            }),
        }
    }

    #[allow(dead_code)]
    fn as_global(&mut self) -> GlobalContext<'a, '_, '_> {
        GlobalContext {
            ast_expressions: self.ast_expressions,
            globals: self.globals,
            types: self.types,
            module: self.module,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            global_expression_kind_tracker: self.global_expression_kind_tracker,
        }
    }

    fn invalid_assignment_type(&self, expr: Handle<crate::Expression>) -> InvalidAssignmentType {
        if let Some(&(_, span)) = self.named_expressions.get(&expr) {
            InvalidAssignmentType::ImmutableBinding(span)
        } else {
            match self.function.expressions[expr] {
                crate::Expression::Swizzle { .. } => InvalidAssignmentType::Swizzle,
                crate::Expression::Access { base, .. } => self.invalid_assignment_type(base),
                crate::Expression::AccessIndex { base, .. } => self.invalid_assignment_type(base),
                _ => InvalidAssignmentType::Other,
            }
        }
    }
}

pub struct LocalExpressionContext<'temp, 'out> {
    /// A map from [`ast::Local`] handles to the Naga expressions we've built for them.
    ///
    /// This is always [`StatementContext::local_table`] for the
    /// enclosing statement; see that documentation for details.
    local_table: &'temp FastHashMap<Handle<ast::Local>, Declared<Typed<Handle<crate::Expression>>>>,

    function: &'out mut crate::Function,
    block: &'temp mut crate::Block,
    emitter: &'temp mut Emitter,
    typifier: &'temp mut Typifier,

    /// Which `Expression`s in `self.naga_expressions` are const expressions, in
    /// the WGSL sense.
    ///
    /// See [`StatementContext::local_expression_kind_tracker`] for details.
    local_expression_kind_tracker: &'temp mut crate::proc::ExpressionKindTracker,
}

/// The type of Naga IR expression we are lowering an [`ast::Expression`] to.
pub enum ExpressionContextType<'temp, 'out> {
    /// We are lowering to an arbitrary runtime expression, to be
    /// included in a function's body.
    ///
    /// The given [`LocalExpressionContext`] holds information about local
    /// variables, arguments, and other definitions available only to runtime
    /// expressions, not constant or override expressions.
    Runtime(LocalExpressionContext<'temp, 'out>),

    /// We are lowering to a constant expression, to be included in the module's
    /// constant expression arena.
    ///
    /// Everything global constant expressions are allowed to refer to is
    /// available in the [`ExpressionContext`], but local constant expressions can
    /// also refer to other
    Constant(Option<LocalExpressionContext<'temp, 'out>>),

    /// We are lowering to an override expression, to be included in the module's
    /// constant expression arena.
    ///
    /// Everything override expressions are allowed to refer to is
    /// available in the [`ExpressionContext`], so this variant
    /// carries no further information.
    Override,
}

/// State for lowering an [`ast::Expression`] to Naga IR.
///
/// [`ExpressionContext`]s come in two kinds, distinguished by
/// the value of the [`expr_type`] field:
///
/// - A [`Runtime`] context contributes [`naga::Expression`]s to a [`naga::Function`]'s
///   runtime expression arena.
///
/// - A [`Constant`] context contributes [`naga::Expression`]s to a [`naga::Module`]'s
///   constant expression arena.
///
/// [`ExpressionContext`]s are constructed in restricted ways:
///
/// - To get a [`Runtime`] [`ExpressionContext`], call
///   [`StatementContext::as_expression`].
///
/// - To get a [`Constant`] [`ExpressionContext`], call
///   [`GlobalContext::as_const`].
///
/// - You can demote a [`Runtime`] context to a [`Constant`] context
///   by calling [`as_const`], but there's no way to go in the other
///   direction, producing a runtime context from a constant one. This
///   is because runtime expressions can refer to constant
///   expressions, via [`Expression::Constant`], but constant
///   expressions can't refer to a function's expressions.
///
/// Not to be confused with `wgsl::parse::ExpressionContext`, which is
/// for parsing the `ast::Expression` in the first place.
///
/// [`expr_type`]: ExpressionContext::expr_type
/// [`Runtime`]: ExpressionContextType::Runtime
/// [`naga::Expression`]: crate::Expression
/// [`naga::Function`]: crate::Function
/// [`Constant`]: ExpressionContextType::Constant
/// [`naga::Module`]: crate::Module
/// [`as_const`]: ExpressionContext::as_const
/// [`Expression::Constant`]: crate::Expression::Constant
pub struct ExpressionContext<'source, 'temp, 'out> {
    // WGSL AST values.
    ast_expressions: &'temp Arena<ast::Expression<'source>>,
    types: &'temp Arena<ast::Type<'source>>,

    // Naga IR values.
    /// The map from the names of module-scope declarations to the Naga IR
    /// `Handle`s we have built for them, owned by `Lowerer::lower`.
    globals: &'temp mut FastHashMap<&'source str, LoweredGlobalDecl>,

    /// The IR [`Module`] we're constructing.
    ///
    /// [`Module`]: crate::Module
    module: &'out mut crate::Module,

    /// Type judgments for [`module::global_expressions`].
    ///
    /// [`module::global_expressions`]: crate::Module::global_expressions
    const_typifier: &'temp mut Typifier,
    layouter: &'temp mut Layouter,
    global_expression_kind_tracker: &'temp mut crate::proc::ExpressionKindTracker,

    /// Whether we are lowering a constant expression or a general
    /// runtime expression, and the data needed in each case.
    expr_type: ExpressionContextType<'temp, 'out>,
}

impl TypeContext for ExpressionContext<'_, '_, '_> {
    fn lookup_type(&self, handle: Handle<crate::Type>) -> &crate::Type {
        &self.module.types[handle]
    }

    fn type_name(&self, handle: Handle<crate::Type>) -> &str {
        self.module.types[handle]
            .name
            .as_deref()
            .unwrap_or("{anonymous type}")
    }

    fn write_override<W: core::fmt::Write>(
        &self,
        handle: Handle<crate::Override>,
        out: &mut W,
    ) -> core::fmt::Result {
        match self.module.overrides[handle].name {
            Some(ref name) => out.write_str(name),
            None => write!(out, "{{anonymous override {handle:?}}}"),
        }
    }
}

impl<'source, 'temp, 'out> ExpressionContext<'source, 'temp, 'out> {
    #[allow(dead_code)]
    fn as_const(&mut self) -> ExpressionContext<'source, '_, '_> {
        ExpressionContext {
            globals: self.globals,
            types: self.types,
            ast_expressions: self.ast_expressions,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            module: self.module,
            expr_type: ExpressionContextType::Constant(match self.expr_type {
                ExpressionContextType::Runtime(ref mut local_expression_context)
                | ExpressionContextType::Constant(Some(ref mut local_expression_context)) => {
                    Some(LocalExpressionContext {
                        local_table: local_expression_context.local_table,
                        function: local_expression_context.function,
                        block: local_expression_context.block,
                        emitter: local_expression_context.emitter,
                        typifier: local_expression_context.typifier,
                        local_expression_kind_tracker: local_expression_context
                            .local_expression_kind_tracker,
                    })
                }
                ExpressionContextType::Constant(None) | ExpressionContextType::Override => None,
            }),
            global_expression_kind_tracker: self.global_expression_kind_tracker,
        }
    }

    fn as_global(&mut self) -> GlobalContext<'source, '_, '_> {
        GlobalContext {
            ast_expressions: self.ast_expressions,
            globals: self.globals,
            types: self.types,
            module: self.module,
            const_typifier: self.const_typifier,
            layouter: self.layouter,
            global_expression_kind_tracker: self.global_expression_kind_tracker,
        }
    }

    fn as_const_evaluator(&mut self) -> ConstantEvaluator {
        match self.expr_type {
            ExpressionContextType::Runtime(ref mut rctx) => ConstantEvaluator::for_wgsl_function(
                self.module,
                &mut rctx.function.expressions,
                rctx.local_expression_kind_tracker,
                self.layouter,
                rctx.emitter,
                rctx.block,
                false,
            ),
            ExpressionContextType::Constant(Some(ref mut rctx)) => {
                ConstantEvaluator::for_wgsl_function(
                    self.module,
                    &mut rctx.function.expressions,
                    rctx.local_expression_kind_tracker,
                    self.layouter,
                    rctx.emitter,
                    rctx.block,
                    true,
                )
            }
            ExpressionContextType::Constant(None) => ConstantEvaluator::for_wgsl_module(
                self.module,
                self.global_expression_kind_tracker,
                self.layouter,
                false,
            ),
            ExpressionContextType::Override => ConstantEvaluator::for_wgsl_module(
                self.module,
                self.global_expression_kind_tracker,
                self.layouter,
                true,
            ),
        }
    }

    fn append_expression(
        &mut self,
        expr: crate::Expression,
        span: Span,
    ) -> Result<'source, Handle<crate::Expression>> {
        let mut eval = self.as_const_evaluator();
        eval.try_eval_and_append(expr, span)
            .map_err(|e| Box::new(Error::ConstantEvaluatorError(e.into(), span)))
    }

    fn const_eval_expr_to_u32(
        &self,
        handle: Handle<crate::Expression>,
    ) -> core::result::Result<u32, crate::proc::U32EvalError> {
        match self.expr_type {
            ExpressionContextType::Runtime(ref ctx) => {
                if !ctx.local_expression_kind_tracker.is_const(handle) {
                    return Err(crate::proc::U32EvalError::NonConst);
                }

                self.module
                    .to_ctx()
                    .eval_expr_to_u32_from(handle, &ctx.function.expressions)
            }
            ExpressionContextType::Constant(Some(ref ctx)) => {
                assert!(ctx.local_expression_kind_tracker.is_const(handle));
                self.module
                    .to_ctx()
                    .eval_expr_to_u32_from(handle, &ctx.function.expressions)
            }
            ExpressionContextType::Constant(None) => self.module.to_ctx().eval_expr_to_u32(handle),
            ExpressionContextType::Override => Err(crate::proc::U32EvalError::NonConst),
        }
    }

    fn get_expression_span(&self, handle: Handle<crate::Expression>) -> Span {
        match self.expr_type {
            ExpressionContextType::Runtime(ref ctx)
            | ExpressionContextType::Constant(Some(ref ctx)) => {
                ctx.function.expressions.get_span(handle)
            }
            ExpressionContextType::Constant(None) | ExpressionContextType::Override => {
                self.module.global_expressions.get_span(handle)
            }
        }
    }

    fn typifier(&self) -> &Typifier {
        match self.expr_type {
            ExpressionContextType::Runtime(ref ctx)
            | ExpressionContextType::Constant(Some(ref ctx)) => ctx.typifier,
            ExpressionContextType::Constant(None) | ExpressionContextType::Override => {
                self.const_typifier
            }
        }
    }

    fn local(
        &mut self,
        local: &Handle<ast::Local>,
        span: Span,
    ) -> Result<'source, Typed<Handle<crate::Expression>>> {
        match self.expr_type {
            ExpressionContextType::Runtime(ref ctx) => Ok(ctx.local_table[local].runtime()),
            ExpressionContextType::Constant(Some(ref ctx)) => ctx.local_table[local]
                .const_time()
                .ok_or(Box::new(Error::UnexpectedOperationInConstContext(span))),
            _ => Err(Box::new(Error::UnexpectedOperationInConstContext(span))),
        }
    }

    fn runtime_expression_ctx(
        &mut self,
        span: Span,
    ) -> Result<'source, &mut LocalExpressionContext<'temp, 'out>> {
        match self.expr_type {
            ExpressionContextType::Runtime(ref mut ctx) => Ok(ctx),
            ExpressionContextType::Constant(_) | ExpressionContextType::Override => {
                Err(Box::new(Error::UnexpectedOperationInConstContext(span)))
            }
        }
    }

    fn gather_component(
        &mut self,
        expr: Handle<crate::Expression>,
        component_span: Span,
        gather_span: Span,
    ) -> Result<'source, crate::SwizzleComponent> {
        match self.expr_type {
            ExpressionContextType::Runtime(ref rctx) => {
                if !rctx.local_expression_kind_tracker.is_const(expr) {
                    return Err(Box::new(Error::ExpectedConstExprConcreteIntegerScalar(
                        component_span,
                    )));
                }

                let index = self
                    .module
                    .to_ctx()
                    .eval_expr_to_u32_from(expr, &rctx.function.expressions)
                    .map_err(|err| match err {
                        crate::proc::U32EvalError::NonConst => {
                            Error::ExpectedConstExprConcreteIntegerScalar(component_span)
                        }
                        crate::proc::U32EvalError::Negative => {
                            Error::ExpectedNonNegative(component_span)
                        }
                    })?;
                crate::SwizzleComponent::XYZW
                    .get(index as usize)
                    .copied()
                    .ok_or(Box::new(Error::InvalidGatherComponent(component_span)))
            }
            // This means a `gather` operation appeared in a constant expression.
            // This error refers to the `gather` itself, not its "component" argument.
            ExpressionContextType::Constant(_) | ExpressionContextType::Override => Err(Box::new(
                Error::UnexpectedOperationInConstContext(gather_span),
            )),
        }
    }

    /// Determine the type of `handle`, and add it to the module's arena.
    ///
    /// If you just need a `TypeInner` for `handle`'s type, use the
    /// [`resolve_inner!`] macro instead. This function
    /// should only be used when the type of `handle` needs to appear
    /// in the module's final `Arena<Type>`, for example, if you're
    /// creating a [`LocalVariable`] whose type is inferred from its
    /// initializer.
    ///
    /// [`LocalVariable`]: crate::LocalVariable
    fn register_type(
        &mut self,
        handle: Handle<crate::Expression>,
    ) -> Result<'source, Handle<crate::Type>> {
        self.grow_types(handle)?;
        // This is equivalent to calling ExpressionContext::typifier(),
        // except that this lets the borrow checker see that it's okay
        // to also borrow self.module.types mutably below.
        let typifier = match self.expr_type {
            ExpressionContextType::Runtime(ref ctx)
            | ExpressionContextType::Constant(Some(ref ctx)) => ctx.typifier,
            ExpressionContextType::Constant(None) | ExpressionContextType::Override => {
                &*self.const_typifier
            }
        };
        Ok(typifier.register_type(handle, &mut self.module.types))
    }

    /// Resolve the types of all expressions up through `handle`.
    ///
    /// Ensure that [`self.typifier`] has a [`TypeResolution`] for
    /// every expression in [`self.function.expressions`].
    ///
    /// This does not add types to any arena. The [`Typifier`]
    /// documentation explains the steps we take to avoid filling
    /// arenas with intermediate types.
    ///
    /// This function takes `&mut self`, so it can't conveniently
    /// return a shared reference to the resulting `TypeResolution`:
    /// the shared reference would extend the mutable borrow, and you
    /// wouldn't be able to use `self` for anything else. Instead, you
    /// should use [`register_type`] or one of [`resolve!`],
    /// [`resolve_inner!`] or [`resolve_inner_binary!`].
    ///
    /// [`self.typifier`]: ExpressionContext::typifier
    /// [`TypeResolution`]: crate::proc::TypeResolution
    /// [`register_type`]: Self::register_type
    /// [`Typifier`]: Typifier
    fn grow_types(&mut self, handle: Handle<crate::Expression>) -> Result<'source, &mut Self> {
        let empty_arena = Arena::new();
        let resolve_ctx;
        let typifier;
        let expressions;
        match self.expr_type {
            ExpressionContextType::Runtime(ref mut ctx)
            | ExpressionContextType::Constant(Some(ref mut ctx)) => {
                resolve_ctx = ResolveContext::with_locals(
                    self.module,
                    &ctx.function.local_variables,
                    &ctx.function.arguments,
                );
                typifier = &mut *ctx.typifier;
                expressions = &ctx.function.expressions;
            }
            ExpressionContextType::Constant(None) | ExpressionContextType::Override => {
                resolve_ctx = ResolveContext::with_locals(self.module, &empty_arena, &[]);
                typifier = self.const_typifier;
                expressions = &self.module.global_expressions;
            }
        };
        typifier
            .grow(handle, expressions, &resolve_ctx)
            .map_err(Error::InvalidResolve)?;

        Ok(self)
    }

    fn image_data(
        &mut self,
        image: Handle<crate::Expression>,
        span: Span,
    ) -> Result<'source, (crate::ImageClass, bool)> {
        match *resolve_inner!(self, image) {
            crate::TypeInner::Image { class, arrayed, .. } => Ok((class, arrayed)),
            _ => Err(Box::new(Error::BadTexture(span))),
        }
    }

    fn prepare_args<'b>(
        &mut self,
        args: &'b [Handle<ast::Expression<'source>>],
        min_args: u32,
        span: Span,
    ) -> ArgumentContext<'b, 'source> {
        ArgumentContext {
            args: args.iter(),
            min_args,
            args_used: 0,
            total_args: args.len() as u32,
            span,
        }
    }

    /// Insert splats, if needed by the non-'*' operations.
    ///
    /// See the "Binary arithmetic expressions with mixed scalar and vector operands"
    /// table in the WebGPU Shading Language specification for relevant operators.
    ///
    /// Multiply is not handled here as backends are expected to handle vec*scalar
    /// operations, so inserting splats into the IR increases size needlessly.
    fn binary_op_splat(
        &mut self,
        op: crate::BinaryOperator,
        left: &mut Handle<crate::Expression>,
        right: &mut Handle<crate::Expression>,
    ) -> Result<'source, ()> {
        if matches!(
            op,
            crate::BinaryOperator::Add
                | crate::BinaryOperator::Subtract
                | crate::BinaryOperator::Divide
                | crate::BinaryOperator::Modulo
        ) {
            match resolve_inner_binary!(self, *left, *right) {
                (&crate::TypeInner::Vector { size, .. }, &crate::TypeInner::Scalar { .. }) => {
                    *right = self.append_expression(
                        crate::Expression::Splat {
                            size,
                            value: *right,
                        },
                        self.get_expression_span(*right),
                    )?;
                }
                (&crate::TypeInner::Scalar { .. }, &crate::TypeInner::Vector { size, .. }) => {
                    *left = self.append_expression(
                        crate::Expression::Splat { size, value: *left },
                        self.get_expression_span(*left),
                    )?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Add a single expression to the expression table that is not covered by `self.emitter`.
    ///
    /// This is useful for `CallResult` and `AtomicResult` expressions, which should not be covered by
    /// `Emit` statements.
    fn interrupt_emitter(
        &mut self,
        expression: crate::Expression,
        span: Span,
    ) -> Result<'source, Handle<crate::Expression>> {
        match self.expr_type {
            ExpressionContextType::Runtime(ref mut rctx)
            | ExpressionContextType::Constant(Some(ref mut rctx)) => {
                rctx.block
                    .extend(rctx.emitter.finish(&rctx.function.expressions));
            }
            ExpressionContextType::Constant(None) | ExpressionContextType::Override => {}
        }
        let result = self.append_expression(expression, span);
        match self.expr_type {
            ExpressionContextType::Runtime(ref mut rctx)
            | ExpressionContextType::Constant(Some(ref mut rctx)) => {
                rctx.emitter.start(&rctx.function.expressions);
            }
            ExpressionContextType::Constant(None) | ExpressionContextType::Override => {}
        }
        result
    }

    /// Apply the WGSL Load Rule to `expr`.
    ///
    /// If `expr` is has type `ref<SC, T, A>`, perform a load to produce a value of type
    /// `T`. Otherwise, return `expr` unchanged.
    fn apply_load_rule(
        &mut self,
        expr: Typed<Handle<crate::Expression>>,
    ) -> Result<'source, Handle<crate::Expression>> {
        match expr {
            Typed::Reference(pointer) => {
                let load = crate::Expression::Load { pointer };
                let span = self.get_expression_span(pointer);
                self.append_expression(load, span)
            }
            Typed::Plain(handle) => Ok(handle),
        }
    }

    fn ensure_type_exists(&mut self, inner: crate::TypeInner) -> Handle<crate::Type> {
        self.as_global().ensure_type_exists(None, inner)
    }
}

struct ArgumentContext<'ctx, 'source> {
    args: core::slice::Iter<'ctx, Handle<ast::Expression<'source>>>,
    min_args: u32,
    args_used: u32,
    total_args: u32,
    span: Span,
}

impl<'source> ArgumentContext<'_, 'source> {
    pub fn finish(self) -> Result<'source, ()> {
        if self.args.len() == 0 {
            Ok(())
        } else {
            Err(Box::new(Error::WrongArgumentCount {
                found: self.total_args,
                expected: self.min_args..self.args_used + 1,
                span: self.span,
            }))
        }
    }

    pub fn next(&mut self) -> Result<'source, Handle<ast::Expression<'source>>> {
        match self.args.next().copied() {
            Some(arg) => {
                self.args_used += 1;
                Ok(arg)
            }
            None => Err(Box::new(Error::WrongArgumentCount {
                found: self.total_args,
                expected: self.min_args..self.args_used + 1,
                span: self.span,
            })),
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum Declared<T> {
    /// Value declared as const
    Const(T),

    /// Value declared as non-const
    Runtime(T),
}

impl<T> Declared<T> {
    fn runtime(self) -> T {
        match self {
            Declared::Const(t) | Declared::Runtime(t) => t,
        }
    }

    fn const_time(self) -> Option<T> {
        match self {
            Declared::Const(t) => Some(t),
            Declared::Runtime(_) => None,
        }
    }
}

/// WGSL type annotations on expressions, types, values, etc.
///
/// Naga and WGSL types are very close, but Naga lacks WGSL's `ref` types, which
/// we need to know to apply the Load Rule. This enum carries some WGSL or Naga
/// datum along with enough information to determine its corresponding WGSL
/// type.
///
/// The `T` type parameter can be any expression-like thing:
///
/// - `Typed<Handle<crate::Type>>` can represent a full WGSL type. For example,
///   given some Naga `Pointer` type `ptr`, a WGSL reference type is a
///   `Typed::Reference(ptr)` whereas a WGSL pointer type is a
///   `Typed::Plain(ptr)`.
///
/// - `Typed<crate::Expression>` or `Typed<Handle<crate::Expression>>` can
///   represent references similarly.
///
/// Use the `map` and `try_map` methods to convert from one expression
/// representation to another.
///
/// [`Expression`]: crate::Expression
#[derive(Debug, Copy, Clone)]
enum Typed<T> {
    /// A WGSL reference.
    Reference(T),

    /// A WGSL plain type.
    Plain(T),
}

impl<T> Typed<T> {
    fn map<U>(self, mut f: impl FnMut(T) -> U) -> Typed<U> {
        match self {
            Self::Reference(v) => Typed::Reference(f(v)),
            Self::Plain(v) => Typed::Plain(f(v)),
        }
    }

    fn try_map<U, E>(
        self,
        mut f: impl FnMut(T) -> core::result::Result<U, E>,
    ) -> core::result::Result<Typed<U>, E> {
        Ok(match self {
            Self::Reference(expr) => Typed::Reference(f(expr)?),
            Self::Plain(expr) => Typed::Plain(f(expr)?),
        })
    }
}

/// A single vector component or swizzle.
///
/// This represents the things that can appear after the `.` in a vector access
/// expression: either a single component name, or a series of them,
/// representing a swizzle.
enum Components {
    Single(u32),
    Swizzle {
        size: crate::VectorSize,
        pattern: [crate::SwizzleComponent; 4],
    },
}

impl Components {
    const fn letter_component(letter: char) -> Option<crate::SwizzleComponent> {
        use crate::SwizzleComponent as Sc;
        match letter {
            'x' | 'r' => Some(Sc::X),
            'y' | 'g' => Some(Sc::Y),
            'z' | 'b' => Some(Sc::Z),
            'w' | 'a' => Some(Sc::W),
            _ => None,
        }
    }

    fn single_component(name: &str, name_span: Span) -> Result<u32> {
        let ch = name.chars().next().ok_or(Error::BadAccessor(name_span))?;
        match Self::letter_component(ch) {
            Some(sc) => Ok(sc as u32),
            None => Err(Box::new(Error::BadAccessor(name_span))),
        }
    }

    /// Construct a `Components` value from a 'member' name, like `"wzy"` or `"x"`.
    ///
    /// Use `name_span` for reporting errors in parsing the component string.
    fn new(name: &str, name_span: Span) -> Result<Self> {
        let size = match name.len() {
            1 => return Ok(Components::Single(Self::single_component(name, name_span)?)),
            2 => crate::VectorSize::Bi,
            3 => crate::VectorSize::Tri,
            4 => crate::VectorSize::Quad,
            _ => return Err(Box::new(Error::BadAccessor(name_span))),
        };

        let mut pattern = [crate::SwizzleComponent::X; 4];
        for (comp, ch) in pattern.iter_mut().zip(name.chars()) {
            *comp = Self::letter_component(ch).ok_or(Error::BadAccessor(name_span))?;
        }

        if name.chars().all(|c| matches!(c, 'x' | 'y' | 'z' | 'w'))
            || name.chars().all(|c| matches!(c, 'r' | 'g' | 'b' | 'a'))
        {
            Ok(Components::Swizzle { size, pattern })
        } else {
            Err(Box::new(Error::BadAccessor(name_span)))
        }
    }
}

/// An `ast::GlobalDecl` for which we have built the Naga IR equivalent.
enum LoweredGlobalDecl {
    Function {
        handle: Handle<crate::Function>,
        must_use: bool,
    },
    Var(Handle<crate::GlobalVariable>),
    Const(Handle<crate::Constant>),
    Override(Handle<crate::Override>),
    Type(Handle<crate::Type>),
    EntryPoint,
}

enum Texture {
    Gather,
    GatherCompare,

    Sample,
    SampleBias,
    SampleCompare,
    SampleCompareLevel,
    SampleGrad,
    SampleLevel,
    // SampleBaseClampToEdge,
}

impl Texture {
    pub fn map(word: &str) -> Option<Self> {
        Some(match word {
            "textureGather" => Self::Gather,
            "textureGatherCompare" => Self::GatherCompare,

            "textureSample" => Self::Sample,
            "textureSampleBias" => Self::SampleBias,
            "textureSampleCompare" => Self::SampleCompare,
            "textureSampleCompareLevel" => Self::SampleCompareLevel,
            "textureSampleGrad" => Self::SampleGrad,
            "textureSampleLevel" => Self::SampleLevel,
            // "textureSampleBaseClampToEdge" => Some(Self::SampleBaseClampToEdge),
            _ => return None,
        })
    }

    pub const fn min_argument_count(&self) -> u32 {
        match *self {
            Self::Gather => 3,
            Self::GatherCompare => 4,

            Self::Sample => 3,
            Self::SampleBias => 5,
            Self::SampleCompare => 5,
            Self::SampleCompareLevel => 5,
            Self::SampleGrad => 6,
            Self::SampleLevel => 5,
            // Self::SampleBaseClampToEdge => 3,
        }
    }
}

enum SubgroupGather {
    BroadcastFirst,
    Broadcast,
    Shuffle,
    ShuffleDown,
    ShuffleUp,
    ShuffleXor,
}

impl SubgroupGather {
    pub fn map(word: &str) -> Option<Self> {
        Some(match word {
            "subgroupBroadcastFirst" => Self::BroadcastFirst,
            "subgroupBroadcast" => Self::Broadcast,
            "subgroupShuffle" => Self::Shuffle,
            "subgroupShuffleDown" => Self::ShuffleDown,
            "subgroupShuffleUp" => Self::ShuffleUp,
            "subgroupShuffleXor" => Self::ShuffleXor,
            _ => return None,
        })
    }
}

/// Whether a declaration accepts abstract types, or concretizes.
enum AbstractRule {
    /// This declaration concretizes its initialization expression.
    Concretize,

    /// This declaration can accept initializers with abstract types.
    Allow,
}

pub struct Lowerer<'source, 'temp> {
    index: &'temp Index<'source>,
}

impl<'source, 'temp> Lowerer<'source, 'temp> {
    pub const fn new(index: &'temp Index<'source>) -> Self {
        Self { index }
    }

    pub fn lower(&mut self, tu: ast::TranslationUnit<'source>) -> Result<'source, crate::Module> {
        let mut module = crate::Module {
            diagnostic_filters: tu.diagnostic_filters,
            diagnostic_filter_leaf: tu.diagnostic_filter_leaf,
            ..Default::default()
        };

        let mut ctx = GlobalContext {
            ast_expressions: &tu.expressions,
            globals: &mut FastHashMap::default(),
            types: &tu.types,
            module: &mut module,
            const_typifier: &mut Typifier::new(),
            layouter: &mut Layouter::default(),
            global_expression_kind_tracker: &mut crate::proc::ExpressionKindTracker::new(),
        };

        for decl_handle in self.index.visit_ordered() {
            let span = tu.decls.get_span(decl_handle);
            let decl = &tu.decls[decl_handle];

            match decl.kind {
                ast::GlobalDeclKind::Fn(ref f) => {
                    let lowered_decl = self.function(f, span, &mut ctx)?;
                    ctx.globals.insert(f.name.name, lowered_decl);
                }
                ast::GlobalDeclKind::Var(ref v) => {
                    let explicit_ty =
                        v.ty.map(|ast| self.resolve_ast_type(ast, &mut ctx.as_const()))
                            .transpose()?;

                    let (ty, initializer) = self.type_and_init(
                        v.name,
                        v.init,
                        explicit_ty,
                        AbstractRule::Concretize,
                        &mut ctx.as_override(),
                    )?;

                    let binding = if let Some(ref binding) = v.binding {
                        Some(crate::ResourceBinding {
                            group: self.const_u32(binding.group, &mut ctx.as_const())?.0,
                            binding: self.const_u32(binding.binding, &mut ctx.as_const())?.0,
                        })
                    } else {
                        None
                    };

                    let handle = ctx.module.global_variables.append(
                        crate::GlobalVariable {
                            name: Some(v.name.name.to_string()),
                            space: v.space,
                            binding,
                            ty,
                            init: initializer,
                        },
                        span,
                    );

                    ctx.globals
                        .insert(v.name.name, LoweredGlobalDecl::Var(handle));
                }
                ast::GlobalDeclKind::Const(ref c) => {
                    let mut ectx = ctx.as_const();

                    let explicit_ty =
                        c.ty.map(|ast| self.resolve_ast_type(ast, &mut ectx))
                            .transpose()?;

                    let (ty, init) = self.type_and_init(
                        c.name,
                        Some(c.init),
                        explicit_ty,
                        AbstractRule::Allow,
                        &mut ectx,
                    )?;
                    let init = init.expect("Global const must have init");

                    let handle = ctx.module.constants.append(
                        crate::Constant {
                            name: Some(c.name.name.to_string()),
                            ty,
                            init,
                        },
                        span,
                    );

                    ctx.globals
                        .insert(c.name.name, LoweredGlobalDecl::Const(handle));
                }
                ast::GlobalDeclKind::Override(ref o) => {
                    let explicit_ty =
                        o.ty.map(|ast| self.resolve_ast_type(ast, &mut ctx.as_const()))
                            .transpose()?;

                    let mut ectx = ctx.as_override();

                    let (ty, init) = self.type_and_init(
                        o.name,
                        o.init,
                        explicit_ty,
                        AbstractRule::Concretize,
                        &mut ectx,
                    )?;

                    let id =
                        o.id.map(|id| self.const_u32(id, &mut ctx.as_const()))
                            .transpose()?;

                    let id = if let Some((id, id_span)) = id {
                        Some(
                            u16::try_from(id)
                                .map_err(|_| Error::PipelineConstantIDValue(id_span))?,
                        )
                    } else {
                        None
                    };

                    let handle = ctx.module.overrides.append(
                        crate::Override {
                            name: Some(o.name.name.to_string()),
                            id,
                            ty,
                            init,
                        },
                        span,
                    );

                    ctx.globals
                        .insert(o.name.name, LoweredGlobalDecl::Override(handle));
                }
                ast::GlobalDeclKind::Struct(ref s) => {
                    let handle = self.r#struct(s, span, &mut ctx)?;
                    ctx.globals
                        .insert(s.name.name, LoweredGlobalDecl::Type(handle));
                }
                ast::GlobalDeclKind::Type(ref alias) => {
                    let ty = self.resolve_named_ast_type(
                        alias.ty,
                        Some(alias.name.name.to_string()),
                        &mut ctx.as_const(),
                    )?;
                    ctx.globals
                        .insert(alias.name.name, LoweredGlobalDecl::Type(ty));
                }
                ast::GlobalDeclKind::ConstAssert(condition) => {
                    let condition = self.expression(condition, &mut ctx.as_const())?;

                    let span = ctx.module.global_expressions.get_span(condition);
                    match ctx
                        .module
                        .to_ctx()
                        .eval_expr_to_bool_from(condition, &ctx.module.global_expressions)
                    {
                        Some(true) => Ok(()),
                        Some(false) => Err(Error::ConstAssertFailed(span)),
                        _ => Err(Error::NotBool(span)),
                    }?;
                }
            }
        }

        // Constant evaluation may leave abstract-typed literals and
        // compositions in expression arenas, so we need to compact the module
        // to remove unused expressions and types.
        crate::compact::compact(&mut module);

        Ok(module)
    }

    /// Obtain (inferred) type and initializer after automatic conversion
    fn type_and_init(
        &mut self,
        name: ast::Ident<'source>,
        init: Option<Handle<ast::Expression<'source>>>,
        explicit_ty: Option<Handle<crate::Type>>,
        abstract_rule: AbstractRule,
        ectx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, (Handle<crate::Type>, Option<Handle<crate::Expression>>)> {
        let ty;
        let initializer;
        match (init, explicit_ty) {
            (Some(init), Some(explicit_ty)) => {
                let init = self.expression_for_abstract(init, ectx)?;
                let ty_res = crate::proc::TypeResolution::Handle(explicit_ty);
                let init = ectx
                    .try_automatic_conversions(init, &ty_res, name.span)
                    .map_err(|error| match *error {
                        Error::AutoConversion(e) => Box::new(Error::InitializationTypeMismatch {
                            name: name.span,
                            expected: e.dest_type,
                            got: e.source_type,
                        }),
                        _ => error,
                    })?;

                let init_ty = ectx.register_type(init)?;
                let explicit_inner = &ectx.module.types[explicit_ty].inner;
                let init_inner = &ectx.module.types[init_ty].inner;
                if !explicit_inner.equivalent(init_inner, &ectx.module.types) {
                    return Err(Box::new(Error::InitializationTypeMismatch {
                        name: name.span,
                        expected: ectx.type_inner_to_string(explicit_inner),
                        got: ectx.type_inner_to_string(init_inner),
                    }));
                }
                ty = explicit_ty;
                initializer = Some(init);
            }
            (Some(init), None) => {
                let mut init = self.expression_for_abstract(init, ectx)?;
                if let AbstractRule::Concretize = abstract_rule {
                    init = ectx.concretize(init)?;
                }
                ty = ectx.register_type(init)?;
                initializer = Some(init);
            }
            (None, Some(explicit_ty)) => {
                ty = explicit_ty;
                initializer = None;
            }
            (None, None) => return Err(Box::new(Error::DeclMissingTypeAndInit(name.span))),
        }
        Ok((ty, initializer))
    }

    fn function(
        &mut self,
        f: &ast::Function<'source>,
        span: Span,
        ctx: &mut GlobalContext<'source, '_, '_>,
    ) -> Result<'source, LoweredGlobalDecl> {
        let mut local_table = FastHashMap::default();
        let mut expressions = Arena::new();
        let mut named_expressions = FastIndexMap::default();
        let mut local_expression_kind_tracker = crate::proc::ExpressionKindTracker::new();

        let arguments = f
            .arguments
            .iter()
            .enumerate()
            .map(|(i, arg)| -> Result<'_, _> {
                let ty = self.resolve_ast_type(arg.ty, &mut ctx.as_const())?;
                let expr = expressions
                    .append(crate::Expression::FunctionArgument(i as u32), arg.name.span);
                local_table.insert(arg.handle, Declared::Runtime(Typed::Plain(expr)));
                named_expressions.insert(expr, (arg.name.name.to_string(), arg.name.span));
                local_expression_kind_tracker.insert(expr, crate::proc::ExpressionKind::Runtime);

                Ok(crate::FunctionArgument {
                    name: Some(arg.name.name.to_string()),
                    ty,
                    binding: self.binding(&arg.binding, ty, ctx)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let result = f
            .result
            .as_ref()
            .map(|res| -> Result<'_, _> {
                let ty = self.resolve_ast_type(res.ty, &mut ctx.as_const())?;
                Ok(crate::FunctionResult {
                    ty,
                    binding: self.binding(&res.binding, ty, ctx)?,
                })
            })
            .transpose()?;

        let mut function = crate::Function {
            name: Some(f.name.name.to_string()),
            arguments,
            result,
            local_variables: Arena::new(),
            expressions,
            named_expressions: crate::NamedExpressions::default(),
            body: crate::Block::default(),
            diagnostic_filter_leaf: f.diagnostic_filter_leaf,
        };

        let mut typifier = Typifier::default();
        let mut stmt_ctx = StatementContext {
            local_table: &mut local_table,
            globals: ctx.globals,
            ast_expressions: ctx.ast_expressions,
            const_typifier: ctx.const_typifier,
            typifier: &mut typifier,
            layouter: ctx.layouter,
            function: &mut function,
            named_expressions: &mut named_expressions,
            types: ctx.types,
            module: ctx.module,
            local_expression_kind_tracker: &mut local_expression_kind_tracker,
            global_expression_kind_tracker: ctx.global_expression_kind_tracker,
        };
        let mut body = self.block(&f.body, false, &mut stmt_ctx)?;
        ensure_block_returns(&mut body);

        function.body = body;
        function.named_expressions = named_expressions
            .into_iter()
            .map(|(key, (name, _))| (key, name))
            .collect();

        if let Some(ref entry) = f.entry_point {
            let workgroup_size_info = if let Some(workgroup_size) = entry.workgroup_size {
                // TODO: replace with try_map once stabilized
                let mut workgroup_size_out = [1; 3];
                let mut workgroup_size_overrides_out = [None; 3];
                for (i, size) in workgroup_size.into_iter().enumerate() {
                    if let Some(size_expr) = size {
                        match self.const_u32(size_expr, &mut ctx.as_const()) {
                            Ok(value) => {
                                workgroup_size_out[i] = value.0;
                            }
                            Err(err) => {
                                if let Error::ConstantEvaluatorError(ref ty, _) = *err {
                                    match **ty {
                                        crate::proc::ConstantEvaluatorError::OverrideExpr => {
                                            workgroup_size_overrides_out[i] =
                                                Some(self.workgroup_size_override(
                                                    size_expr,
                                                    &mut ctx.as_override(),
                                                )?);
                                        }
                                        _ => {
                                            return Err(err);
                                        }
                                    }
                                } else {
                                    return Err(err);
                                }
                            }
                        }
                    }
                }
                if workgroup_size_overrides_out.iter().all(|x| x.is_none()) {
                    (workgroup_size_out, None)
                } else {
                    (workgroup_size_out, Some(workgroup_size_overrides_out))
                }
            } else {
                ([0; 3], None)
            };

            let (workgroup_size, workgroup_size_overrides) = workgroup_size_info;
            ctx.module.entry_points.push(crate::EntryPoint {
                name: f.name.name.to_string(),
                stage: entry.stage,
                early_depth_test: entry.early_depth_test,
                workgroup_size,
                workgroup_size_overrides,
                function,
            });
            Ok(LoweredGlobalDecl::EntryPoint)
        } else {
            let handle = ctx.module.functions.append(function, span);
            Ok(LoweredGlobalDecl::Function {
                handle,
                must_use: f.result.as_ref().is_some_and(|res| res.must_use),
            })
        }
    }

    fn workgroup_size_override(
        &mut self,
        size_expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let span = ctx.ast_expressions.get_span(size_expr);
        let expr = self.expression(size_expr, ctx)?;
        match resolve_inner!(ctx, expr).scalar_kind().ok_or(0) {
            Ok(crate::ScalarKind::Sint) | Ok(crate::ScalarKind::Uint) => Ok(expr),
            _ => Err(Box::new(Error::ExpectedConstExprConcreteIntegerScalar(
                span,
            ))),
        }
    }

    fn block(
        &mut self,
        b: &ast::Block<'source>,
        is_inside_loop: bool,
        ctx: &mut StatementContext<'source, '_, '_>,
    ) -> Result<'source, crate::Block> {
        let mut block = crate::Block::default();

        for stmt in b.stmts.iter() {
            self.statement(stmt, &mut block, is_inside_loop, ctx)?;
        }

        Ok(block)
    }

    fn statement(
        &mut self,
        stmt: &ast::Statement<'source>,
        block: &mut crate::Block,
        is_inside_loop: bool,
        ctx: &mut StatementContext<'source, '_, '_>,
    ) -> Result<'source, ()> {
        let out = match stmt.kind {
            ast::StatementKind::Block(ref block) => {
                let block = self.block(block, is_inside_loop, ctx)?;
                crate::Statement::Block(block)
            }
            ast::StatementKind::LocalDecl(ref decl) => match *decl {
                ast::LocalDecl::Let(ref l) => {
                    let mut emitter = Emitter::default();
                    emitter.start(&ctx.function.expressions);

                    let value =
                        self.expression(l.init, &mut ctx.as_expression(block, &mut emitter))?;

                    // The WGSL spec says that any expression that refers to a
                    // `let`-bound variable is not a const expression. This
                    // affects when errors must be reported, so we can't even
                    // treat suitable `let` bindings as constant as an
                    // optimization.
                    ctx.local_expression_kind_tracker.force_non_const(value);

                    let explicit_ty = l
                        .ty
                        .map(|ty| self.resolve_ast_type(ty, &mut ctx.as_const(block, &mut emitter)))
                        .transpose()?;

                    if let Some(ty) = explicit_ty {
                        let mut ctx = ctx.as_expression(block, &mut emitter);
                        let init_ty = ctx.register_type(value)?;
                        if !ctx.module.types[ty]
                            .inner
                            .equivalent(&ctx.module.types[init_ty].inner, &ctx.module.types)
                        {
                            return Err(Box::new(Error::InitializationTypeMismatch {
                                name: l.name.span,
                                expected: ctx.type_to_string(ty),
                                got: ctx.type_to_string(init_ty),
                            }));
                        }
                    }

                    block.extend(emitter.finish(&ctx.function.expressions));
                    ctx.local_table
                        .insert(l.handle, Declared::Runtime(Typed::Plain(value)));
                    ctx.named_expressions
                        .insert(value, (l.name.name.to_string(), l.name.span));

                    return Ok(());
                }
                ast::LocalDecl::Var(ref v) => {
                    let mut emitter = Emitter::default();
                    emitter.start(&ctx.function.expressions);

                    let explicit_ty =
                        v.ty.map(|ast| {
                            self.resolve_ast_type(ast, &mut ctx.as_const(block, &mut emitter))
                        })
                        .transpose()?;

                    let mut ectx = ctx.as_expression(block, &mut emitter);
                    let (ty, initializer) = self.type_and_init(
                        v.name,
                        v.init,
                        explicit_ty,
                        AbstractRule::Concretize,
                        &mut ectx,
                    )?;

                    let (const_initializer, initializer) = {
                        match initializer {
                            Some(init) => {
                                // It's not correct to hoist the initializer up
                                // to the top of the function if:
                                // - the initialization is inside a loop, and should
                                //   take place on every iteration, or
                                // - the initialization is not a constant
                                //   expression, so its value depends on the
                                //   state at the point of initialization.
                                if is_inside_loop
                                    || !ctx.local_expression_kind_tracker.is_const_or_override(init)
                                {
                                    (None, Some(init))
                                } else {
                                    (Some(init), None)
                                }
                            }
                            None => (None, None),
                        }
                    };

                    let var = ctx.function.local_variables.append(
                        crate::LocalVariable {
                            name: Some(v.name.name.to_string()),
                            ty,
                            init: const_initializer,
                        },
                        stmt.span,
                    );

                    let handle = ctx.as_expression(block, &mut emitter).interrupt_emitter(
                        crate::Expression::LocalVariable(var),
                        Span::UNDEFINED,
                    )?;
                    block.extend(emitter.finish(&ctx.function.expressions));
                    ctx.local_table
                        .insert(v.handle, Declared::Runtime(Typed::Reference(handle)));

                    match initializer {
                        Some(initializer) => crate::Statement::Store {
                            pointer: handle,
                            value: initializer,
                        },
                        None => return Ok(()),
                    }
                }
                ast::LocalDecl::Const(ref c) => {
                    let mut emitter = Emitter::default();
                    emitter.start(&ctx.function.expressions);

                    let ectx = &mut ctx.as_const(block, &mut emitter);

                    let explicit_ty =
                        c.ty.map(|ast| self.resolve_ast_type(ast, &mut ectx.as_const()))
                            .transpose()?;

                    let (ty, init) = self.type_and_init(
                        c.name,
                        Some(c.init),
                        explicit_ty,
                        AbstractRule::Allow,
                        &mut ectx.as_const(),
                    )?;
                    let init = init.expect("Local const must have init");

                    block.extend(emitter.finish(&ctx.function.expressions));
                    ctx.local_table
                        .insert(c.handle, Declared::Const(Typed::Plain(init)));
                    // Only add constants of non-abstract types to the named expressions
                    // to prevent abstract types ending up in the IR.
                    let is_abstract = ctx.module.types[ty].inner.is_abstract(&ctx.module.types);
                    if !is_abstract {
                        ctx.named_expressions
                            .insert(init, (c.name.name.to_string(), c.name.span));
                    }
                    return Ok(());
                }
            },
            ast::StatementKind::If {
                condition,
                ref accept,
                ref reject,
            } => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let condition =
                    self.expression(condition, &mut ctx.as_expression(block, &mut emitter))?;
                block.extend(emitter.finish(&ctx.function.expressions));

                let accept = self.block(accept, is_inside_loop, ctx)?;
                let reject = self.block(reject, is_inside_loop, ctx)?;

                crate::Statement::If {
                    condition,
                    accept,
                    reject,
                }
            }
            ast::StatementKind::Switch {
                selector,
                ref cases,
            } => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let mut ectx = ctx.as_expression(block, &mut emitter);

                // Determine the scalar type of the selector and case expressions, find the
                // consensus type for automatic conversion, then convert them.
                let (mut exprs, spans) = core::iter::once(selector)
                    .chain(cases.iter().filter_map(|case| match case.value {
                        ast::SwitchValue::Expr(expr) => Some(expr),
                        ast::SwitchValue::Default => None,
                    }))
                    .enumerate()
                    .map(|(i, expr)| {
                        let span = ectx.ast_expressions.get_span(expr);
                        let expr = self.expression_for_abstract(expr, &mut ectx)?;
                        let ty = resolve_inner!(ectx, expr);
                        match *ty {
                            crate::TypeInner::Scalar(
                                crate::Scalar::I32
                                | crate::Scalar::U32
                                | crate::Scalar::ABSTRACT_INT,
                            ) => Ok((expr, span)),
                            _ => match i {
                                0 => Err(Box::new(Error::InvalidSwitchSelector { span })),
                                _ => Err(Box::new(Error::InvalidSwitchCase { span })),
                            },
                        }
                    })
                    .collect::<Result<(Vec<_>, Vec<_>)>>()?;

                let mut consensus =
                    ectx.automatic_conversion_consensus(&exprs)
                        .map_err(|span_idx| Error::SwitchCaseTypeMismatch {
                            span: spans[span_idx],
                        })?;
                // Concretize to I32 if the selector and all cases were abstract
                if consensus == crate::Scalar::ABSTRACT_INT {
                    consensus = crate::Scalar::I32;
                }
                for expr in &mut exprs {
                    ectx.convert_to_leaf_scalar(expr, consensus)?;
                }

                block.extend(emitter.finish(&ctx.function.expressions));

                let mut exprs = exprs.into_iter();
                let selector = exprs
                    .next()
                    .expect("First element should be selector expression");

                let cases = cases
                    .iter()
                    .map(|case| {
                        Ok(crate::SwitchCase {
                            value: match case.value {
                                ast::SwitchValue::Expr(expr) => {
                                    let span = ctx.ast_expressions.get_span(expr);
                                    let expr = exprs.next().expect(
                                        "Should yield expression for each SwitchValue::Expr case",
                                    );
                                    match ctx
                                        .module
                                        .to_ctx()
                                        .eval_expr_to_literal_from(expr, &ctx.function.expressions)
                                    {
                                        Some(crate::Literal::I32(value)) => {
                                            crate::SwitchValue::I32(value)
                                        }
                                        Some(crate::Literal::U32(value)) => {
                                            crate::SwitchValue::U32(value)
                                        }
                                        _ => {
                                            return Err(Box::new(Error::InvalidSwitchCase {
                                                span,
                                            }));
                                        }
                                    }
                                }
                                ast::SwitchValue::Default => crate::SwitchValue::Default,
                            },
                            body: self.block(&case.body, is_inside_loop, ctx)?,
                            fall_through: case.fall_through,
                        })
                    })
                    .collect::<Result<_>>()?;

                crate::Statement::Switch { selector, cases }
            }
            ast::StatementKind::Loop {
                ref body,
                ref continuing,
                break_if,
            } => {
                let body = self.block(body, true, ctx)?;
                let mut continuing = self.block(continuing, true, ctx)?;

                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);
                let break_if = break_if
                    .map(|expr| {
                        self.expression(expr, &mut ctx.as_expression(&mut continuing, &mut emitter))
                    })
                    .transpose()?;
                continuing.extend(emitter.finish(&ctx.function.expressions));

                crate::Statement::Loop {
                    body,
                    continuing,
                    break_if,
                }
            }
            ast::StatementKind::Break => crate::Statement::Break,
            ast::StatementKind::Continue => crate::Statement::Continue,
            ast::StatementKind::Return { value: ast_value } => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let value;
                if let Some(ast_expr) = ast_value {
                    let result_ty = ctx.function.result.as_ref().map(|r| r.ty);
                    let mut ectx = ctx.as_expression(block, &mut emitter);
                    let expr = self.expression_for_abstract(ast_expr, &mut ectx)?;

                    if let Some(result_ty) = result_ty {
                        let mut ectx = ctx.as_expression(block, &mut emitter);
                        let resolution = crate::proc::TypeResolution::Handle(result_ty);
                        let converted =
                            ectx.try_automatic_conversions(expr, &resolution, Span::default())?;
                        value = Some(converted);
                    } else {
                        value = Some(expr);
                    }
                } else {
                    value = None;
                }
                block.extend(emitter.finish(&ctx.function.expressions));

                crate::Statement::Return { value }
            }
            ast::StatementKind::Kill => crate::Statement::Kill,
            ast::StatementKind::Call {
                ref function,
                ref arguments,
            } => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let _ = self.call(
                    stmt.span,
                    function,
                    arguments,
                    &mut ctx.as_expression(block, &mut emitter),
                    true,
                )?;
                block.extend(emitter.finish(&ctx.function.expressions));
                return Ok(());
            }
            ast::StatementKind::Assign {
                target: ast_target,
                op,
                value,
            } => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);
                let target_span = ctx.ast_expressions.get_span(ast_target);

                let mut ectx = ctx.as_expression(block, &mut emitter);
                let target = self.expression_for_reference(ast_target, &mut ectx)?;
                let target_handle = match target {
                    Typed::Reference(handle) => handle,
                    Typed::Plain(handle) => {
                        let ty = ctx.invalid_assignment_type(handle);
                        return Err(Box::new(Error::InvalidAssignment {
                            span: target_span,
                            ty,
                        }));
                    }
                };

                // Usually the value needs to be converted to match the type of
                // the memory view you're assigning it to. The bit shift
                // operators are exceptions, in that the right operand is always
                // a `u32` or `vecN<u32>`.
                let target_scalar = match op {
                    Some(crate::BinaryOperator::ShiftLeft | crate::BinaryOperator::ShiftRight) => {
                        Some(crate::Scalar::U32)
                    }
                    _ => resolve_inner!(ectx, target_handle)
                        .pointer_automatically_convertible_scalar(&ectx.module.types),
                };

                let value = self.expression_for_abstract(value, &mut ectx)?;
                let mut value = match target_scalar {
                    Some(target_scalar) => ectx.try_automatic_conversion_for_leaf_scalar(
                        value,
                        target_scalar,
                        target_span,
                    )?,
                    None => value,
                };

                let value = match op {
                    Some(op) => {
                        let mut left = ectx.apply_load_rule(target)?;
                        ectx.binary_op_splat(op, &mut left, &mut value)?;
                        ectx.append_expression(
                            crate::Expression::Binary {
                                op,
                                left,
                                right: value,
                            },
                            stmt.span,
                        )?
                    }
                    None => value,
                };
                block.extend(emitter.finish(&ctx.function.expressions));

                crate::Statement::Store {
                    pointer: target_handle,
                    value,
                }
            }
            ast::StatementKind::Increment(value) | ast::StatementKind::Decrement(value) => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let op = match stmt.kind {
                    ast::StatementKind::Increment(_) => crate::BinaryOperator::Add,
                    ast::StatementKind::Decrement(_) => crate::BinaryOperator::Subtract,
                    _ => unreachable!(),
                };

                let value_span = ctx.ast_expressions.get_span(value);
                let target = self
                    .expression_for_reference(value, &mut ctx.as_expression(block, &mut emitter))?;
                let target_handle = match target {
                    Typed::Reference(handle) => handle,
                    Typed::Plain(_) => {
                        return Err(Box::new(Error::BadIncrDecrReferenceType(value_span)))
                    }
                };

                let mut ectx = ctx.as_expression(block, &mut emitter);
                let scalar = match *resolve_inner!(ectx, target_handle) {
                    crate::TypeInner::ValuePointer {
                        size: None, scalar, ..
                    } => scalar,
                    crate::TypeInner::Pointer { base, .. } => match ectx.module.types[base].inner {
                        crate::TypeInner::Scalar(scalar) => scalar,
                        _ => return Err(Box::new(Error::BadIncrDecrReferenceType(value_span))),
                    },
                    _ => return Err(Box::new(Error::BadIncrDecrReferenceType(value_span))),
                };
                let literal = match scalar.kind {
                    crate::ScalarKind::Sint | crate::ScalarKind::Uint => {
                        crate::Literal::one(scalar)
                            .ok_or(Error::BadIncrDecrReferenceType(value_span))?
                    }
                    _ => return Err(Box::new(Error::BadIncrDecrReferenceType(value_span))),
                };

                let right =
                    ectx.interrupt_emitter(crate::Expression::Literal(literal), Span::UNDEFINED)?;
                let rctx = ectx.runtime_expression_ctx(stmt.span)?;
                let left = rctx.function.expressions.append(
                    crate::Expression::Load {
                        pointer: target_handle,
                    },
                    value_span,
                );
                let value = rctx
                    .function
                    .expressions
                    .append(crate::Expression::Binary { op, left, right }, stmt.span);
                rctx.local_expression_kind_tracker
                    .insert(left, crate::proc::ExpressionKind::Runtime);
                rctx.local_expression_kind_tracker
                    .insert(value, crate::proc::ExpressionKind::Runtime);

                block.extend(emitter.finish(&ctx.function.expressions));
                crate::Statement::Store {
                    pointer: target_handle,
                    value,
                }
            }
            ast::StatementKind::ConstAssert(condition) => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let condition =
                    self.expression(condition, &mut ctx.as_const(block, &mut emitter))?;

                let span = ctx.function.expressions.get_span(condition);
                match ctx
                    .module
                    .to_ctx()
                    .eval_expr_to_bool_from(condition, &ctx.function.expressions)
                {
                    Some(true) => Ok(()),
                    Some(false) => Err(Error::ConstAssertFailed(span)),
                    _ => Err(Error::NotBool(span)),
                }?;

                block.extend(emitter.finish(&ctx.function.expressions));

                return Ok(());
            }
            ast::StatementKind::Phony(expr) => {
                let mut emitter = Emitter::default();
                emitter.start(&ctx.function.expressions);

                let value = self.expression(expr, &mut ctx.as_expression(block, &mut emitter))?;
                block.extend(emitter.finish(&ctx.function.expressions));
                ctx.named_expressions
                    .insert(value, ("phony".to_string(), stmt.span));
                return Ok(());
            }
        };

        block.push(out, stmt.span);

        Ok(())
    }

    /// Lower `expr` and apply the Load Rule if possible.
    ///
    /// For the time being, this concretizes abstract values, to support
    /// consumers that haven't been adapted to consume them yet. Consumers
    /// prepared for abstract values can call [`expression_for_abstract`].
    ///
    /// [`expression_for_abstract`]: Lowerer::expression_for_abstract
    fn expression(
        &mut self,
        expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let expr = self.expression_for_abstract(expr, ctx)?;
        ctx.concretize(expr)
    }

    fn expression_for_abstract(
        &mut self,
        expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let expr = self.expression_for_reference(expr, ctx)?;
        ctx.apply_load_rule(expr)
    }

    fn expression_for_reference(
        &mut self,
        expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Typed<Handle<crate::Expression>>> {
        let span = ctx.ast_expressions.get_span(expr);
        let expr = &ctx.ast_expressions[expr];

        let expr: Typed<crate::Expression> = match *expr {
            ast::Expression::Literal(literal) => {
                let literal = match literal {
                    ast::Literal::Number(Number::F16(f)) => crate::Literal::F16(f),
                    ast::Literal::Number(Number::F32(f)) => crate::Literal::F32(f),
                    ast::Literal::Number(Number::I32(i)) => crate::Literal::I32(i),
                    ast::Literal::Number(Number::U32(u)) => crate::Literal::U32(u),
                    ast::Literal::Number(Number::I64(i)) => crate::Literal::I64(i),
                    ast::Literal::Number(Number::U64(u)) => crate::Literal::U64(u),
                    ast::Literal::Number(Number::F64(f)) => crate::Literal::F64(f),
                    ast::Literal::Number(Number::AbstractInt(i)) => crate::Literal::AbstractInt(i),
                    ast::Literal::Number(Number::AbstractFloat(f)) => {
                        crate::Literal::AbstractFloat(f)
                    }
                    ast::Literal::Bool(b) => crate::Literal::Bool(b),
                };
                let handle = ctx.interrupt_emitter(crate::Expression::Literal(literal), span)?;
                return Ok(Typed::Plain(handle));
            }
            ast::Expression::Ident(ast::IdentExpr::Local(local)) => {
                return ctx.local(&local, span);
            }
            ast::Expression::Ident(ast::IdentExpr::Unresolved(name)) => {
                let global = ctx
                    .globals
                    .get(name)
                    .ok_or(Error::UnknownIdent(span, name))?;
                let expr = match *global {
                    LoweredGlobalDecl::Var(handle) => {
                        let expr = crate::Expression::GlobalVariable(handle);
                        match ctx.module.global_variables[handle].space {
                            crate::AddressSpace::Handle => Typed::Plain(expr),
                            _ => Typed::Reference(expr),
                        }
                    }
                    LoweredGlobalDecl::Const(handle) => {
                        Typed::Plain(crate::Expression::Constant(handle))
                    }
                    LoweredGlobalDecl::Override(handle) => {
                        Typed::Plain(crate::Expression::Override(handle))
                    }
                    LoweredGlobalDecl::Function { .. }
                    | LoweredGlobalDecl::Type(_)
                    | LoweredGlobalDecl::EntryPoint => {
                        return Err(Box::new(Error::Unexpected(span, ExpectedToken::Variable)));
                    }
                };

                return expr.try_map(|handle| ctx.interrupt_emitter(handle, span));
            }
            ast::Expression::Construct {
                ref ty,
                ty_span,
                ref components,
            } => {
                let handle = self.construct(span, ty, ty_span, components, ctx)?;
                return Ok(Typed::Plain(handle));
            }
            ast::Expression::Unary { op, expr } => {
                let expr = self.expression_for_abstract(expr, ctx)?;
                Typed::Plain(crate::Expression::Unary { op, expr })
            }
            ast::Expression::AddrOf(expr) => {
                // The `&` operator simply converts a reference to a pointer. And since a
                // reference is required, the Load Rule is not applied.
                match self.expression_for_reference(expr, ctx)? {
                    Typed::Reference(handle) => {
                        let expr = &ctx.runtime_expression_ctx(span)?.function.expressions[handle];
                        if let &crate::Expression::Access { base, .. }
                        | &crate::Expression::AccessIndex { base, .. } = expr
                        {
                            if let Some(ty) = resolve_inner!(ctx, base).pointer_base_type() {
                                if matches!(
                                    *ty.inner_with(&ctx.module.types),
                                    crate::TypeInner::Vector { .. },
                                ) {
                                    return Err(Box::new(Error::InvalidAddrOfOperand(
                                        ctx.get_expression_span(handle),
                                    )));
                                }
                            }
                        }
                        // No code is generated. We just declare the reference a pointer now.
                        return Ok(Typed::Plain(handle));
                    }
                    Typed::Plain(_) => {
                        return Err(Box::new(Error::NotReference(
                            "the operand of the `&` operator",
                            span,
                        )));
                    }
                }
            }
            ast::Expression::Deref(expr) => {
                // The pointer we dereference must be loaded.
                let pointer = self.expression(expr, ctx)?;

                if resolve_inner!(ctx, pointer).pointer_space().is_none() {
                    return Err(Box::new(Error::NotPointer(span)));
                }

                // No code is generated. We just declare the pointer a reference now.
                return Ok(Typed::Reference(pointer));
            }
            ast::Expression::Binary { op, left, right } => {
                self.binary(op, left, right, span, ctx)?
            }
            ast::Expression::Call {
                ref function,
                ref arguments,
            } => {
                let handle = self
                    .call(span, function, arguments, ctx, false)?
                    .ok_or(Error::FunctionReturnsVoid(function.span))?;
                return Ok(Typed::Plain(handle));
            }
            ast::Expression::Index { base, index } => {
                let mut lowered_base = self.expression_for_reference(base, ctx)?;
                let index = self.expression(index, ctx)?;

                // <https://www.w3.org/TR/WGSL/#language_extension-pointer_composite_access>
                // Declare pointer as reference
                if let Typed::Plain(handle) = lowered_base {
                    if resolve_inner!(ctx, handle).pointer_space().is_some() {
                        lowered_base = Typed::Reference(handle);
                    }
                }

                lowered_base.try_map(|base| match ctx.const_eval_expr_to_u32(index).ok() {
                    Some(index) => {
                        Ok::<_, Box<Error>>(crate::Expression::AccessIndex { base, index })
                    }
                    None => {
                        // When an abstract array value e is indexed by an expression
                        // that is not a const-expression, then the array is concretized
                        // before the index is applied.
                        // https://www.w3.org/TR/WGSL/#array-access-expr
                        // Also applies to vectors and matrices.
                        let base = ctx.concretize(base)?;
                        Ok(crate::Expression::Access { base, index })
                    }
                })?
            }
            ast::Expression::Member { base, ref field } => {
                let mut lowered_base = self.expression_for_reference(base, ctx)?;

                // <https://www.w3.org/TR/WGSL/#language_extension-pointer_composite_access>
                // Declare pointer as reference
                if let Typed::Plain(handle) = lowered_base {
                    if resolve_inner!(ctx, handle).pointer_space().is_some() {
                        lowered_base = Typed::Reference(handle);
                    }
                }

                let temp_ty;
                let composite_type: &crate::TypeInner = match lowered_base {
                    Typed::Reference(handle) => {
                        temp_ty = resolve_inner!(ctx, handle)
                            .pointer_base_type()
                            .expect("In Typed::Reference(handle), handle must be a Naga pointer");
                        temp_ty.inner_with(&ctx.module.types)
                    }

                    Typed::Plain(handle) => {
                        resolve_inner!(ctx, handle)
                    }
                };

                let access = match *composite_type {
                    crate::TypeInner::Struct { ref members, .. } => {
                        let index = members
                            .iter()
                            .position(|m| m.name.as_deref() == Some(field.name))
                            .ok_or(Error::BadAccessor(field.span))?
                            as u32;

                        lowered_base.map(|base| crate::Expression::AccessIndex { base, index })
                    }
                    crate::TypeInner::Vector { .. } => {
                        match Components::new(field.name, field.span)? {
                            Components::Swizzle { size, pattern } => {
                                Typed::Plain(crate::Expression::Swizzle {
                                    size,
                                    vector: ctx.apply_load_rule(lowered_base)?,
                                    pattern,
                                })
                            }
                            Components::Single(index) => lowered_base
                                .map(|base| crate::Expression::AccessIndex { base, index }),
                        }
                    }
                    _ => return Err(Box::new(Error::BadAccessor(field.span))),
                };

                access
            }
            ast::Expression::Bitcast { expr, to, ty_span } => {
                let expr = self.expression(expr, ctx)?;
                let to_resolved = self.resolve_ast_type(to, &mut ctx.as_const())?;

                let element_scalar = match ctx.module.types[to_resolved].inner {
                    crate::TypeInner::Scalar(scalar) => scalar,
                    crate::TypeInner::Vector { scalar, .. } => scalar,
                    _ => {
                        let ty = resolve!(ctx, expr);
                        return Err(Box::new(Error::BadTypeCast {
                            from_type: ctx.type_resolution_to_string(ty),
                            span: ty_span,
                            to_type: ctx.type_to_string(to_resolved),
                        }));
                    }
                };

                Typed::Plain(crate::Expression::As {
                    expr,
                    kind: element_scalar.kind,
                    convert: None,
                })
            }
        };

        expr.try_map(|handle| ctx.append_expression(handle, span))
    }

    fn binary(
        &mut self,
        op: crate::BinaryOperator,
        left: Handle<ast::Expression<'source>>,
        right: Handle<ast::Expression<'source>>,
        span: Span,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Typed<crate::Expression>> {
        // Load both operands.
        let mut left = self.expression_for_abstract(left, ctx)?;
        let mut right = self.expression_for_abstract(right, ctx)?;

        // Convert `scalar op vector` to `vector op vector` by introducing
        // `Splat` expressions.
        ctx.binary_op_splat(op, &mut left, &mut right)?;

        // Apply automatic conversions.
        match op {
            crate::BinaryOperator::ShiftLeft | crate::BinaryOperator::ShiftRight => {
                // Shift operators require the right operand to be `u32` or
                // `vecN<u32>`. We can let the validator sort out vector length
                // issues, but the right operand must be, or convert to, a u32 leaf
                // scalar.
                right =
                    ctx.try_automatic_conversion_for_leaf_scalar(right, crate::Scalar::U32, span)?;

                // Additionally, we must concretize the left operand if the right operand
                // is not a const-expression.
                // See https://www.w3.org/TR/WGSL/#overload-resolution-section.
                //
                // 2. Eliminate any candidate where one of its subexpressions resolves to
                // an abstract type after feasible automatic conversions, but another of
                // the candidate’s subexpressions is not a const-expression.
                //
                // We only have to explicitly do so for shifts as their operands may be
                // of different types - for other binary ops this is achieved by finding
                // the conversion consensus for both operands.
                let expr_kind_tracker = match ctx.expr_type {
                    ExpressionContextType::Runtime(ref ctx)
                    | ExpressionContextType::Constant(Some(ref ctx)) => {
                        &ctx.local_expression_kind_tracker
                    }
                    ExpressionContextType::Constant(None) | ExpressionContextType::Override => {
                        &ctx.global_expression_kind_tracker
                    }
                };
                if !expr_kind_tracker.is_const(right) {
                    left = ctx.concretize(left)?;
                }
            }

            // All other operators follow the same pattern: reconcile the
            // scalar leaf types. If there's no reconciliation possible,
            // leave the expressions as they are: validation will report the
            // problem.
            _ => {
                ctx.grow_types(left)?;
                ctx.grow_types(right)?;
                if let Ok(consensus_scalar) =
                    ctx.automatic_conversion_consensus([left, right].iter())
                {
                    ctx.convert_to_leaf_scalar(&mut left, consensus_scalar)?;
                    ctx.convert_to_leaf_scalar(&mut right, consensus_scalar)?;
                }
            }
        }

        Ok(Typed::Plain(crate::Expression::Binary { op, left, right }))
    }

    /// Generate Naga IR for call expressions and statements, and type
    /// constructor expressions.
    ///
    /// The "function" being called is simply an `Ident` that we know refers to
    /// some module-scope definition.
    ///
    /// - If it is the name of a type, then the expression is a type constructor
    ///   expression: either constructing a value from components, a conversion
    ///   expression, or a zero value expression.
    ///
    /// - If it is the name of a function, then we're generating a [`Call`]
    ///   statement. We may be in the midst of generating code for an
    ///   expression, in which case we must generate an `Emit` statement to
    ///   force evaluation of the IR expressions we've generated so far, add the
    ///   `Call` statement to the current block, and then resume generating
    ///   expressions.
    ///
    /// [`Call`]: crate::Statement::Call
    fn call(
        &mut self,
        span: Span,
        function: &ast::Ident<'source>,
        arguments: &[Handle<ast::Expression<'source>>],
        ctx: &mut ExpressionContext<'source, '_, '_>,
        is_statement: bool,
    ) -> Result<'source, Option<Handle<crate::Expression>>> {
        let function_span = function.span;
        match ctx.globals.get(function.name) {
            Some(&LoweredGlobalDecl::Type(ty)) => {
                let handle = self.construct(
                    span,
                    &ast::ConstructorType::Type(ty),
                    function_span,
                    arguments,
                    ctx,
                )?;
                Ok(Some(handle))
            }
            Some(
                &LoweredGlobalDecl::Const(_)
                | &LoweredGlobalDecl::Override(_)
                | &LoweredGlobalDecl::Var(_),
            ) => Err(Box::new(Error::Unexpected(
                function_span,
                ExpectedToken::Function,
            ))),
            Some(&LoweredGlobalDecl::EntryPoint) => {
                Err(Box::new(Error::CalledEntryPoint(function_span)))
            }
            Some(&LoweredGlobalDecl::Function {
                handle: function,
                must_use,
            }) => {
                let arguments = arguments
                    .iter()
                    .enumerate()
                    .map(|(i, &arg)| {
                        // Try to convert abstract values to the known argument types
                        let Some(&crate::FunctionArgument {
                            ty: parameter_ty, ..
                        }) = ctx.module.functions[function].arguments.get(i)
                        else {
                            // Wrong number of arguments... just concretize the type here
                            // and let the validator report the error.
                            return self.expression(arg, ctx);
                        };

                        let expr = self.expression_for_abstract(arg, ctx)?;
                        ctx.try_automatic_conversions(
                            expr,
                            &crate::proc::TypeResolution::Handle(parameter_ty),
                            ctx.ast_expressions.get_span(arg),
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;

                let has_result = ctx.module.functions[function].result.is_some();

                if must_use && is_statement {
                    return Err(Box::new(Error::FunctionMustUseUnused(function_span)));
                }

                let rctx = ctx.runtime_expression_ctx(span)?;
                // we need to always do this before a fn call since all arguments need to be emitted before the fn call
                rctx.block
                    .extend(rctx.emitter.finish(&rctx.function.expressions));
                let result = has_result.then(|| {
                    let result = rctx
                        .function
                        .expressions
                        .append(crate::Expression::CallResult(function), span);
                    rctx.local_expression_kind_tracker
                        .insert(result, crate::proc::ExpressionKind::Runtime);
                    result
                });
                rctx.emitter.start(&rctx.function.expressions);
                rctx.block.push(
                    crate::Statement::Call {
                        function,
                        arguments,
                        result,
                    },
                    span,
                );

                Ok(result)
            }
            None => {
                let span = function_span;
                let expr = if let Some(fun) = conv::map_relational_fun(function.name) {
                    let mut args = ctx.prepare_args(arguments, 1, span);
                    let argument = self.expression(args.next()?, ctx)?;
                    args.finish()?;

                    // Check for no-op all(bool) and any(bool):
                    let argument_unmodified = matches!(
                        fun,
                        crate::RelationalFunction::All | crate::RelationalFunction::Any
                    ) && {
                        matches!(
                            resolve_inner!(ctx, argument),
                            &crate::TypeInner::Scalar(crate::Scalar {
                                kind: crate::ScalarKind::Bool,
                                ..
                            })
                        )
                    };

                    if argument_unmodified {
                        return Ok(Some(argument));
                    } else {
                        crate::Expression::Relational { fun, argument }
                    }
                } else if let Some((axis, ctrl)) = conv::map_derivative(function.name) {
                    let mut args = ctx.prepare_args(arguments, 1, span);
                    let expr = self.expression(args.next()?, ctx)?;
                    args.finish()?;

                    crate::Expression::Derivative { axis, ctrl, expr }
                } else if let Some(fun) = conv::map_standard_fun(function.name) {
                    let expected = fun.argument_count() as _;
                    let mut args = ctx.prepare_args(arguments, expected, span);

                    let arg = self.expression(args.next()?, ctx)?;
                    let arg1 = args
                        .next()
                        .map(|x| self.expression(x, ctx))
                        .ok()
                        .transpose()?;
                    let arg2 = args
                        .next()
                        .map(|x| self.expression(x, ctx))
                        .ok()
                        .transpose()?;
                    let arg3 = args
                        .next()
                        .map(|x| self.expression(x, ctx))
                        .ok()
                        .transpose()?;

                    args.finish()?;

                    if fun == crate::MathFunction::Modf || fun == crate::MathFunction::Frexp {
                        if let Some((size, scalar)) = match *resolve_inner!(ctx, arg) {
                            crate::TypeInner::Scalar(scalar) => Some((None, scalar)),
                            crate::TypeInner::Vector { size, scalar, .. } => {
                                Some((Some(size), scalar))
                            }
                            _ => None,
                        } {
                            ctx.module.generate_predeclared_type(
                                if fun == crate::MathFunction::Modf {
                                    crate::PredeclaredType::ModfResult { size, scalar }
                                } else {
                                    crate::PredeclaredType::FrexpResult { size, scalar }
                                },
                            );
                        }
                    }

                    crate::Expression::Math {
                        fun,
                        arg,
                        arg1,
                        arg2,
                        arg3,
                    }
                } else if let Some(fun) = Texture::map(function.name) {
                    self.texture_sample_helper(fun, arguments, span, ctx)?
                } else if let Some((op, cop)) = conv::map_subgroup_operation(function.name) {
                    return Ok(Some(
                        self.subgroup_operation_helper(span, op, cop, arguments, ctx)?,
                    ));
                } else if let Some(mode) = SubgroupGather::map(function.name) {
                    return Ok(Some(
                        self.subgroup_gather_helper(span, mode, arguments, ctx)?,
                    ));
                } else if let Some(fun) = crate::AtomicFunction::map(function.name) {
                    return self.atomic_helper(span, fun, arguments, is_statement, ctx);
                } else {
                    match function.name {
                        "select" => {
                            let mut args = ctx.prepare_args(arguments, 3, span);

                            let reject = self.expression(args.next()?, ctx)?;
                            let accept = self.expression(args.next()?, ctx)?;
                            let condition = self.expression(args.next()?, ctx)?;

                            args.finish()?;

                            crate::Expression::Select {
                                reject,
                                accept,
                                condition,
                            }
                        }
                        "arrayLength" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let expr = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            crate::Expression::ArrayLength(expr)
                        }
                        "atomicLoad" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let pointer = self.atomic_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            crate::Expression::Load { pointer }
                        }
                        "atomicStore" => {
                            let mut args = ctx.prepare_args(arguments, 2, span);
                            let pointer = self.atomic_pointer(args.next()?, ctx)?;
                            let value = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .extend(rctx.emitter.finish(&rctx.function.expressions));
                            rctx.emitter.start(&rctx.function.expressions);
                            rctx.block
                                .push(crate::Statement::Store { pointer, value }, span);
                            return Ok(None);
                        }
                        "atomicCompareExchangeWeak" => {
                            let mut args = ctx.prepare_args(arguments, 3, span);

                            let pointer = self.atomic_pointer(args.next()?, ctx)?;

                            let compare = self.expression(args.next()?, ctx)?;

                            let value = args.next()?;
                            let value_span = ctx.ast_expressions.get_span(value);
                            let value = self.expression(value, ctx)?;

                            args.finish()?;

                            let expression = match *resolve_inner!(ctx, value) {
                                crate::TypeInner::Scalar(scalar) => {
                                    crate::Expression::AtomicResult {
                                        ty: ctx.module.generate_predeclared_type(
                                            crate::PredeclaredType::AtomicCompareExchangeWeakResult(
                                                scalar,
                                            ),
                                        ),
                                        comparison: true,
                                    }
                                }
                                _ => {
                                    return Err(Box::new(Error::InvalidAtomicOperandType(
                                        value_span,
                                    )))
                                }
                            };

                            let result = ctx.interrupt_emitter(expression, span)?;
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block.push(
                                crate::Statement::Atomic {
                                    pointer,
                                    fun: crate::AtomicFunction::Exchange {
                                        compare: Some(compare),
                                    },
                                    value,
                                    result: Some(result),
                                },
                                span,
                            );
                            return Ok(Some(result));
                        }
                        "textureAtomicMin" | "textureAtomicMax" | "textureAtomicAdd"
                        | "textureAtomicAnd" | "textureAtomicOr" | "textureAtomicXor" => {
                            let mut args = ctx.prepare_args(arguments, 3, span);

                            let image = args.next()?;
                            let image_span = ctx.ast_expressions.get_span(image);
                            let image = self.expression(image, ctx)?;

                            let coordinate = self.expression(args.next()?, ctx)?;

                            let (_, arrayed) = ctx.image_data(image, image_span)?;
                            let array_index = arrayed
                                .then(|| {
                                    args.min_args += 1;
                                    self.expression(args.next()?, ctx)
                                })
                                .transpose()?;

                            let value = self.expression(args.next()?, ctx)?;

                            args.finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .extend(rctx.emitter.finish(&rctx.function.expressions));
                            rctx.emitter.start(&rctx.function.expressions);
                            let stmt = crate::Statement::ImageAtomic {
                                image,
                                coordinate,
                                array_index,
                                fun: match function.name {
                                    "textureAtomicMin" => crate::AtomicFunction::Min,
                                    "textureAtomicMax" => crate::AtomicFunction::Max,
                                    "textureAtomicAdd" => crate::AtomicFunction::Add,
                                    "textureAtomicAnd" => crate::AtomicFunction::And,
                                    "textureAtomicOr" => crate::AtomicFunction::InclusiveOr,
                                    "textureAtomicXor" => crate::AtomicFunction::ExclusiveOr,
                                    _ => unreachable!(),
                                },
                                value,
                            };
                            rctx.block.push(stmt, span);
                            return Ok(None);
                        }
                        "storageBarrier" => {
                            ctx.prepare_args(arguments, 0, span).finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::Barrier(crate::Barrier::STORAGE), span);
                            return Ok(None);
                        }
                        "workgroupBarrier" => {
                            ctx.prepare_args(arguments, 0, span).finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::Barrier(crate::Barrier::WORK_GROUP), span);
                            return Ok(None);
                        }
                        "subgroupBarrier" => {
                            ctx.prepare_args(arguments, 0, span).finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::Barrier(crate::Barrier::SUB_GROUP), span);
                            return Ok(None);
                        }
                        "textureBarrier" => {
                            ctx.prepare_args(arguments, 0, span).finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::Barrier(crate::Barrier::TEXTURE), span);
                            return Ok(None);
                        }
                        "workgroupUniformLoad" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let expr = args.next()?;
                            args.finish()?;

                            let pointer = self.expression(expr, ctx)?;
                            let result_ty = match *resolve_inner!(ctx, pointer) {
                                crate::TypeInner::Pointer {
                                    base,
                                    space: crate::AddressSpace::WorkGroup,
                                } => base,
                                ref other => {
                                    log::error!("Type {other:?} passed to workgroupUniformLoad");
                                    let span = ctx.ast_expressions.get_span(expr);
                                    return Err(Box::new(Error::InvalidWorkGroupUniformLoad(span)));
                                }
                            };
                            let result = ctx.interrupt_emitter(
                                crate::Expression::WorkGroupUniformLoadResult { ty: result_ty },
                                span,
                            )?;
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block.push(
                                crate::Statement::WorkGroupUniformLoad { pointer, result },
                                span,
                            );

                            return Ok(Some(result));
                        }
                        "textureStore" => {
                            let mut args = ctx.prepare_args(arguments, 3, span);

                            let image = args.next()?;
                            let image_span = ctx.ast_expressions.get_span(image);
                            let image = self.expression(image, ctx)?;

                            let coordinate = self.expression(args.next()?, ctx)?;

                            let (_, arrayed) = ctx.image_data(image, image_span)?;
                            let array_index = arrayed
                                .then(|| {
                                    args.min_args += 1;
                                    self.expression(args.next()?, ctx)
                                })
                                .transpose()?;

                            let value = self.expression(args.next()?, ctx)?;

                            args.finish()?;

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .extend(rctx.emitter.finish(&rctx.function.expressions));
                            rctx.emitter.start(&rctx.function.expressions);
                            let stmt = crate::Statement::ImageStore {
                                image,
                                coordinate,
                                array_index,
                                value,
                            };
                            rctx.block.push(stmt, span);
                            return Ok(None);
                        }
                        "textureLoad" => {
                            let mut args = ctx.prepare_args(arguments, 2, span);

                            let image = args.next()?;
                            let image_span = ctx.ast_expressions.get_span(image);
                            let image = self.expression(image, ctx)?;

                            let coordinate = self.expression(args.next()?, ctx)?;

                            let (class, arrayed) = ctx.image_data(image, image_span)?;
                            let array_index = arrayed
                                .then(|| {
                                    args.min_args += 1;
                                    self.expression(args.next()?, ctx)
                                })
                                .transpose()?;

                            let level = class
                                .is_mipmapped()
                                .then(|| {
                                    args.min_args += 1;
                                    self.expression(args.next()?, ctx)
                                })
                                .transpose()?;

                            let sample = class
                                .is_multisampled()
                                .then(|| self.expression(args.next()?, ctx))
                                .transpose()?;

                            args.finish()?;

                            crate::Expression::ImageLoad {
                                image,
                                coordinate,
                                array_index,
                                level,
                                sample,
                            }
                        }
                        "textureDimensions" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let image = self.expression(args.next()?, ctx)?;
                            let level = args
                                .next()
                                .map(|arg| self.expression(arg, ctx))
                                .ok()
                                .transpose()?;
                            args.finish()?;

                            crate::Expression::ImageQuery {
                                image,
                                query: crate::ImageQuery::Size { level },
                            }
                        }
                        "textureNumLevels" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let image = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            crate::Expression::ImageQuery {
                                image,
                                query: crate::ImageQuery::NumLevels,
                            }
                        }
                        "textureNumLayers" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let image = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            crate::Expression::ImageQuery {
                                image,
                                query: crate::ImageQuery::NumLayers,
                            }
                        }
                        "textureNumSamples" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let image = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            crate::Expression::ImageQuery {
                                image,
                                query: crate::ImageQuery::NumSamples,
                            }
                        }
                        "rayQueryInitialize" => {
                            let mut args = ctx.prepare_args(arguments, 3, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            let acceleration_structure = self.expression(args.next()?, ctx)?;
                            let descriptor = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            let _ = ctx.module.generate_ray_desc_type();
                            let fun = crate::RayQueryFunction::Initialize {
                                acceleration_structure,
                                descriptor,
                            };

                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .extend(rctx.emitter.finish(&rctx.function.expressions));
                            rctx.emitter.start(&rctx.function.expressions);
                            rctx.block
                                .push(crate::Statement::RayQuery { query, fun }, span);
                            return Ok(None);
                        }
                        "getCommittedHitVertexPositions" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let _ = ctx.module.generate_vertex_return_type();

                            crate::Expression::RayQueryVertexPositions {
                                query,
                                committed: true,
                            }
                        }
                        "getCandidateHitVertexPositions" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let _ = ctx.module.generate_vertex_return_type();

                            crate::Expression::RayQueryVertexPositions {
                                query,
                                committed: false,
                            }
                        }
                        "rayQueryProceed" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let result = ctx.interrupt_emitter(
                                crate::Expression::RayQueryProceedResult,
                                span,
                            )?;
                            let fun = crate::RayQueryFunction::Proceed { result };
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::RayQuery { query, fun }, span);
                            return Ok(Some(result));
                        }
                        "rayQueryGenerateIntersection" => {
                            let mut args = ctx.prepare_args(arguments, 2, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            let hit_t = self.expression(args.next()?, ctx)?;
                            args.finish()?;

                            let fun = crate::RayQueryFunction::GenerateIntersection { hit_t };
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::RayQuery { query, fun }, span);
                            return Ok(None);
                        }
                        "rayQueryConfirmIntersection" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let fun = crate::RayQueryFunction::ConfirmIntersection;
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::RayQuery { query, fun }, span);
                            return Ok(None);
                        }
                        "rayQueryTerminate" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let fun = crate::RayQueryFunction::Terminate;
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::RayQuery { query, fun }, span);
                            return Ok(None);
                        }
                        "rayQueryGetCommittedIntersection" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let _ = ctx.module.generate_ray_intersection_type();
                            crate::Expression::RayQueryGetIntersection {
                                query,
                                committed: true,
                            }
                        }
                        "rayQueryGetCandidateIntersection" => {
                            let mut args = ctx.prepare_args(arguments, 1, span);
                            let query = self.ray_query_pointer(args.next()?, ctx)?;
                            args.finish()?;

                            let _ = ctx.module.generate_ray_intersection_type();
                            crate::Expression::RayQueryGetIntersection {
                                query,
                                committed: false,
                            }
                        }
                        "RayDesc" => {
                            let ty = ctx.module.generate_ray_desc_type();
                            let handle = self.construct(
                                span,
                                &ast::ConstructorType::Type(ty),
                                function.span,
                                arguments,
                                ctx,
                            )?;
                            return Ok(Some(handle));
                        }
                        "subgroupBallot" => {
                            let mut args = ctx.prepare_args(arguments, 0, span);
                            let predicate = if arguments.len() == 1 {
                                Some(self.expression(args.next()?, ctx)?)
                            } else {
                                None
                            };
                            args.finish()?;

                            let result = ctx
                                .interrupt_emitter(crate::Expression::SubgroupBallotResult, span)?;
                            let rctx = ctx.runtime_expression_ctx(span)?;
                            rctx.block
                                .push(crate::Statement::SubgroupBallot { result, predicate }, span);
                            return Ok(Some(result));
                        }
                        _ => {
                            return Err(Box::new(Error::UnknownIdent(function.span, function.name)))
                        }
                    }
                };

                let expr = ctx.append_expression(expr, span)?;
                Ok(Some(expr))
            }
        }
    }

    fn atomic_pointer(
        &mut self,
        expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let span = ctx.ast_expressions.get_span(expr);
        let pointer = self.expression(expr, ctx)?;

        match *resolve_inner!(ctx, pointer) {
            crate::TypeInner::Pointer { base, .. } => match ctx.module.types[base].inner {
                crate::TypeInner::Atomic { .. } => Ok(pointer),
                ref other => {
                    log::error!("Pointer type to {:?} passed to atomic op", other);
                    Err(Box::new(Error::InvalidAtomicPointer(span)))
                }
            },
            ref other => {
                log::error!("Type {:?} passed to atomic op", other);
                Err(Box::new(Error::InvalidAtomicPointer(span)))
            }
        }
    }

    fn atomic_helper(
        &mut self,
        span: Span,
        fun: crate::AtomicFunction,
        args: &[Handle<ast::Expression<'source>>],
        is_statement: bool,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Option<Handle<crate::Expression>>> {
        let mut args = ctx.prepare_args(args, 2, span);

        let pointer = self.atomic_pointer(args.next()?, ctx)?;
        let value = self.expression(args.next()?, ctx)?;
        let value_inner = resolve_inner!(ctx, value);
        args.finish()?;

        // If we don't use the return value of a 64-bit `min` or `max`
        // operation, generate a no-result form of the `Atomic` statement, so
        // that we can pass validation with only `SHADER_INT64_ATOMIC_MIN_MAX`
        // whenever possible.
        let is_64_bit_min_max =
            matches!(fun, crate::AtomicFunction::Min | crate::AtomicFunction::Max)
                && matches!(
                    *value_inner,
                    crate::TypeInner::Scalar(crate::Scalar { width: 8, .. })
                );
        let result = if is_64_bit_min_max && is_statement {
            let rctx = ctx.runtime_expression_ctx(span)?;
            rctx.block
                .extend(rctx.emitter.finish(&rctx.function.expressions));
            rctx.emitter.start(&rctx.function.expressions);
            None
        } else {
            let ty = ctx.register_type(value)?;
            Some(ctx.interrupt_emitter(
                crate::Expression::AtomicResult {
                    ty,
                    comparison: false,
                },
                span,
            )?)
        };
        let rctx = ctx.runtime_expression_ctx(span)?;
        rctx.block.push(
            crate::Statement::Atomic {
                pointer,
                fun,
                value,
                result,
            },
            span,
        );
        Ok(result)
    }

    fn texture_sample_helper(
        &mut self,
        fun: Texture,
        args: &[Handle<ast::Expression<'source>>],
        span: Span,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, crate::Expression> {
        let mut args = ctx.prepare_args(args, fun.min_argument_count(), span);

        fn get_image_and_span<'source>(
            lowerer: &mut Lowerer<'source, '_>,
            args: &mut ArgumentContext<'_, 'source>,
            ctx: &mut ExpressionContext<'source, '_, '_>,
        ) -> Result<'source, (Handle<crate::Expression>, Span)> {
            let image = args.next()?;
            let image_span = ctx.ast_expressions.get_span(image);
            let image = lowerer.expression(image, ctx)?;
            Ok((image, image_span))
        }

        let (image, image_span, gather) = match fun {
            Texture::Gather => {
                let image_or_component = args.next()?;
                let image_or_component_span = ctx.ast_expressions.get_span(image_or_component);
                // Gathers from depth textures don't take an initial `component` argument.
                let lowered_image_or_component = self.expression(image_or_component, ctx)?;

                match *resolve_inner!(ctx, lowered_image_or_component) {
                    crate::TypeInner::Image {
                        class: crate::ImageClass::Depth { .. },
                        ..
                    } => (
                        lowered_image_or_component,
                        image_or_component_span,
                        Some(crate::SwizzleComponent::X),
                    ),
                    _ => {
                        let (image, image_span) = get_image_and_span(self, &mut args, ctx)?;
                        (
                            image,
                            image_span,
                            Some(ctx.gather_component(
                                lowered_image_or_component,
                                image_or_component_span,
                                span,
                            )?),
                        )
                    }
                }
            }
            Texture::GatherCompare => {
                let (image, image_span) = get_image_and_span(self, &mut args, ctx)?;
                (image, image_span, Some(crate::SwizzleComponent::X))
            }

            _ => {
                let (image, image_span) = get_image_and_span(self, &mut args, ctx)?;
                (image, image_span, None)
            }
        };

        let sampler = self.expression(args.next()?, ctx)?;

        let coordinate = self.expression(args.next()?, ctx)?;

        let (_, arrayed) = ctx.image_data(image, image_span)?;
        let array_index = arrayed
            .then(|| self.expression(args.next()?, ctx))
            .transpose()?;

        let (level, depth_ref) = match fun {
            Texture::Gather => (crate::SampleLevel::Zero, None),
            Texture::GatherCompare => {
                let reference = self.expression(args.next()?, ctx)?;
                (crate::SampleLevel::Zero, Some(reference))
            }

            Texture::Sample => (crate::SampleLevel::Auto, None),
            Texture::SampleBias => {
                let bias = self.expression(args.next()?, ctx)?;
                (crate::SampleLevel::Bias(bias), None)
            }
            Texture::SampleCompare => {
                let reference = self.expression(args.next()?, ctx)?;
                (crate::SampleLevel::Auto, Some(reference))
            }
            Texture::SampleCompareLevel => {
                let reference = self.expression(args.next()?, ctx)?;
                (crate::SampleLevel::Zero, Some(reference))
            }
            Texture::SampleGrad => {
                let x = self.expression(args.next()?, ctx)?;
                let y = self.expression(args.next()?, ctx)?;
                (crate::SampleLevel::Gradient { x, y }, None)
            }
            Texture::SampleLevel => {
                let level = self.expression(args.next()?, ctx)?;
                (crate::SampleLevel::Exact(level), None)
            }
        };

        let offset = args
            .next()
            .map(|arg| self.expression(arg, &mut ctx.as_const()))
            .ok()
            .transpose()?;

        args.finish()?;

        Ok(crate::Expression::ImageSample {
            image,
            sampler,
            gather,
            coordinate,
            array_index,
            offset,
            level,
            depth_ref,
        })
    }

    fn subgroup_operation_helper(
        &mut self,
        span: Span,
        op: crate::SubgroupOperation,
        collective_op: crate::CollectiveOperation,
        arguments: &[Handle<ast::Expression<'source>>],
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let mut args = ctx.prepare_args(arguments, 1, span);

        let argument = self.expression(args.next()?, ctx)?;
        args.finish()?;

        let ty = ctx.register_type(argument)?;

        let result =
            ctx.interrupt_emitter(crate::Expression::SubgroupOperationResult { ty }, span)?;
        let rctx = ctx.runtime_expression_ctx(span)?;
        rctx.block.push(
            crate::Statement::SubgroupCollectiveOperation {
                op,
                collective_op,
                argument,
                result,
            },
            span,
        );
        Ok(result)
    }

    fn subgroup_gather_helper(
        &mut self,
        span: Span,
        mode: SubgroupGather,
        arguments: &[Handle<ast::Expression<'source>>],
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let mut args = ctx.prepare_args(arguments, 2, span);

        let argument = self.expression(args.next()?, ctx)?;

        use SubgroupGather as Sg;
        let mode = if let Sg::BroadcastFirst = mode {
            crate::GatherMode::BroadcastFirst
        } else {
            let index = self.expression(args.next()?, ctx)?;
            match mode {
                Sg::Broadcast => crate::GatherMode::Broadcast(index),
                Sg::Shuffle => crate::GatherMode::Shuffle(index),
                Sg::ShuffleDown => crate::GatherMode::ShuffleDown(index),
                Sg::ShuffleUp => crate::GatherMode::ShuffleUp(index),
                Sg::ShuffleXor => crate::GatherMode::ShuffleXor(index),
                Sg::BroadcastFirst => unreachable!(),
            }
        };

        args.finish()?;

        let ty = ctx.register_type(argument)?;

        let result =
            ctx.interrupt_emitter(crate::Expression::SubgroupOperationResult { ty }, span)?;
        let rctx = ctx.runtime_expression_ctx(span)?;
        rctx.block.push(
            crate::Statement::SubgroupGather {
                mode,
                argument,
                result,
            },
            span,
        );
        Ok(result)
    }

    fn r#struct(
        &mut self,
        s: &ast::Struct<'source>,
        span: Span,
        ctx: &mut GlobalContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Type>> {
        let mut offset = 0;
        let mut struct_alignment = Alignment::ONE;
        let mut members = Vec::with_capacity(s.members.len());

        for member in s.members.iter() {
            let ty = self.resolve_ast_type(member.ty, &mut ctx.as_const())?;

            ctx.layouter.update(ctx.module.to_ctx()).unwrap();

            let member_min_size = ctx.layouter[ty].size;
            let member_min_alignment = ctx.layouter[ty].alignment;

            let member_size = if let Some(size_expr) = member.size {
                let (size, span) = self.const_u32(size_expr, &mut ctx.as_const())?;
                if size < member_min_size {
                    return Err(Box::new(Error::SizeAttributeTooLow(span, member_min_size)));
                } else {
                    size
                }
            } else {
                member_min_size
            };

            let member_alignment = if let Some(align_expr) = member.align {
                let (align, span) = self.const_u32(align_expr, &mut ctx.as_const())?;
                if let Some(alignment) = Alignment::new(align) {
                    if alignment < member_min_alignment {
                        return Err(Box::new(Error::AlignAttributeTooLow(
                            span,
                            member_min_alignment,
                        )));
                    } else {
                        alignment
                    }
                } else {
                    return Err(Box::new(Error::NonPowerOfTwoAlignAttribute(span)));
                }
            } else {
                member_min_alignment
            };

            let binding = self.binding(&member.binding, ty, ctx)?;

            offset = member_alignment.round_up(offset);
            struct_alignment = struct_alignment.max(member_alignment);

            members.push(crate::StructMember {
                name: Some(member.name.name.to_owned()),
                ty,
                binding,
                offset,
            });

            offset += member_size;
        }

        let size = struct_alignment.round_up(offset);
        let inner = crate::TypeInner::Struct {
            members,
            span: size,
        };

        let handle = ctx.module.types.insert(
            crate::Type {
                name: Some(s.name.name.to_string()),
                inner,
            },
            span,
        );
        Ok(handle)
    }

    fn const_u32(
        &mut self,
        expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, (u32, Span)> {
        let span = ctx.ast_expressions.get_span(expr);
        let expr = self.expression(expr, ctx)?;
        let value = ctx
            .module
            .to_ctx()
            .eval_expr_to_u32(expr)
            .map_err(|err| match err {
                crate::proc::U32EvalError::NonConst => {
                    Error::ExpectedConstExprConcreteIntegerScalar(span)
                }
                crate::proc::U32EvalError::Negative => Error::ExpectedNonNegative(span),
            })?;
        Ok((value, span))
    }

    fn array_size(
        &mut self,
        size: ast::ArraySize<'source>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, crate::ArraySize> {
        Ok(match size {
            ast::ArraySize::Constant(expr) => {
                let span = ctx.ast_expressions.get_span(expr);
                let const_expr = self.expression(expr, &mut ctx.as_const());
                match const_expr {
                    Ok(value) => {
                        let len = ctx.const_eval_expr_to_u32(value).map_err(|err| {
                            Box::new(match err {
                                crate::proc::U32EvalError::NonConst => {
                                    Error::ExpectedConstExprConcreteIntegerScalar(span)
                                }
                                crate::proc::U32EvalError::Negative => {
                                    Error::ExpectedPositiveArrayLength(span)
                                }
                            })
                        })?;
                        let size =
                            NonZeroU32::new(len).ok_or(Error::ExpectedPositiveArrayLength(span))?;
                        crate::ArraySize::Constant(size)
                    }
                    Err(err) => {
                        if let Error::ConstantEvaluatorError(ref ty, _) = *err {
                            match **ty {
                                crate::proc::ConstantEvaluatorError::OverrideExpr => {
                                    crate::ArraySize::Pending(self.array_size_override(
                                        expr,
                                        &mut ctx.as_global().as_override(),
                                        span,
                                    )?)
                                }
                                _ => {
                                    return Err(err);
                                }
                            }
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
            ast::ArraySize::Dynamic => crate::ArraySize::Dynamic,
        })
    }

    fn array_size_override(
        &mut self,
        size_expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
        span: Span,
    ) -> Result<'source, Handle<crate::Override>> {
        let expr = self.expression(size_expr, ctx)?;
        match resolve_inner!(ctx, expr).scalar_kind().ok_or(0) {
            Ok(crate::ScalarKind::Sint) | Ok(crate::ScalarKind::Uint) => Ok({
                if let crate::Expression::Override(handle) = ctx.module.global_expressions[expr] {
                    handle
                } else {
                    let ty = ctx.register_type(expr)?;
                    ctx.module.overrides.append(
                        crate::Override {
                            name: None,
                            id: None,
                            ty,
                            init: Some(expr),
                        },
                        span,
                    )
                }
            }),
            _ => Err(Box::new(Error::ExpectedConstExprConcreteIntegerScalar(
                span,
            ))),
        }
    }

    /// Build the Naga equivalent of a named AST type.
    ///
    /// Return a Naga `Handle<Type>` representing the front-end type
    /// `handle`, which should be named `name`, if given.
    ///
    /// If `handle` refers to a type cached in [`SpecialTypes`],
    /// `name` may be ignored.
    ///
    /// [`SpecialTypes`]: crate::SpecialTypes
    fn resolve_named_ast_type(
        &mut self,
        handle: Handle<ast::Type<'source>>,
        name: Option<String>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Type>> {
        let inner = match ctx.types[handle] {
            ast::Type::Scalar(scalar) => scalar.to_inner_scalar(),
            ast::Type::Vector { size, ty, ty_span } => {
                let ty = self.resolve_ast_type(ty, ctx)?;
                let scalar = match ctx.module.types[ty].inner {
                    crate::TypeInner::Scalar(sc) => sc,
                    _ => return Err(Box::new(Error::UnknownScalarType(ty_span))),
                };
                crate::TypeInner::Vector { size, scalar }
            }
            ast::Type::Matrix {
                rows,
                columns,
                ty,
                ty_span,
            } => {
                let ty = self.resolve_ast_type(ty, ctx)?;
                let scalar = match ctx.module.types[ty].inner {
                    crate::TypeInner::Scalar(sc) => sc,
                    _ => return Err(Box::new(Error::UnknownScalarType(ty_span))),
                };
                match scalar.kind {
                    crate::ScalarKind::Float => crate::TypeInner::Matrix {
                        columns,
                        rows,
                        scalar,
                    },
                    _ => return Err(Box::new(Error::BadMatrixScalarKind(ty_span, scalar))),
                }
            }
            ast::Type::Atomic(scalar) => scalar.to_inner_atomic(),
            ast::Type::Pointer { base, space } => {
                let base = self.resolve_ast_type(base, ctx)?;
                crate::TypeInner::Pointer { base, space }
            }
            ast::Type::Array { base, size } => {
                let base = self.resolve_ast_type(base, &mut ctx.as_const())?;
                let size = self.array_size(size, ctx)?;

                ctx.layouter.update(ctx.module.to_ctx()).unwrap();
                let stride = ctx.layouter[base].to_stride();

                crate::TypeInner::Array { base, size, stride }
            }
            ast::Type::Image {
                dim,
                arrayed,
                class,
            } => crate::TypeInner::Image {
                dim,
                arrayed,
                class,
            },
            ast::Type::Sampler { comparison } => crate::TypeInner::Sampler { comparison },
            ast::Type::AccelerationStructure { vertex_return } => {
                crate::TypeInner::AccelerationStructure { vertex_return }
            }
            ast::Type::RayQuery { vertex_return } => crate::TypeInner::RayQuery { vertex_return },
            ast::Type::BindingArray { base, size } => {
                let base = self.resolve_ast_type(base, ctx)?;
                let size = self.array_size(size, ctx)?;
                crate::TypeInner::BindingArray { base, size }
            }
            ast::Type::RayDesc => {
                return Ok(ctx.module.generate_ray_desc_type());
            }
            ast::Type::RayIntersection => {
                return Ok(ctx.module.generate_ray_intersection_type());
            }
            ast::Type::User(ref ident) => {
                return match ctx.globals.get(ident.name) {
                    Some(&LoweredGlobalDecl::Type(handle)) => Ok(handle),
                    Some(_) => Err(Box::new(Error::Unexpected(ident.span, ExpectedToken::Type))),
                    None => Err(Box::new(Error::UnknownType(ident.span))),
                }
            }
        };

        Ok(ctx.as_global().ensure_type_exists(name, inner))
    }

    /// Return a Naga `Handle<Type>` representing the front-end type `handle`.
    fn resolve_ast_type(
        &mut self,
        handle: Handle<ast::Type<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Type>> {
        self.resolve_named_ast_type(handle, None, ctx)
    }

    fn binding(
        &mut self,
        binding: &Option<ast::Binding<'source>>,
        ty: Handle<crate::Type>,
        ctx: &mut GlobalContext<'source, '_, '_>,
    ) -> Result<'source, Option<crate::Binding>> {
        Ok(match *binding {
            Some(ast::Binding::BuiltIn(b)) => Some(crate::Binding::BuiltIn(b)),
            Some(ast::Binding::Location {
                location,
                interpolation,
                sampling,
                blend_src,
            }) => {
                let blend_src = if let Some(blend_src) = blend_src {
                    Some(self.const_u32(blend_src, &mut ctx.as_const())?.0)
                } else {
                    None
                };

                let mut binding = crate::Binding::Location {
                    location: self.const_u32(location, &mut ctx.as_const())?.0,
                    interpolation,
                    sampling,
                    blend_src,
                };
                binding.apply_default_interpolation(&ctx.module.types[ty].inner);
                Some(binding)
            }
            None => None,
        })
    }

    fn ray_query_pointer(
        &mut self,
        expr: Handle<ast::Expression<'source>>,
        ctx: &mut ExpressionContext<'source, '_, '_>,
    ) -> Result<'source, Handle<crate::Expression>> {
        let span = ctx.ast_expressions.get_span(expr);
        let pointer = self.expression(expr, ctx)?;

        match *resolve_inner!(ctx, pointer) {
            crate::TypeInner::Pointer { base, .. } => match ctx.module.types[base].inner {
                crate::TypeInner::RayQuery { .. } => Ok(pointer),
                ref other => {
                    log::error!("Pointer type to {:?} passed to ray query op", other);
                    Err(Box::new(Error::InvalidRayQueryPointer(span)))
                }
            },
            ref other => {
                log::error!("Type {:?} passed to ray query op", other);
                Err(Box::new(Error::InvalidRayQueryPointer(span)))
            }
        }
    }
}

impl crate::AtomicFunction {
    pub fn map(word: &str) -> Option<Self> {
        Some(match word {
            "atomicAdd" => crate::AtomicFunction::Add,
            "atomicSub" => crate::AtomicFunction::Subtract,
            "atomicAnd" => crate::AtomicFunction::And,
            "atomicOr" => crate::AtomicFunction::InclusiveOr,
            "atomicXor" => crate::AtomicFunction::ExclusiveOr,
            "atomicMin" => crate::AtomicFunction::Min,
            "atomicMax" => crate::AtomicFunction::Max,
            "atomicExchange" => crate::AtomicFunction::Exchange { compare: None },
            _ => return None,
        })
    }
}
