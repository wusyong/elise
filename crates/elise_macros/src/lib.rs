//! `#[derive(GcObject)]`: makes a struct of named fields an object of the
//! GC's heap.
//!
//! For `struct Element { parent: Member<Node>, flags: Cell<u32> }` it
//! generates:
//!
//! - `ElementInit<'gc>`, with the struct's fields and visibility, each of
//!   its field type's `HeapField::Init<'gc>`: an `Option<Gc<'gc, Node>>` for
//!   a `Member<Node>`, the value itself for plain data and an `External`.
//!   `cx.alloc(ElementInit { .. })` writes it in place, and knows the object
//!   from it (`ObjectInit`). A field type that isn't a `HeapField` (`Box`,
//!   `Vec`, a reference) fails here, pointed at the field.
//! - The layout the GC walks: each field's offset and kind, in a static
//!   with the type's index in the heap's type table (assigned on first
//!   use, as cppgc's `GCInfoTrait<T>::Index()`).
//! - An impl of a trait for every `T: Drop` and one for the struct, which
//!   conflict if the struct implements `Drop` (pin-project's way): an
//!   object's only drop glue is its `External`s'.

#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "tests"
    )
)]

use proc_macro::TokenStream;
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

#[proc_macro_derive(GcObject)]
pub fn derive_gc_object(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "a GcObject can't be generic",
        ));
    }
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "only a struct can be a GcObject",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &data.fields,
            "a GcObject's fields must be named",
        ));
    };
    let name = &input.ident;
    let vis = &input.vis;
    let init = format_ident!("{name}Init");
    let names: Vec<_> = fields
        .named
        .iter()
        .filter_map(|field| field.ident.as_ref())
        .collect();
    let types: Vec<_> = fields.named.iter().map(|field| &field.ty).collect();
    let field_vis: Vec<_> = fields.named.iter().map(|field| &field.vis).collect();
    // Each at its field's type, so that a type that can't be a field is
    // pointed at there.
    let inits: Vec<_> = types
        .iter()
        .map(|ty| quote_spanned!(ty.span()=> <#ty as ::elise::HeapField>::Init<'gc>))
        .collect();
    let doc = format!("The fields of a new `{name}`, for `Mutation::alloc`.");
    Ok(quote! {
        #[doc = #doc]
        #vis struct #init<'gc> {
            #( #field_vis #names: #inits, )*
        }

        // SAFETY: the layout lists every field with its offset and its
        // type's kind, and `__init` writes every field at its offset with
        // its type's own writer.
        unsafe impl ::elise::HeapType for #name {
            fn __type() -> &'static ::elise::TypeSlot {
                static TYPE: ::elise::TypeSlot = ::elise::TypeSlot::new(&::elise::TypeInfo::of::<#name>(
                    ::core::stringify!(#name),
                    &[ #( ::elise::FieldInfo {
                        offset: ::core::mem::offset_of!(#name, #names),
                        kind: <#types as ::elise::HeapField>::KIND,
                        main_thread_drop: <#types as ::elise::HeapField>::MAIN_THREAD_DROP,
                    }, )* ],
                ));
                &TYPE
            }
        }

        // SAFETY: as above.
        unsafe impl ::elise::GcObject for #name {
            type Init<'gc> = #init<'gc>;

            fn __init<'gc>(init: #init<'gc>, slot: ::elise::InitSlot<'_, 'gc, Self>) {
                let #init { #( #names, )* } = init;
                #( slot.field::<#types>(::core::mem::offset_of!(#name, #names), #names); )*
            }
        }

        impl<'gc> ::elise::ObjectInit<'gc> for #init<'gc> {
            type Object = #name;
        }

        const _: () = {
            trait MustNotImplDrop {}
            #[allow(drop_bounds)]
            impl<T: ::core::ops::Drop> MustNotImplDrop for T {}
            impl MustNotImplDrop for #name {}
        };
    })
}
