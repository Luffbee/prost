#![doc(html_root_url = "https://docs.rs/prost-derive/0.5.0")]
// The `quote!` macro requires deep recursion.
#![recursion_limit = "4096"]

extern crate proc_macro;

use failure::bail;
use quote::quote;

use failure::Error;
use itertools::Itertools;
use proc_macro::TokenStream;
use proc_macro2::Span;
use syn::{
    punctuated::Punctuated, Data, DataEnum, DataStruct, DeriveInput, Expr, Fields, FieldsNamed,
    FieldsUnnamed, Ident, Variant,
};

mod field;
use crate::field::Field;

fn try_message(input: TokenStream) -> Result<TokenStream, Error> {
    let input: DeriveInput = syn::parse(input)?;

    let ident = input.ident;

    let variant_data = match input.data {
        Data::Struct(variant_data) => variant_data,
        Data::Enum(..) => bail!("Message can not be derived for an enum"),
        Data::Union(..) => bail!("Message can not be derived for a union"),
    };

    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        bail!("Message may not be derived for generic type");
    }

    let fields = match variant_data {
        DataStruct {
            fields: Fields::Named(FieldsNamed { named: fields, .. }),
            ..
        }
        | DataStruct {
            fields:
                Fields::Unnamed(FieldsUnnamed {
                    unnamed: fields, ..
                }),
            ..
        } => fields.into_iter().collect(),
        DataStruct {
            fields: Fields::Unit,
            ..
        } => Vec::new(),
    };

    let mut next_tag: u32 = 0;
    let mut fields = fields
        .into_iter()
        .enumerate()
        .flat_map(|(idx, field)| {
            let field_ident = field
                .ident
                .unwrap_or_else(|| Ident::new(&idx.to_string(), Span::call_site()));
            match Field::new(field.attrs, Some(next_tag)) {
                Ok(Some(field)) => {
                    next_tag = field.tags().iter().max().map(|t| t + 1).unwrap_or(next_tag);
                    Some(Ok((field_ident, field)))
                }
                Ok(None) => None,
                Err(err) => Some(Err(
                    err.context(format!("invalid message field {}.{}", ident, field_ident))
                )),
            }
        })
        .collect::<Result<Vec<(Ident, Field)>, failure::Context<String>>>()?;

    // We want Debug to be in declaration order
    let unsorted_fields = fields.clone();

    // Sort the fields by tag number so that fields will be encoded in tag order.
    // TODO: This encodes oneof fields in the position of their lowest tag,
    // regardless of the currently occupied variant, is that consequential?
    // See: https://developers.google.com/protocol-buffers/docs/encoding#order
    fields.sort_by_key(|&(_, ref field)| field.tags().into_iter().min().unwrap());
    let fields = fields;

    let mut tags = fields
        .iter()
        .flat_map(|&(_, ref field)| field.tags())
        .collect::<Vec<_>>();
    let num_tags = tags.len();
    tags.sort();
    tags.dedup();
    if tags.len() != num_tags {
        bail!("message {} has fields with duplicate tags", ident);
    }

    // Put impls in a const, so that 'extern crate' can be used.
    let dummy_const = Ident::new(&format!("{}_MESSAGE", ident), Span::call_site());

    let encoded_len = fields
        .iter()
        .map(|&(ref field_ident, ref field)| field.encoded_len(quote!(self.#field_ident)));

    let encode = fields
        .iter()
        .map(|&(ref field_ident, ref field)| field.encode(quote!(self.#field_ident)));

    let merge = fields.iter().map(|&(ref field_ident, ref field)| {
        let merge = field.merge(quote!(self.#field_ident));
        let tags = field
            .tags()
            .into_iter()
            .map(|tag| quote!(#tag))
            .intersperse(quote!(|));
        quote!(#(#tags)* => #merge.map_err(|mut error| {
            error.push(STRUCT_NAME, stringify!(#field_ident));
            error
        }),)
    });

    let struct_name = if fields.is_empty() {
        quote!()
    } else {
        quote!(
            const STRUCT_NAME: &'static str = stringify!(#ident);
        )
    };

    // TODO
    let is_struct = true;

    let clear = fields
        .iter()
        .map(|&(ref field_ident, ref field)| field.clear(quote!(self.#field_ident)));

    let default = fields.iter().map(|&(ref field_ident, ref field)| {
        let value = field.default();
        quote!(#field_ident: #value,)
    });

    let methods = fields
        .iter()
        .flat_map(|&(ref field_ident, ref field)| field.methods(field_ident))
        .collect::<Vec<_>>();
    let methods = if methods.is_empty() {
        quote!()
    } else {
        quote! {
            #[allow(dead_code)]
            impl #ident {
                #(#methods)*
            }
        }
    };

    let debugs = unsorted_fields.iter().map(|&(ref field_ident, ref field)| {
        let wrapper = field.debug(quote!(self.#field_ident));
        let call = if is_struct {
            quote!(builder.field(stringify!(#field_ident), &wrapper))
        } else {
            quote!(builder.field(&wrapper))
        };
        quote! {
             let builder = {
                 let wrapper = #wrapper;
                 #call
             };
        }
    });
    let debug_builder = if is_struct {
        quote!(f.debug_struct(stringify!(#ident)))
    } else {
        quote!(f.debug_tuple(stringify!(#ident)))
    };

    let expanded = quote! {
        #[allow(non_snake_case, unused_attributes)]
        const #dummy_const: () = {
            extern crate prost as _prost;
            extern crate bytes as _bytes;

            impl _prost::Message for #ident {
                #[allow(unused_variables)]
                fn encode_raw<B>(&self, buf: &mut B) where B: _bytes::BufMut {
                    #(#encode)*
                }

                #[allow(unused_variables)]
                fn merge_field<B>(&mut self,
                                  tag: u32,
                                  wire_type: _prost::encoding::WireType,
                                  buf: &mut B,
                                  ctx: _prost::encoding::DecodeContext,
                ) -> ::std::result::Result<(), _prost::DecodeError>
                where B: _bytes::Buf {
                    #struct_name
                    match tag {
                        #(#merge)*
                        _ => _prost::encoding::skip_field(wire_type, tag, buf),
                    }
                }

                #[inline]
                fn encoded_len(&self) -> usize {
                    0 #(+ #encoded_len)*
                }

                fn clear(&mut self) {
                    #(#clear;)*
                }
            }

            impl Default for #ident {
                fn default() -> #ident {
                    #ident {
                        #(#default)*
                    }
                }
            }

            impl ::std::fmt::Debug for #ident {
                fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                    let mut builder = #debug_builder;
                    #(#debugs;)*
                    builder.finish()
                }
            }

            #methods
        };
    };

    Ok(expanded.into())
}

#[proc_macro_derive(Message, attributes(prost))]
pub fn message(input: TokenStream) -> TokenStream {
    try_message(input).unwrap()
}

fn try_enumeration(input: TokenStream) -> Result<TokenStream, Error> {
    let input: DeriveInput = syn::parse(input)?;
    let ident = input.ident;

    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        bail!("Message may not be derived for generic type");
    }

    let punctuated_variants = match input.data {
        Data::Enum(DataEnum { variants, .. }) => variants,
        Data::Struct(_) => bail!("Enumeration can not be derived for a struct"),
        Data::Union(..) => bail!("Enumeration can not be derived for a union"),
    };

    // Map the variants into 'fields'.
    let mut variants: Vec<(Ident, Expr)> = Vec::new();
    for Variant {
        ident,
        fields,
        discriminant,
        ..
    } in punctuated_variants
    {
        match fields {
            Fields::Unit => (),
            Fields::Named(_) | Fields::Unnamed(_) => {
                bail!("Enumeration variants may not have fields")
            }
        }

        match discriminant {
            Some((_, expr)) => variants.push((ident, expr)),
            None => bail!("Enumeration variants must have a disriminant"),
        }
    }

    if variants.is_empty() {
        panic!("Enumeration must have at least one variant");
    }

    let default = variants[0].0.clone();

    // Put impls in a const, so that 'extern crate' can be used.
    let dummy_const = Ident::new(&format!("{}_ENUMERATION", ident), Span::call_site());
    let is_valid = variants
        .iter()
        .map(|&(_, ref value)| quote!(#value => true));
    let from = variants.iter().map(
        |&(ref variant, ref value)| quote!(#value => ::std::option::Option::Some(#ident::#variant)),
    );

    let is_valid_doc = format!("Returns `true` if `value` is a variant of `{}`.", ident);
    let from_i32_doc = format!(
        "Converts an `i32` to a `{}`, or `None` if `value` is not a valid variant.",
        ident
    );

    let expanded = quote! {
        #[allow(non_snake_case, unused_attributes)]
        const #dummy_const: () = {
            impl #ident {

                #[doc=#is_valid_doc]
                pub fn is_valid(value: i32) -> bool {
                    match value {
                        #(#is_valid,)*
                        _ => false,
                    }
                }

                #[doc=#from_i32_doc]
                pub fn from_i32(value: i32) -> ::std::option::Option<#ident> {
                    match value {
                        #(#from,)*
                        _ => ::std::option::Option::None,
                    }
                }
            }

            impl ::std::default::Default for #ident {
                fn default() -> #ident {
                    #ident::#default
                }
            }

            impl ::std::convert::From<#ident> for i32 {
                fn from(value: #ident) -> i32 {
                    value as i32
                }
            }
        };
    };

    Ok(expanded.into())
}

#[proc_macro_derive(Enumeration, attributes(prost))]
pub fn enumeration(input: TokenStream) -> TokenStream {
    try_enumeration(input).unwrap()
}

fn try_oneof(input: TokenStream) -> Result<TokenStream, Error> {
    let input: DeriveInput = syn::parse(input)?;

    let ident = input.ident;

    let variants = match input.data {
        Data::Enum(DataEnum { variants, .. }) => variants,
        Data::Struct(..) => bail!("Oneof can not be derived for a struct"),
        Data::Union(..) => bail!("Oneof can not be derived for a union"),
    };

    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        bail!("Message may not be derived for generic type");
    }

    // Map the variants into 'fields'.
    let mut fields: Vec<(Ident, Field)> = Vec::new();
    for Variant {
        attrs,
        ident: variant_ident,
        fields: variant_fields,
        ..
    } in variants
    {
        let variant_fields = match variant_fields {
            Fields::Unit => Punctuated::new(),
            Fields::Named(FieldsNamed { named: fields, .. })
            | Fields::Unnamed(FieldsUnnamed {
                unnamed: fields, ..
            }) => fields,
        };
        if variant_fields.len() != 1 {
            bail!("Oneof enum variants must have a single field");
        }
        match Field::new_oneof(attrs)? {
            Some(field) => fields.push((variant_ident, field)),
            None => bail!("invalid oneof variant: oneof variants may not be ignored"),
        }
    }

    let mut tags = fields
        .iter()
        .flat_map(|&(ref variant_ident, ref field)| -> Result<u32, Error> {
            if field.tags().len() > 1 {
                bail!(
                    "invalid oneof variant {}::{}: oneof variants may only have a single tag",
                    ident,
                    variant_ident
                );
            }
            Ok(field.tags()[0])
        })
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    if tags.len() != fields.len() {
        panic!("invalid oneof {}: variants have duplicate tags", ident);
    }

    // Put impls in a const, so that 'extern crate' can be used.
    let dummy_const = Ident::new(&format!("{}_ONEOF", ident), Span::call_site());

    let encode = fields.iter().map(|&(ref variant_ident, ref field)| {
        let encode = field.encode(quote!(*value));
        quote!(#ident::#variant_ident(ref value) => { #encode })
    });

    let merge = fields.iter().map(|&(ref variant_ident, ref field)| {
        let tag = field.tags()[0];
        let merge = field.merge(quote!(value));
        quote! {
            #tag => {
                let mut value = ::std::default::Default::default();
                #merge.map(|_| *field = ::std::option::Option::Some(#ident::#variant_ident(value)))
            }
        }
    });

    let encoded_len = fields.iter().map(|&(ref variant_ident, ref field)| {
        let encoded_len = field.encoded_len(quote!(*value));
        quote!(#ident::#variant_ident(ref value) => #encoded_len)
    });

    let debug = fields.iter().map(|&(ref variant_ident, ref field)| {
        let wrapper = field.debug(quote!(*value));
        quote!(#ident::#variant_ident(ref value) => {
            let wrapper = #wrapper;
            f.debug_tuple(stringify!(#variant_ident))
                .field(&wrapper)
                .finish()
        })
    });

    let expanded = quote! {
        #[allow(non_snake_case, unused_attributes)]
        const #dummy_const: () = {
            extern crate bytes as _bytes;
            extern crate prost as _prost;

            impl #ident {
                pub fn encode<B>(&self, buf: &mut B) where B: _bytes::BufMut {
                    match *self {
                        #(#encode,)*
                    }
                }

                pub fn merge<B>(field: &mut ::std::option::Option<#ident>,
                                tag: u32,
                                wire_type: _prost::encoding::WireType,
                                buf: &mut B,
                                ctx: _prost::encoding::DecodeContext,
                ) -> ::std::result::Result<(), _prost::DecodeError>
                where B: _bytes::Buf {
                    match tag {
                        #(#merge,)*
                        _ => unreachable!(concat!("invalid ", stringify!(#ident), " tag: {}"), tag),
                    }
                }

                #[inline]
                pub fn encoded_len(&self) -> usize {
                    match *self {
                        #(#encoded_len,)*
                    }
                }
            }

            impl ::std::fmt::Debug for #ident {
                fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                    match *self {
                        #(#debug,)*
                    }
                }
            }
        };
    };

    Ok(expanded.into())
}

#[proc_macro_derive(Oneof, attributes(prost))]
pub fn oneof(input: TokenStream) -> TokenStream {
    try_oneof(input).unwrap()
}
