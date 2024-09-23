use std::{collections::HashSet, iter};

use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use syn::{
    parenthesized,
    parse::{Parse, ParseStream},
    parse_quote,
    punctuated::{Pair, Punctuated},
    spanned::Spanned,
    token::Paren,
    AngleBracketedGenericArguments, Block, Expr, ExprBlock, ExprLet, ExprPath, FnArg,
    GenericArgument, Meta, Pat, PatIdent, PatType, Path, PathArguments, PathSegment, Receiver,
    ReturnType, Signature, Stmt, Token, Type, TypeGroup, TypePath, TypeTuple,
};

#[derive(Debug)]
pub struct TurboFn {
    /// Identifier of the exposed function (same as the original function's name).
    ident: Ident,
    output: Type,
    this: Option<Input>,
    inputs: Vec<Input>,
    /// Should we check that the return type contains a `ResolvedValue`?
    resolved: Option<Span>,
    /// Should this function use `TaskPersistence::LocalCells`?
    local_cells: bool,

    /// Signature for the "inline" function. The inline function is the function with minimal
    /// changes that's called by the turbo-tasks framework during scheduling.
    ///
    /// This is in contrast to the "exposed" function, which is the public function that the user
    /// should call.
    ///
    /// This function signature should match the name given by [`Self::inline_ident`].
    inline_signature: Signature,

    /// Identifier of the inline function (a mangled version of the original function's name).
    inline_ident: Ident,

    /// A minimally wrapped version of the original function block.
    inline_block: Block,
}

#[derive(Debug)]
pub struct Input {
    pub ident: Ident,
    pub ty: Type,
}

impl TurboFn {
    pub fn new(
        orig_signature: &Signature,
        definition_context: DefinitionContext,
        args: FunctionArguments,
        orig_block: Block,
    ) -> Option<TurboFn> {
        if !orig_signature.generics.params.is_empty() {
            orig_signature
                .generics
                .span()
                .unwrap()
                .error(format!(
                    "{} do not support generic parameters",
                    definition_context.function_type(),
                ))
                .emit();
            return None;
        }

        if orig_signature.generics.where_clause.is_some() {
            orig_signature
                .generics
                .where_clause
                .span()
                .unwrap()
                .error(format!(
                    "{} do not support where clauses",
                    definition_context.function_type(),
                ))
                .emit();
            return None;
        }

        let mut raw_inputs = orig_signature.inputs.iter();
        let mut this = None;
        let mut inputs = Vec::with_capacity(raw_inputs.len());

        if let Some(possibly_receiver) = raw_inputs.next() {
            match possibly_receiver {
                FnArg::Receiver(
                    receiver @ Receiver {
                        attrs,
                        self_token,
                        reference,
                        mutability,
                    },
                ) => {
                    if !attrs.is_empty() {
                        receiver
                            .span()
                            .unwrap()
                            .error(format!(
                                "{} do not support attributes on arguments",
                                definition_context.function_type(),
                            ))
                            .emit();
                        return None;
                    }

                    // tt::functions in tt::value_impl can either take self as a typed `self:
                    // Vc<Self>`, or as immutable references `&self`. We must validate against any
                    // other forms of self.

                    let definition_context = match &definition_context {
                        DefinitionContext::NakedFn { .. } => return None,
                        _ => &definition_context,
                    };

                    if !attrs.is_empty() {
                        receiver
                            .span()
                            .unwrap()
                            .error(format!(
                                "{} do not support attributes on self",
                                definition_context.function_type(),
                            ))
                            .emit();
                        return None;
                    }

                    if mutability.is_some() {
                        receiver
                            .span()
                            .unwrap()
                            .error(format!(
                                "{} cannot take self by mutable reference, use &self or self: \
                                 Vc<Self> instead",
                                definition_context.function_type(),
                            ))
                            .emit();
                        return None;
                    }

                    match &reference {
                        None => {
                            receiver
                                .span()
                                .unwrap()
                                .error(format!(
                                    "{} cannot take self by value, use &self or self: Vc<Self> \
                                     instead",
                                    definition_context.function_type(),
                                ))
                                .emit();
                            return None;
                        }
                        Some((_, Some(lifetime))) => {
                            lifetime
                                .span()
                                .unwrap()
                                .error(format!(
                                    "{} cannot take self by reference with a custom lifetime, use \
                                     &self or self: Vc<Self> instead",
                                    definition_context.function_type(),
                                ))
                                .emit();
                            return None;
                        }
                        _ => {}
                    }

                    this = Some(Input {
                        ident: Ident::new("self", self_token.span()),
                        ty: parse_quote! { turbo_tasks::Vc<Self> },
                    });
                }
                FnArg::Typed(typed) => {
                    if !typed.attrs.is_empty() {
                        typed
                            .span()
                            .unwrap()
                            .error(format!(
                                "{} does not support attributes on arguments",
                                definition_context.function_type(),
                            ))
                            .emit();
                        return None;
                    }

                    if let Pat::Ident(ident) = &*typed.pat {
                        if ident.ident == "self" {
                            if let DefinitionContext::NakedFn { .. } = definition_context {
                                // The function is not associated. The compiler will emit an error
                                // on its own.
                                return None;
                            };

                            // We don't validate that the user provided a valid
                            // `turbo_tasks::Vc<Self>` here.
                            // We'll rely on the compiler to emit an error
                            // if the user provided an invalid receiver type

                            let ident = ident.ident.clone();

                            this = Some(Input {
                                ident,
                                ty: parse_quote! { turbo_tasks::Vc<Self> },
                            });
                        } else {
                            match definition_context {
                                DefinitionContext::NakedFn { .. }
                                | DefinitionContext::ValueInherentImpl { .. } => {}
                                DefinitionContext::ValueTraitImpl { .. }
                                | DefinitionContext::ValueTrait { .. } => {
                                    typed
                                        .span()
                                        .unwrap()
                                        .error(format!(
                                            "{} must accept &self or self: Vc<Self> as the first \
                                             argument",
                                            definition_context.function_type(),
                                        ))
                                        .emit();
                                    return None;
                                }
                            }
                            let ident = ident.ident.clone();

                            inputs.push(Input {
                                ident,
                                ty: (*typed.ty).clone(),
                            });
                        }
                    } else {
                        // We can't support destructuring patterns (or other kinds of patterns).
                        let ident = Ident::new("arg1", typed.pat.span());

                        inputs.push(Input {
                            ident,
                            ty: (*typed.ty).clone(),
                        });
                    }
                }
            }
        }

        for (i, input) in raw_inputs.enumerate() {
            match input {
                FnArg::Receiver(_) => {
                    // The compiler will emit an error on its own.
                    return None;
                }
                FnArg::Typed(typed) => {
                    let ident = if let Pat::Ident(ident) = &*typed.pat {
                        ident.ident.clone()
                    } else {
                        Ident::new(&format!("arg{}", i + 2), typed.pat.span())
                    };

                    inputs.push(Input {
                        ident,
                        ty: (*typed.ty).clone(),
                    });
                }
            }
        }

        let output = return_type_to_type(&orig_signature.output);

        let original_ident = &orig_signature.ident;
        let inline_ident = Ident::new(
            // Hygiene: This should use `.resolved_at(Span::def_site())`, but that's unstable, so
            // instead we just pick a long, unique name
            &format!("{original_ident}_turbo_tasks_function_inline"),
            original_ident.span(),
        );
        let inline_signature = Signature {
            ident: inline_ident.clone(),
            inputs: orig_signature
                .inputs
                .iter()
                .enumerate()
                .map(|(idx, arg)| match arg {
                    FnArg::Receiver(_) => arg.clone(),
                    FnArg::Typed(pat_type) => {
                        // arbitrary self types aren't `FnArg::Receiver` on syn 1.x (fixed in 2.x)
                        if let Pat::Ident(pat_id) = &*pat_type.pat {
                            // TODO: Support `self: ResolvedVc<Self>`
                            if pat_id.ident == "self" {
                                return arg.clone();
                            }
                        }
                        FnArg::Typed(PatType {
                            pat: Box::new(Pat::Ident(PatIdent {
                                attrs: Vec::new(),
                                by_ref: None,
                                mutability: None,
                                ident: Ident::new(&format!("arg{idx}"), pat_type.pat.span()),
                                subpat: None,
                            })),
                            ty: Box::new(expand_task_input_type(&pat_type.ty)),
                            ..pat_type.clone()
                        })
                    }
                })
                .collect(),
            ..orig_signature.clone()
        };

        let inline_block = {
            let stmts: Vec<Stmt> = orig_signature
                .inputs
                .iter()
                .enumerate()
                .filter_map(|(idx, arg)| match arg {
                    FnArg::Receiver(_) => None,
                    FnArg::Typed(pat_type) => {
                        if let Pat::Ident(pat_id) = &*pat_type.pat {
                            // TODO: Support `self: ResolvedVc<Self>`
                            if pat_id.ident == "self" {
                                return None;
                            }
                        }
                        let arg_ident = Ident::new(&format!("arg{idx}"), pat_type.span());
                        let ty = &*pat_type.ty;
                        Some(Stmt::Semi(
                            Expr::Let(ExprLet {
                                attrs: Vec::new(),
                                let_token: Default::default(),
                                pat: *pat_type.pat.clone(),
                                eq_token: Default::default(),
                                expr: parse_quote! {
                                    {
                                        use turbo_tasks::task::FromTaskInput;
                                        turbo_tasks::macro_helpers::AutoFromTaskInput::<#ty>
                                            ::from_task_input(#arg_ident).0
                                    }
                                },
                            }),
                            Default::default(),
                        ))
                    }
                })
                .chain(iter::once(Stmt::Expr(Expr::Block(ExprBlock {
                    attrs: Vec::new(),
                    label: None,
                    block: orig_block,
                }))))
                .collect();
            Block {
                brace_token: Default::default(),
                stmts,
            }
        };

        Some(TurboFn {
            ident: original_ident.clone(),
            output,
            this,
            inputs,
            resolved: args.resolved,
            local_cells: args.local_cells.is_some(),
            inline_signature,
            inline_block,
            inline_ident,
        })
    }

    /// The signature of the exposed function. This is the original signature
    /// converted to a standard turbo_tasks function signature.
    pub fn signature(&self) -> Signature {
        let exposed_inputs: Punctuated<_, Token![,]> = self
            .this
            .as_ref()
            .into_iter()
            .chain(self.inputs.iter())
            .map(|input| {
                FnArg::Typed(PatType {
                    attrs: Vec::new(),
                    pat: Box::new(Pat::Ident(PatIdent {
                        attrs: Default::default(),
                        by_ref: None,
                        mutability: None,
                        ident: input.ident.clone(),
                        subpat: None,
                    })),
                    colon_token: Default::default(),
                    ty: Box::new(expand_task_input_type(&input.ty)),
                })
            })
            .collect();

        let ident = &self.ident;
        let orig_output = &self.output;
        let new_output = expand_vc_return_type(orig_output);

        parse_quote! {
            fn #ident(#exposed_inputs) -> #new_output
        }
    }

    pub fn trait_signature(&self) -> Signature {
        let signature = self.signature();

        parse_quote! {
            #signature where Self: Sized
        }
    }

    pub fn inline_signature(&self) -> &Signature {
        &self.inline_signature
    }

    pub fn inline_ident(&self) -> &Ident {
        &self.inline_ident
    }

    pub fn inline_block(&self) -> &Block {
        &self.inline_block
    }

    fn input_idents(&self) -> impl Iterator<Item = &Ident> {
        self.inputs.iter().map(|Input { ident, .. }| ident)
    }

    pub fn input_types(&self) -> Vec<&Type> {
        self.inputs.iter().map(|Input { ty, .. }| ty).collect()
    }

    pub fn persistence(&self) -> impl ToTokens {
        if self.local_cells {
            quote! {
                turbo_tasks::TaskPersistence::LocalCells
            }
        } else {
            quote! {
                turbo_tasks::macro_helpers::get_non_local_persistence_from_inputs(&*inputs)
            }
        }
    }

    pub fn persistence_with_this(&self) -> impl ToTokens {
        if self.local_cells {
            quote! {
                turbo_tasks::TaskPersistence::LocalCells
            }
        } else {
            quote! {
                turbo_tasks::macro_helpers::get_non_local_persistence_from_inputs_and_this(this, &*inputs)
            }
        }
    }

    fn converted_this(&self) -> Option<Expr> {
        self.this.as_ref().map(|Input { ty: _, ident }| {
            parse_quote! {
                turbo_tasks::Vc::into_raw(#ident)
            }
        })
    }

    fn get_assertions(&self) -> TokenStream {
        if let Some(span) = self.resolved {
            let return_type = &self.output;
            quote_spanned! {
                span =>
                {
                    turbo_tasks::macro_helpers::assert_returns_resolved_value::<#return_type, _>()
                }
            }
        } else {
            quote! {}
        }
    }

    /// The block of the exposed function for a dynamic dispatch call to the
    /// given trait.
    pub fn dynamic_block(&self, trait_type_id_ident: &Ident) -> Block {
        let Some(converted_this) = self.converted_this() else {
            return parse_quote! {
                {
                    unimplemented!("trait methods without self are not yet supported")
                }
            };
        };

        let ident = &self.ident;
        let output = &self.output;
        let assertions = self.get_assertions();
        let inputs = self.input_idents();
        let persistence = self.persistence_with_this();
        parse_quote! {
            {
                #assertions
                let inputs = std::boxed::Box::new((#(#inputs,)*));
                let this = #converted_this;
                let persistence = #persistence;
                <#output as turbo_tasks::task::TaskOutput>::try_from_raw_vc(
                    turbo_tasks::trait_call(
                        *#trait_type_id_ident,
                        std::borrow::Cow::Borrowed(stringify!(#ident)),
                        this,
                        inputs as std::boxed::Box<dyn turbo_tasks::MagicAny>,
                        persistence,
                    )
                )
            }
        }
    }

    /// The block of the exposed function for a static dispatch call to the
    /// given native function.
    pub fn static_block(&self, native_function_id_ident: &Ident) -> Block {
        let output = &self.output;
        let inputs = self.input_idents();
        let assertions = self.get_assertions();
        if let Some(converted_this) = self.converted_this() {
            let persistence = self.persistence_with_this();
            parse_quote! {
                {
                    #assertions
                    let inputs = std::boxed::Box::new((#(#inputs,)*));
                    let this = #converted_this;
                    let persistence = #persistence;
                    <#output as turbo_tasks::task::TaskOutput>::try_from_raw_vc(
                        turbo_tasks::dynamic_this_call(
                            *#native_function_id_ident,
                            this,
                            inputs as std::boxed::Box<dyn turbo_tasks::MagicAny>,
                            persistence,
                        )
                    )
                }
            }
        } else {
            let persistence = self.persistence();
            parse_quote! {
                {
                    #assertions
                    let inputs = std::boxed::Box::new((#(#inputs,)*));
                    let persistence = #persistence;
                    <#output as turbo_tasks::task::TaskOutput>::try_from_raw_vc(
                        turbo_tasks::dynamic_call(
                            *#native_function_id_ident,
                            inputs as std::boxed::Box<dyn turbo_tasks::MagicAny>,
                            persistence,
                        )
                    )
                }
            }
        }
    }

    pub(crate) fn is_method(&self) -> bool {
        self.this.is_some()
    }
}

/// An indication of what kind of IO this function does. Currently only used for
/// static analysis, and ignored within this macro.
#[derive(Hash, PartialEq, Eq)]
enum IoMarker {
    Filesystem,
    Network,
}

/// Unwraps a parenthesized set of tokens.
///
/// Syn's lower-level [`parenthesized`] macro which this uses requires a
/// [`ParseStream`] and cannot be used with [`parse_macro_input`],
/// [`syn::parse2`] or anything else accepting a [`TokenStream`]. This can be
/// used with those [`TokenStream`]-based parsing APIs.
pub struct Parenthesized<T: Parse> {
    pub _paren_token: Paren,
    pub inner: T,
}

impl<T: Parse> Parse for Parenthesized<T> {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let inner;
        Ok(Self {
            _paren_token: parenthesized!(inner in input),
            inner: <T>::parse(&inner)?,
        })
    }
}

/// A newtype wrapper for [`Option<Parenthesized>`][Parenthesized] that
/// implements [`Parse`].
pub struct MaybeParenthesized<T: Parse> {
    pub parenthesized: Option<Parenthesized<T>>,
}

impl<T: Parse> Parse for MaybeParenthesized<T> {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(Self {
            parenthesized: if input.peek(Paren) {
                Some(Parenthesized::<T>::parse(input)?)
            } else {
                None
            },
        })
    }
}

/// Arguments to the `#[turbo_tasks::function]` macro.
#[derive(Default)]
pub struct FunctionArguments {
    /// Manually annotated metadata about what kind of IO this function does. Currently only used
    /// by some static analysis tools. May be exposed via `tracing` or used as part of an
    /// optimization heuristic in the future.
    ///
    /// This should only be used by the task that directly performs the IO. Tasks that transitively
    /// perform IO should not be manually annotated.
    io_markers: HashSet<IoMarker>,
    /// Should we check that the return type contains a `ResolvedValue`?
    ///
    /// If there is an error due to this option being set, it should be reported to this span.
    ///
    /// If [`Self::local_cells`] is set, this will also be set to the same span.
    resolved: Option<Span>,
    /// Changes the behavior of `Vc::cell` to create local cells that are not cached across task
    /// executions. Cells can be converted to their non-local versions by calling `Vc::resolve`.
    ///
    /// If there is an error due to this option being set, it should be reported to this span.
    ///
    /// Setting this option will also set [`Self::resolved`] to the same span.
    pub local_cells: Option<Span>,
}

impl Parse for FunctionArguments {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut parsed_args = FunctionArguments::default();
        let punctuated: Punctuated<Meta, Token![,]> = input.parse_terminated(Meta::parse)?;
        for meta in punctuated {
            match (
                meta.path()
                    .get_ident()
                    .map(ToString::to_string)
                    .as_deref()
                    .unwrap_or_default(),
                &meta,
            ) {
                ("fs", Meta::Path(_)) => {
                    parsed_args.io_markers.insert(IoMarker::Filesystem);
                }
                ("network", Meta::Path(_)) => {
                    parsed_args.io_markers.insert(IoMarker::Network);
                }
                ("resolved", Meta::Path(_)) => {
                    parsed_args.resolved = Some(meta.span());
                }
                ("local_cells", Meta::Path(_)) => {
                    let span = Some(meta.span());
                    parsed_args.local_cells = span;
                    parsed_args.resolved = span;
                }
                (_, meta) => {
                    return Err(syn::Error::new_spanned(
                        meta,
                        "unexpected token, expected one of: \"fs\", \"network\", \"resolved\", \
                         \"local_cells\"",
                    ))
                }
            }
        }
        Ok(parsed_args)
    }
}

fn return_type_to_type(return_type: &ReturnType) -> Type {
    match return_type {
        ReturnType::Default => parse_quote! { () },
        ReturnType::Type(_, ref return_type) => (**return_type).clone(),
    }
}

/// Approximates the conversion of type `T` to `<T as FromTaskInput>::TaskInput` (in combination
/// with the `AutoFromTaskInput` specialization hack).
///
/// This expansion happens manually here for a couple reasons:
/// - While it's possible to simulate specialization of methods (with inherent impls, autoref, or
///   autoderef) there's currently no way to simulate specialization of type aliases on stable rust.
/// - Replacing arguments with types like `<T as FromTaskInput>::TaskInput` would make function
///   signatures illegible in the resulting rustdocs.
fn expand_task_input_type(orig_input: &Type) -> Type {
    match orig_input {
        Type::Group(TypeGroup { elem, .. }) => expand_task_input_type(elem),
        Type::Path(TypePath {
            qself: None,
            path: Path {
                leading_colon,
                segments,
            },
        }) => {
            enum PathMatch {
                Empty,
                StdMod,
                VecMod,
                Vec,
                OptionMod,
                Option,
                TurboTasksMod,
                ResolvedVc,
            }

            let mut path_match = PathMatch::Empty;
            let has_leading_colon = leading_colon.is_some();
            for segment in segments {
                path_match = match (has_leading_colon, path_match, &segment.ident) {
                    (_, PathMatch::Empty, id) if id == "std" || id == "core" || id == "alloc" => {
                        PathMatch::StdMod
                    }

                    (_, PathMatch::StdMod, id) if id == "vec" => PathMatch::VecMod,
                    (false, PathMatch::Empty | PathMatch::VecMod, id) if id == "Vec" => {
                        PathMatch::Vec
                    }

                    (_, PathMatch::StdMod, id) if id == "option" => PathMatch::OptionMod,
                    (false, PathMatch::Empty | PathMatch::OptionMod, id) if id == "Option" => {
                        PathMatch::Option
                    }

                    (_, PathMatch::Empty, id) if id == "turbo_tasks" => PathMatch::TurboTasksMod,
                    (false, PathMatch::Empty | PathMatch::TurboTasksMod, id)
                        if id == "ResolvedVc" =>
                    {
                        PathMatch::ResolvedVc
                    }

                    // some type we don't have an expansion for
                    _ => return orig_input.clone(),
                }
            }

            let mut segments = segments.clone();
            let last_segment = segments.last_mut().expect("segments is non-empty");
            match path_match {
                PathMatch::Vec | PathMatch::Option => {
                    if let PathArguments::AngleBracketed(AngleBracketedGenericArguments {
                        args,
                        ..
                    }) = &mut last_segment.arguments
                    {
                        if let Some(GenericArgument::Type(first_arg)) = args.first_mut() {
                            *first_arg = expand_task_input_type(first_arg);
                        }
                    }
                }
                PathMatch::ResolvedVc => {
                    last_segment.ident = Ident::new("Vc", last_segment.ident.span())
                }
                _ => {}
            }
            Type::Path(TypePath {
                qself: None,
                path: Path {
                    leading_colon: *leading_colon,
                    segments,
                },
            })
        }
        _ => orig_input.clone(),
    }
}

fn expand_vc_return_type(orig_output: &Type) -> Type {
    // HACK: Approximate the expansion that we'd otherwise get from
    // `<T as TaskOutput>::Return`, so that the return type shown in the rustdocs
    // is as simple as possible. Break out as soon as we see something we don't
    // recognize.
    let mut new_output = orig_output.clone();
    let mut found_vc = false;
    loop {
        new_output = match new_output {
            Type::Group(TypeGroup { elem, .. }) => *elem,
            Type::Tuple(TypeTuple { elems, .. }) if elems.is_empty() => {
                Type::Path(parse_quote!(turbo_tasks::Vc<()>))
            }
            Type::Path(TypePath {
                qself: None,
                path:
                    Path {
                        leading_colon,
                        ref segments,
                    },
            }) => {
                let mut pairs = segments.pairs();
                let mut cur_pair = pairs.next();

                enum PathPrefix {
                    Anyhow,
                    TurboTasks,
                }

                // try to strip a `turbo_tasks::` or `anyhow::` prefix
                let prefix = if let Some(first) = cur_pair.as_ref().map(|p| p.value()) {
                    if first.arguments.is_none() {
                        if first.ident == "turbo_tasks" {
                            Some(PathPrefix::TurboTasks)
                        } else if first.ident == "anyhow" {
                            Some(PathPrefix::Anyhow)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if prefix.is_some() {
                    cur_pair = pairs.next(); // strip the matched prefix
                } else if leading_colon.is_some() {
                    break; // something like `::Vc` isn't valid
                }

                // Look for a `Vc<...>` or `Result<...>` generic
                let Some(Pair::End(PathSegment {
                    ident,
                    arguments:
                        PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }),
                })) = cur_pair
                else {
                    break;
                };
                if ident == "Vc" {
                    found_vc = true;
                    break; // Vc is the bottom-most level
                }
                if ident == "Result" && args.len() == 1 {
                    let GenericArgument::Type(ty) =
                        args.first().expect("Result<...> type has an argument")
                    else {
                        break;
                    };
                    ty.clone()
                } else {
                    break; // we only support expanding Result<...>
                }
            }
            _ => break,
        }
    }

    if !found_vc {
        orig_output
            .span()
            .unwrap()
            .error(
                "Expected return type to be `turbo_tasks::Vc<T>` or `anyhow::Result<Vc<T>>`. \
                 Unable to process type.",
            )
            .emit();
    }

    new_output
}

/// The context in which the function is being defined.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DefinitionContext {
    // The function is defined as a naked #[turbo_tasks::function].
    NakedFn,
    // The function is defined as a #[turbo_tasks::value_impl] inherent implementation method.
    ValueInherentImpl,
    // The function is defined as a #[turbo_tasks::value_impl] trait implementation method.
    ValueTraitImpl,
    // The function is defined as a #[turbo_tasks::value_trait] default method.
    ValueTrait,
}

impl DefinitionContext {
    pub fn function_type(&self) -> &'static str {
        match self {
            DefinitionContext::NakedFn => "#[turbo_tasks::function] naked functions",
            DefinitionContext::ValueInherentImpl => "#[turbo_tasks::value_impl] inherent methods",
            DefinitionContext::ValueTraitImpl => "#[turbo_tasks::value_impl] trait methods",
            DefinitionContext::ValueTrait => "#[turbo_tasks::value_trait] methods",
        }
    }
}

#[derive(Debug)]
pub struct NativeFn {
    function_path_string: String,
    function_path: ExprPath,
    is_method: bool,
    local_cells: bool,
}

impl NativeFn {
    pub fn new(
        function_path_string: &str,
        function_path: &ExprPath,
        is_method: bool,
        local_cells: bool,
    ) -> NativeFn {
        NativeFn {
            function_path_string: function_path_string.to_owned(),
            function_path: function_path.clone(),
            is_method,
            local_cells,
        }
    }

    pub fn ty(&self) -> Type {
        parse_quote! { turbo_tasks::macro_helpers::Lazy<turbo_tasks::NativeFunction> }
    }

    pub fn definition(&self) -> Expr {
        let Self {
            function_path_string,
            function_path,
            is_method,
            local_cells,
        } = self;

        let constructor = if *is_method {
            quote! { new_method }
        } else {
            quote! { new_function }
        };

        parse_quote! {
            turbo_tasks::macro_helpers::Lazy::new(|| {
                #[allow(deprecated)]
                turbo_tasks::NativeFunction::#constructor(
                    #function_path_string.to_owned(),
                    turbo_tasks::FunctionMeta {
                        local_cells: #local_cells,
                    },
                    #function_path,
                )
            })
        }
    }

    pub fn id_ty(&self) -> Type {
        parse_quote! { turbo_tasks::macro_helpers::Lazy<turbo_tasks::FunctionId> }
    }

    pub fn id_definition(&self, native_function_id_path: &Path) -> Expr {
        parse_quote! {
            turbo_tasks::macro_helpers::Lazy::new(|| {
                turbo_tasks::registry::get_function_id(&*#native_function_id_path)
            })
        }
    }
}
