mod attr_data;

use proc_macro::TokenStream as TokenStream1;
use proc_macro2::TokenStream as TokenStream2;
use quote::*;
use syn::{parse::Parser, punctuated::Punctuated, spanned::Spanned, Expr, Token};

use attr_data::*;
use syn::ForeignItem;

/// Macro for generating shared symbol thunks procedurally.
///
/// # Safety
///
/// Using an `unwind` friendly abi should be used whenever possible to
/// prevent undefined behavior from occuring should the function panic.
///
/// # Examples
///
/// May currently be used in foreign modules, and foreign functions.
///
///```rust
/// use dylink::*;
/// static FOOBAR: sync::LibLock = sync::LibLock::new(&["foobar.dll"]);
///
/// // foreign module pattern
/// #[dylink(library=FOOBAR)]
/// extern "system-unwind" {
///     fn foo();
/// }
///
/// // foreign function pattern
/// #[dylink(library=FOOBAR)]
/// extern "system-unwind" fn bar();
///```
#[proc_macro_attribute]
pub fn dylink(args: TokenStream1, input: TokenStream1) -> TokenStream1 {
	let punct = Parser::parse2(
		Punctuated::<Expr, Token!(,)>::parse_separated_nonempty,
		args.into(),
	)
	.expect("failed to parse");

	match AttrData::try_from(punct) {
		Ok(attr_data) => {
			if let Ok(foreign_mod) = syn::parse2::<syn::ItemForeignMod>(input.clone().into()) {
				if let Some((_, span)) = attr_data.link_name {
					return syn::Error::new(
						span,
						"`link_name` should be applied to a foreign function",
					)
					.to_compile_error()
					.into();
				}

				let abi = &foreign_mod.abi;
				foreign_mod
					.items
					.iter()
					.map(|item| match item {
						ForeignItem::Fn(fn_item) => {
							parse_fn::<true>(Some(abi), fn_item, &attr_data)
						}
						other => quote!(#abi {#other}),
					})
					.collect::<TokenStream2>()
					.into()
			} else if let Ok(foreign_fn) = syn::parse2::<syn::ForeignItemFn>(input.into()) {
				parse_fn::<false>(foreign_fn.sig.abi.as_ref(), &foreign_fn, &attr_data).into()
			} else {
				panic!("failed to parse");
			}
		}
		Err(e) => syn::Error::into_compile_error(e).into(),
	}
}

fn parse_fn<const IS_MOD_ITEM: bool>(
	abi: Option<&syn::Abi>,
	fn_item: &syn::ForeignItemFn,
	attr_data: &AttrData,
) -> TokenStream2 {
	let abi = abi.to_token_stream();
	let fn_name = fn_item.sig.ident.to_token_stream();
	let vis = fn_item.vis.to_token_stream();
	let output = fn_item.sig.output.to_token_stream();
	let library = match attr_data.library {
		Ok(ref path) => path,
		Err(span) => {
			return syn::Error::new(span, "`link_name` should be applied to a foreign function")
				.to_compile_error()
		}
	};
	// constness makes no sense in this context
	match &fn_item.sig.constness {
		None => (),
		Some(kw) => {
			return syn::Error::new(kw.span(), "`const` functions are unsupported")
				.into_compile_error()
		}
	}

	let fn_attrs: Vec<TokenStream2> = fn_item
		.attrs
		.iter()
		.map(syn::Attribute::to_token_stream)
		.collect();

	// `self` can be used, but not inferred, so it's conditionally useful.
	if let syn::ReturnType::Type(_, ret_type) = &fn_item.sig.output {
		if let syn::Type::Path(syn::TypePath { path, .. }) = ret_type.as_ref() {
			if path.is_ident("Self") {
				return syn::Error::new(
					path.span(),
					"`Self` cannot be inferred. Try using an explicit type instead",
				)
				.to_compile_error();
			}
		}
	}

	let mut param_list = Vec::new();
	let mut param_ty_list = Vec::new();
	let mut internal_param_ty_list = Vec::new();
	let mut internal_param_list = Vec::new();
	for (i, arg) in fn_item.sig.inputs.iter().enumerate() {
		match arg {
			syn::FnArg::Typed(pat_type) => {
				if let syn::Type::Path(syn::TypePath { path, .. }) = pat_type.ty.as_ref() {
					if path.is_ident("Self") {
						return syn::Error::new(
							path.span(),
							"`Self` cannot be inferred. Try using an explicit type instead",
						)
						.to_compile_error();
					}
				}
				let ty = pat_type.ty.to_token_stream();
				let param_name = match pat_type.pat.as_ref() {
					syn::Pat::Wild(_) => format!("p{i}").parse::<TokenStream2>().unwrap(),
					syn::Pat::Ident(pat_id) => pat_id.ident.to_token_stream(),
					_ => unreachable!(),
				};
				param_list.push(param_name.clone());
				internal_param_list.push(param_name.clone());
				param_ty_list.push(quote!(#param_name : #ty));
				internal_param_ty_list.push(quote!(#param_name : #ty));
			}
			syn::FnArg::Receiver(rec) => {
				if IS_MOD_ITEM {
					// TODO: fix error message
					return syn::Error::new(
						rec.span(),
						"`self` arguments are unsupported in this context",
					)
					.into_compile_error();
				} else {
					if let syn::Type::Path(syn::TypePath { path, .. }) = rec.ty.as_ref() {
						if path.is_ident("Self") {
							return syn::Error::new(
								path.span(),
								"type of `self` cannot be inferred. Try using an explicit type instead",
							)
							.to_compile_error();
						}
					}
					let ty = rec.ty.to_token_stream();
					let param_name = format!("p{i}").parse::<TokenStream2>().unwrap();
					param_list.push(quote! {self});
					internal_param_list.push(param_name.clone());
					param_ty_list.push(quote!(self : #ty));
					internal_param_ty_list.push(quote!(#param_name : #ty));
				}
			}
		}
	}

	let lint;
	let link_name = match &attr_data.link_name {
		Some((name, _)) => {
			lint = TokenStream2::default();
			name.clone()
		}
		None => {
			lint = quote! {#[allow(non_snake_case)]};
			fn_name.to_string()
		}
	};

	// This is mainly useful for applying lifetimes.
	let generics = &fn_item.sig.generics;

	// variadic compatible ABIs can use this
	let variadic = match &fn_item.sig.variadic {
		None => TokenStream2::default(),
		Some(token) => quote!(, #token),
	};


	if let Some(token) = &fn_item.sig.asyncness {
		return syn::Error::new(
			token.span(),
			"functions in `extern` blocks cannot have qualifiers",
		)
		.into_compile_error();
	}

	if let Some(token) = &fn_item.sig.unsafety {
		return syn::Error::new(
			token.span(),
			"functions in `extern` blocks cannot have qualifiers",
		)
		.into_compile_error();
	}

	// According to "The Rustonomicon" foreign functions are assumed unsafe,
	// so functions are implicitly prepended with `unsafe`
	quote! {
		#(#fn_attrs)*
		#lint
		#[inline]
		#vis unsafe #abi fn #generics #fn_name (#(#param_ty_list),* #variadic) #output {
			use ::std::sync::atomic::{AtomicPtr, Ordering};
			static FUNC: AtomicPtr<::std::ffi::c_void> = AtomicPtr::new(
				initializer as *mut _
			);

			unsafe #abi fn initializer #generics (#(#internal_param_ty_list),* #variadic) #output {
				let symbol = ::dylink::sync::LibLock::symbol(&#library, #link_name)
					.expect(&format!("Dylink Error: failed to load `{}`", stringify!(#fn_name)));
				FUNC.store(symbol.cast_mut().cast(), Ordering::Relaxed);
				let pfn: #abi fn (#(#internal_param_ty_list),*) #output = ::std::mem::transmute(symbol);
				pfn(#(#internal_param_list),*)
			}

			let symbol = FUNC.load(Ordering::Relaxed);
			::std::sync::atomic::compiler_fence(Ordering::Acquire);
			let pfn : #abi fn (#(#internal_param_ty_list),*) #output = ::std::mem::transmute(symbol);
			pfn(#(#param_list),*)
		}
	}
}
