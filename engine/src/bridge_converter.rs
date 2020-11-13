// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use crate::{
    additional_cpp_generator::{AdditionalNeed, ArgumentConversion, ByValueWrapper},
    known_types::{replace_type_path_without_arguments, should_dereference_in_cpp},
    types::make_ident,
};
use crate::{
    byvalue_checker::ByValueChecker,
    types::Namespace,
    unqualify::{unqualify_params, unqualify_ret_type},
};
use crate::{
    namespace_organizer::{NamespaceEntries, Use},
    types::TypeName,
};
use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{parse::Parser, ItemType};
use syn::{
    parse_quote, Attribute, FnArg, ForeignItem, ForeignItemFn, GenericArgument, Ident, Item,
    ItemForeignMod, ItemMod, Pat, PathArguments, PathSegment, ReturnType, Type, TypePath, TypePtr,
    TypeReference,
};

#[derive(Debug)]
pub enum ConvertError {
    NoContent,
    UnsafePODType(String),
    UnknownForeignItem,
}

/// Results of a conversion.
pub(crate) struct BridgeConversionResults {
    pub items: Vec<Item>,
    pub additional_cpp_needs: Vec<AdditionalNeed>,
}

/// Converts the bindings generated by bindgen into a form suitable
/// for use with `cxx`.
///
/// Non-exhaustive list of things we do:
/// * Replaces certain identifiers e.g. `std::unique_ptr` with `UniquePtr`
/// * Replaces pointers with references
/// * Removes repr attributes
/// * Removes link_name attributes
/// * Adds include! directives
/// * Adds #[cxx::bridge]
/// In fact, most of the actual operation happens within an
/// individual `BridgeConevrsion`.
/// This mod has grown to be rather unwieldy. It started with much
/// smaller ambitions and is now really the core of `autocxx`. It'll
/// need to be split down into smaller crates at some point. TODO.
///
/// # Flexibility in handling bindgen output
///
/// autocxx is inevitably tied to the details of the bindgen output;
/// e.g. the creation of a 'root' mod when namespaces are enabled.
/// At the moment this crate takes the view that it's OK to panic
/// if the bindgen output is not as expected. It may be in future that
/// we need to be a bit more graceful, but for now, that's OK.
pub(crate) struct BridgeConverter {
    include_list: Vec<String>,
    pod_requests: Vec<TypeName>,
}

impl BridgeConverter {
    pub fn new(include_list: Vec<String>, pod_requests: Vec<TypeName>) -> Self {
        Self {
            include_list,
            pod_requests,
        }
    }

    /// Convert a TokenStream of bindgen-generated bindings to a form
    /// suitable for cxx.
    pub(crate) fn convert(
        &mut self,
        bindings: ItemMod,
        exclude_utilities: bool,
    ) -> Result<BridgeConversionResults, ConvertError> {
        match bindings.content {
            None => Err(ConvertError::NoContent),
            Some((brace, items)) => {
                let bindgen_mod = ItemMod {
                    attrs: bindings.attrs,
                    vis: bindings.vis,
                    ident: bindings.ident,
                    mod_token: bindings.mod_token,
                    content: Some((brace, Vec::new())),
                    semi: bindings.semi,
                };
                let conversion = BridgeConversion {
                    bindgen_mod,
                    all_items: Vec::new(),
                    bridge_items: Vec::new(),
                    extern_c_mod: None,
                    extern_c_mod_items: Vec::new(),
                    additional_cpp_needs: Vec::new(),
                    types_found: Vec::new(),
                    byvalue_checker: ByValueChecker::new(),
                    pod_requests: &self.pod_requests,
                    include_list: &self.include_list,
                    final_uses: Vec::new(),
                    typedefs: HashMap::new(),
                };
                conversion.convert_items(items, exclude_utilities)
            }
        }
    }
}

fn get_blank_extern_c_mod() -> ItemForeignMod {
    parse_quote!(
        extern "C" {}
    )
}

fn type_to_typename(ty: &Type) -> Option<TypeName> {
    match ty {
        Type::Path(pn) => Some(TypeName::from_bindgen_type_path(pn)),
        _ => None,
    }
}

/// Analysis of a typedef.
#[derive(Debug)]
enum TypedefTarget {
    NoArguments(TypeName),
    HasArguments,
    SomethingComplex,
}

/// A particular bridge conversion operation. This can really
/// be thought of as a ton of parameters which we'd otherwise
/// need to pass into each individual function within this file.
struct BridgeConversion<'a> {
    bindgen_mod: ItemMod,
    all_items: Vec<Item>,
    bridge_items: Vec<Item>,
    extern_c_mod: Option<ItemForeignMod>,
    extern_c_mod_items: Vec<ForeignItem>,
    additional_cpp_needs: Vec<AdditionalNeed>,
    types_found: Vec<Ident>,
    byvalue_checker: ByValueChecker,
    pod_requests: &'a Vec<TypeName>,
    include_list: &'a Vec<String>,
    final_uses: Vec<Use>,
    typedefs: HashMap<TypeName, TypedefTarget>,
}

impl<'a> BridgeConversion<'a> {
    /// Main function which goes through and performs conversion from
    /// `bindgen`-style Rust output into `cxx::bridge`-style Rust input.
    fn convert_items(
        mut self,
        items: Vec<Item>,
        exclude_utilities: bool,
    ) -> Result<BridgeConversionResults, ConvertError> {
        if !exclude_utilities {
            self.generate_utilities();
        }
        let mut bindgen_root_items = Vec::new();
        for item in items {
            match item {
                Item::Mod(root_mod) => {
                    // With namespaces enabled, bindgen always puts everything
                    // in a mod called 'root'. We don't want to pass that
                    // onto cxx, so jump right into it.
                    assert!(root_mod.ident == "root");
                    if let Some((_, items)) = root_mod.content {
                        let root_ns = Namespace::new();
                        self.find_nested_pod_types(&items, &root_ns)?;
                        self.convert_mod_items(items, root_ns, &mut bindgen_root_items)?;
                    }
                }
                _ => panic!("Unexpected outer item"),
            }
        }
        self.extern_c_mod_items
            .extend(self.build_include_foreign_items());
        // We will always create an extern "C" mod even if bindgen
        // didn't generate one, e.g. because it only generated types.
        // We still want cxx to know about those types.
        let mut extern_c_mod = self
            .extern_c_mod
            .take()
            .unwrap_or_else(get_blank_extern_c_mod);
        extern_c_mod.items.append(&mut self.extern_c_mod_items);
        self.bridge_items.push(Item::ForeignMod(extern_c_mod));
        bindgen_root_items.push(Item::Use(parse_quote! {
            #[allow(unused_imports)]
            use self::super::super::cxxbridge;
        }));
        // The extensive use of parse_quote here could end up
        // being a performance bottleneck. If so, we might want
        // to set the 'contents' field of the ItemMod
        // structures directly.
        self.bindgen_mod.content.as_mut().unwrap().1 = vec![Item::Mod(parse_quote! {
            pub mod root {
                #(#bindgen_root_items)*
            }
        })];
        self.generate_final_use_statements();
        self.all_items.push(Item::Mod(self.bindgen_mod));
        let bridge_items = &self.bridge_items;
        self.all_items.push(Item::Mod(parse_quote! {
            #[cxx::bridge]
            pub mod cxxbridge {
                #(#bridge_items)*
            }
        }));
        Ok(BridgeConversionResults {
            items: self.all_items,
            additional_cpp_needs: self.additional_cpp_needs,
        })
    }

    fn convert_mod_items(
        &mut self,
        items: Vec<Item>,
        ns: Namespace,
        output_items: &mut Vec<Item>,
    ) -> Result<(), ConvertError> {
        for item in items {
            match item {
                Item::ForeignMod(mut fm) => {
                    let items = fm.items;
                    fm.items = Vec::new();
                    if self.extern_c_mod.is_none() {
                        self.extern_c_mod = Some(fm);
                        // We'll use the first 'extern "C"' mod we come
                        // across for attributes, spans etc. but we'll stuff
                        // the contents of all bindgen 'extern "C"' mods into this
                        // one.
                    }
                    self.convert_foreign_mod_items(items, &ns)?;
                }
                Item::Struct(mut s) => {
                    let tyname = TypeName::new(&ns, &s.ident.to_string());
                    let should_be_pod = self.byvalue_checker.is_pod(&tyname);
                    self.generate_type_alias(tyname, should_be_pod)?;
                    if !should_be_pod {
                        // See cxx's opaque::Opaque for rationale for this type... in
                        // short, it's to avoid being Send/Sync.
                        s.fields = syn::Fields::Named(parse_quote! {
                            {
                                do_not_attempt_to_allocate_nonpod_types: [*const u8; 0],
                            }
                        });
                        // Thanks to dtolnay@ for this explanation of why the following
                        // is needed:
                        // If the real alignment of the C++ type is smaller and a reference
                        // is returned from C++ to Rust, mere existence of an insufficiently
                        // aligned reference in Rust causes UB even if never dereferenced
                        // by Rust code
                        // (see https://doc.rust-lang.org/1.47.0/reference/behavior-considered-undefined.html).
                        // Rustc can use least-significant bits of the reference for other storage.
                        s.attrs = vec![parse_quote!(
                            #[repr(C, packed)]
                        )];
                    }
                    output_items.push(Item::Struct(s));
                }
                Item::Enum(e) => {
                    let tyname = TypeName::new(&ns, &e.ident.to_string());
                    self.generate_type_alias(tyname, true)?;
                    output_items.push(Item::Enum(e));
                }
                Item::Impl(i) => {
                    if let Some(ty) = type_to_typename(&i.self_ty) {
                        for item in i.items.clone() {
                            match item {
                                syn::ImplItem::Method(m) if m.sig.ident == "new" => {
                                    self.convert_new_method(m, &ty, &i, output_items)
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Item::Mod(itm) => {
                    let mut new_itm = itm.clone();
                    if let Some((_, items)) = itm.content {
                        let new_ns = ns.push(itm.ident.to_string());
                        let mut new_items = Vec::new();
                        self.convert_mod_items(items, new_ns, &mut new_items)?;
                        new_itm.content.as_mut().unwrap().1 = new_items;
                    }
                    output_items.push(Item::Mod(new_itm));
                }
                Item::Use(_) => {
                    output_items.push(item);
                }
                Item::Const(_) => {
                    self.all_items.push(item);
                }
                Item::Type(ity) => {
                    if Self::should_ignore_item_type(&ity) {
                        // Ignore this for now. Sometimes bindgen generates such things
                        // without an actual need to do so.
                        continue;
                    }
                    let tyname = TypeName::new(&ns, &ity.ident.to_string());
                    let target = Self::analyze_typedef_target(ity.ty.as_ref());
                    output_items.push(Item::Type(ity));
                    self.typedefs.insert(tyname, target);
                }
                _ => {
                    // TODO it would be nice to enable this, but at the moment
                    // we hit it too often. Also, item is not Debug so
                    // it's a bit annoying trying to work out what's up.
                    //panic!("Unhandled item");
                }
            }
        }
        Ok(())
    }

    fn should_ignore_item_type(ity: &ItemType) -> bool {
        ity.generics.lifetimes().next().is_some()
            || ity.generics.const_params().next().is_some()
            || ity.generics.type_params().next().is_some()
    }

    fn analyze_typedef_target(ty: &Type) -> TypedefTarget {
        match ty {
            Type::Path(typ) => {
                let seg = typ.path.segments.last().unwrap();
                if seg.arguments.is_empty() {
                    TypedefTarget::NoArguments(TypeName::from_bindgen_type_path(typ))
                } else {
                    TypedefTarget::HasArguments
                }
            }
            _ => TypedefTarget::SomethingComplex,
        }
    }

    fn find_nested_pod_types(
        &mut self,
        items: &[Item],
        ns: &Namespace,
    ) -> Result<(), ConvertError> {
        for item in items {
            match item {
                Item::Struct(s) => self.byvalue_checker.ingest_struct(s, ns),
                Item::Enum(e) => self
                    .byvalue_checker
                    .ingest_pod_type(TypeName::new(&ns, &e.ident.to_string())),
                Item::Type(ity) => {
                    if Self::should_ignore_item_type(&ity) {
                        // Ignore this for now. Sometimes bindgen generates such things
                        // without an actual need to do so.
                        continue;
                    }
                    let typedef_type = Self::analyze_typedef_target(ity.ty.as_ref());
                    let name = TypeName::new(ns, &ity.ident.to_string());
                    match typedef_type {
                        TypedefTarget::NoArguments(tn) => {
                            self.byvalue_checker.ingest_simple_typedef(name, tn)
                        }
                        TypedefTarget::HasArguments | TypedefTarget::SomethingComplex => {
                            self.byvalue_checker.ingest_nonpod_type(name)
                        }
                    }
                }
                Item::Mod(itm) => {
                    if let Some((_, nested_items)) = &itm.content {
                        let new_ns = ns.push(itm.ident.to_string());
                        self.find_nested_pod_types(nested_items, &new_ns)?;
                    }
                }
                _ => {}
            }
        }
        self.byvalue_checker
            .satisfy_requests(self.pod_requests.clone())
            .map_err(ConvertError::UnsafePODType)
    }

    fn generate_type_alias(
        &mut self,
        tyname: TypeName,
        should_be_pod: bool,
    ) -> Result<(), ConvertError> {
        let final_ident = make_ident(tyname.get_final_ident());
        let kind_item = make_ident(if should_be_pod { "Trivial" } else { "Opaque" });
        let tynamestring = tyname.to_cpp_name();
        let mut for_extern_c_ts = if tyname.has_namespace() {
            let ns_string = tyname
                .ns_segment_iter()
                .cloned()
                .collect::<Vec<String>>()
                .join("::");
            quote! {
                #[namespace = #ns_string]
            }
        } else {
            TokenStream2::new()
        };

        let mut fulltypath = Vec::new();
        // We can't use parse_quote! here because it doesn't support type aliases
        // at the moment.
        let colon = TokenTree::Punct(proc_macro2::Punct::new(':', proc_macro2::Spacing::Joint));
        for_extern_c_ts.extend(
            [
                TokenTree::Ident(make_ident("type")),
                TokenTree::Ident(final_ident.clone()),
                TokenTree::Punct(proc_macro2::Punct::new('=', proc_macro2::Spacing::Alone)),
                TokenTree::Ident(make_ident("super")),
                colon.clone(),
                colon.clone(),
                TokenTree::Ident(make_ident("bindgen")),
                colon.clone(),
                colon.clone(),
                TokenTree::Ident(make_ident("root")),
                colon.clone(),
                colon.clone(),
            ]
            .to_vec(),
        );
        fulltypath.push(make_ident("bindgen"));
        fulltypath.push(make_ident("root"));
        for segment in tyname.ns_segment_iter() {
            let id = make_ident(segment);
            for_extern_c_ts
                .extend([TokenTree::Ident(id.clone()), colon.clone(), colon.clone()].to_vec());
            fulltypath.push(id);
        }
        for_extern_c_ts.extend(
            [
                TokenTree::Ident(final_ident.clone()),
                TokenTree::Punct(proc_macro2::Punct::new(';', proc_macro2::Spacing::Alone)),
            ]
            .to_vec(),
        );
        fulltypath.push(final_ident.clone());
        self.extern_c_mod_items
            .push(ForeignItem::Verbatim(for_extern_c_ts));
        self.bridge_items.push(Item::Impl(parse_quote! {
            impl UniquePtr<#final_ident> {}
        }));
        self.all_items.push(Item::Impl(parse_quote! {
            unsafe impl cxx::ExternType for #(#fulltypath)::* {
                type Id = cxx::type_id!(#tynamestring);
                type Kind = cxx::kind::#kind_item;
            }
        }));
        self.add_use(tyname.get_namespace(), &final_ident);
        self.types_found.push(final_ident);
        Ok(())
    }

    fn build_include_foreign_items(&self) -> Vec<ForeignItem> {
        let extra_inclusion = if self.additional_cpp_needs.is_empty() {
            None
        } else {
            Some("autocxxgen.h".to_string())
        };
        let chained = self.include_list.iter().chain(extra_inclusion.iter());
        chained
            .map(|inc| {
                ForeignItem::Macro(parse_quote! {
                    include!(#inc);
                })
            })
            .collect()
    }

    fn add_use(&mut self, ns: &Namespace, id: &Ident) {
        self.final_uses.push(Use {
            ns: ns.clone(),
            id: id.clone(),
        });
    }

    /// Adds items which we always add, cos they're useful.
    fn generate_utilities(&mut self) {
        // Unless we've been specifically asked not to do so, we always
        // generate a 'make_string' function. That pretty much *always* means
        // we run two passes through bindgen. i.e. the next 'if' is always true,
        // and we always generate an additional C++ file for our bindings additions,
        // unless the include_cpp macro has specified ExcludeUtilities.
        self.extern_c_mod_items.push(ForeignItem::Fn(parse_quote!(
            fn make_string(str_: &str) -> UniquePtr<CxxString>;
        )));
        self.add_use(&Namespace::new(), &make_ident("make_string"));
        self.additional_cpp_needs
            .push(AdditionalNeed::MakeStringConstructor);
    }

    fn convert_new_method(
        &mut self,
        mut m: syn::ImplItemMethod,
        ty: &TypeName,
        i: &syn::ItemImpl,
        output_items: &mut Vec<Item>,
    ) {
        let self_ty = i.self_ty.as_ref();
        let (arrow, oldreturntype) = match &m.sig.output {
            ReturnType::Type(arrow, ty) => (arrow, ty),
            ReturnType::Default => return,
        };
        let cpp_constructor_args = m.sig.inputs.iter().filter_map(|x| match x {
            FnArg::Typed(pt) => type_to_typename(&pt.ty).and_then(|x| match *(pt.pat.clone()) {
                syn::Pat::Ident(pti) => Some((x, pti.ident)),
                _ => None,
            }),
            FnArg::Receiver(_) => None,
        });
        let (cpp_arg_types, cpp_arg_names): (Vec<_>, Vec<_>) = cpp_constructor_args.unzip();
        let rs_args = &m.sig.inputs;
        self.additional_cpp_needs
            .push(AdditionalNeed::MakeUnique(ty.clone(), cpp_arg_types));
        // Create a function which calls Bob_make_unique
        // from Bob::make_unique.
        let call_name = Ident::new(
            &format!("{}_make_unique", ty.to_string()),
            Span::call_site(),
        );
        self.add_use(&ty.get_namespace(), &call_name);
        self.extern_c_mod_items.push(ForeignItem::Fn(parse_quote! {
            pub fn #call_name ( #rs_args ) -> UniquePtr< #self_ty >;
        }));
        m.block = parse_quote!( {
            cxxbridge::#call_name(
                #(#cpp_arg_names),*
            )
        });
        m.sig.ident = Ident::new("make_unique", Span::call_site());
        let new_return_type: TypePath = parse_quote! {
            cxx::UniquePtr < #oldreturntype >
        };
        m.sig.unsafety = None;
        m.sig.output = ReturnType::Type(*arrow, Box::new(Type::Path(new_return_type)));
        let new_impl_method = syn::ImplItem::Method(m);
        let mut new_item_impl = i.clone();
        new_item_impl.attrs = Vec::new();
        new_item_impl.unsafety = None;
        new_item_impl.items = vec![new_impl_method];
        output_items.push(Item::Impl(new_item_impl));
    }

    fn convert_foreign_mod_items(
        &mut self,
        foreign_mod_items: Vec<ForeignItem>,
        ns: &Namespace,
    ) -> Result<(), ConvertError> {
        for i in foreign_mod_items {
            match i {
                ForeignItem::Fn(f) => {
                    self.convert_foreign_fn(f, ns)?;
                }
                _ => return Err(ConvertError::UnknownForeignItem),
            }
        }
        Ok(())
    }

    fn convert_foreign_fn(
        &mut self,
        fun: ForeignItemFn,
        ns: &Namespace,
    ) -> Result<(), ConvertError> {
        // This function is one of the most complex parts of bridge_converter.
        // It needs to consider:
        // 1. Rejecting constructors entirely.
        // 2. For methods, we need to strip off the class name.
        // 3. For anything taking or returning a non-POD type _by value_,
        //    we need to generate a wrapper function in C++ which wraps and unwraps
        //    it from a unique_ptr.
        // See if it's a constructor, in which case skip it.
        // We instead pass onto cxx an alternative make_unique implementation later.
        for ty in &self.types_found {
            let constructor_name = format!("{}_{}", ty, ty);
            if fun.sig.ident == constructor_name {
                return Ok(());
            }
            let destructor_name = format!("{}_{}_destructor", ty, ty);
            if fun.sig.ident == destructor_name {
                return Ok(());
            }
        }
        // Now let's analyze all the parameters. We do this first
        // because we'll use this to determine whether this is a method.
        let (mut params, param_details): (Punctuated<_, syn::Token![,]>, Vec<_>) = fun
            .sig
            .inputs
            .into_iter()
            .map(|i| self.convert_fn_arg(i))
            .unzip();

        let is_a_method = param_details.iter().any(|b| b.was_self);

        // Work out naming.
        let mut rust_name = fun.sig.ident.to_string();
        if is_a_method {
            // bindgen generates methods with the name:
            // {class}_{method name}
            // It then generates an impl section for the Rust type
            // with the original name, but we currently discard that impl section.
            // We want to feed cxx methods with just the method name, so let's
            // strip off the class name.
            // TODO test with class names containing underscores. It should work.
            for cn in &self.types_found {
                let cn = cn.to_string();
                if rust_name.starts_with(&cn) {
                    rust_name = rust_name[cn.len() + 1..].to_string();
                    break;
                }
            }
        }

        // When we generate the cxx::bridge fn declaration, we'll need to
        // put something different into here if we have to do argument or
        // return type conversion, so get some mutable variables ready.
        let mut rust_name_attr = Vec::new();
        let rust_name_ident = make_ident(&rust_name);
        let mut cxxbridge_name = rust_name_ident.clone();

        // Analyze the return type, just as we previously did for the
        // parameters.
        let (mut ret_type, ret_type_conversion) = self.convert_return_type(fun.sig.output);

        // Do we need to convert either parameters or return type?
        let param_conversion_needed = param_details.iter().any(|b| b.conversion.work_needed());
        let ret_type_conversion_needed = ret_type_conversion
            .as_ref()
            .map_or(false, |x| x.work_needed());
        let wrapper_function_needed = param_conversion_needed | ret_type_conversion_needed;

        if wrapper_function_needed {
            // Generate a new layer of C++ code to wrap/unwrap parameters
            // and return values into/out of std::unique_ptrs.
            // First give instructions to generate the additional C++.
            let cpp_construction_ident = cxxbridge_name;
            cxxbridge_name = make_ident(&format!("{}_up_wrapper", rust_name));
            let a = AdditionalNeed::ByValueWrapper(Box::new(ByValueWrapper {
                id: cpp_construction_ident,
                return_conversion: ret_type_conversion.clone(),
                argument_conversion: param_details.iter().map(|d| d.conversion.clone()).collect(),
                is_a_method,
            }));
            self.additional_cpp_needs.push(a);
            // Now modify the cxx::bridge entry we're going to make.
            if let Some(conversion) = ret_type_conversion {
                let new_ret_type = conversion.unconverted_rust_type();
                ret_type = parse_quote!(
                    -> #new_ret_type
                );
            }
            params.clear();
            for pd in param_details {
                let type_name = pd.conversion.converted_rust_type();
                let arg_name = if pd.was_self {
                    parse_quote!(autocxx_gen_this)
                } else {
                    pd.name
                };
                params.push(parse_quote!(
                    #arg_name: #type_name
                ));
            }
            // Keep the original Rust name the same so callers don't
            // need to know about all of these shenanigans.
            rust_name_attr = Attribute::parse_outer
                .parse2(quote!(
                    #[rust_name = #rust_name]
                ))
                .unwrap();
        };
        // Finally - namespace support. All the Types in everything
        // above this point are fully qualified. We need to unqualify them.
        // We need to do that _after_ the above wrapper_function_needed
        // work, because it relies upon spotting fully qualified names like
        // std::unique_ptr. However, after it's done its job, all such
        // well-known types should be unqualified already (e.g. just UniquePtr)
        // and the following code will act to unqualify only those types
        // which the user has declared.
        let params = unqualify_params(params);
        let ret_type = unqualify_ret_type(ret_type);
        // And we need to make an attribute for the namespace that the function
        // itself is in.
        let namespace_attr = if ns.is_empty() {
            Vec::new()
        } else {
            let namespace_string = ns.to_string();
            Attribute::parse_outer
                .parse2(quote!(
                    #[namespace = #namespace_string]
                ))
                .unwrap()
        };
        // At last, actually generate the cxx::bridge entry.
        let vis = &fun.vis;
        self.extern_c_mod_items.push(ForeignItem::Fn(parse_quote!(
            #(#namespace_attr)*
            #(#rust_name_attr)*
            #vis fn #cxxbridge_name ( #params ) #ret_type;
        )));
        if !is_a_method || wrapper_function_needed {
            self.add_use(&ns, &rust_name_ident);
        }
        Ok(())
    }

    /// Returns additionally a Boolean indicating whether an argument was
    /// 'this' and another one indicating whether we took a type by value
    /// and that type was non-trivial.
    ///
    /// Regarding autocxx_gen_this, this pertains to what happens if
    /// we come across a *method* which takes or returns a non-POD type
    /// by value (for example a std::string or a struct containing a
    /// std::string). For normal functions, additional_cpp_generator
    /// generates a wrapper method which effectively boxes/unboxes
    /// values from unique_ptr to the desired by-value type.
    ///
    /// For methods, we generate a similar wrapper, but it's not a
    /// member of the original class - it's just a standalone function.
    /// On the C++ side this method is happy to call the original
    /// member function of the class, and all is well. But then we come
    /// back here during the second invocation of bridge_converter,
    /// and discover the new function we generated. We then have to
    /// decide how to teach cxx about that function, and neither
    /// option is satisfactory:
    /// 1) If we rename the first parameter to 'self' then cxx
    ///    will treat it as a method. This is what we want because
    ///    it means we can call it from Rust like this:
    ///      my_object.methodWhichTakesValue(uniquePtrToArg)
    ///    But, in cxx's own generated code, it will insist on calling
    ///      Type::methodWhichTakesValue(...)
    ///    That doesn't work, since our autogenerated
    ///    methodWhichTakesValue (actually called
    ///    methodWhichTakesValue_up_wrapper) is just a function.
    /// 2) Don't give the first parameter a special name. In which case
    ///    we will generate a standalone function on the Rust side.
    fn convert_fn_arg(&self, arg: FnArg) -> (FnArg, ArgumentAnalysis) {
        match arg {
            FnArg::Typed(mut pt) => {
                let mut found_this = false;
                let old_pat = *pt.pat;
                let new_pat = match old_pat {
                    syn::Pat::Ident(mut pp) if pp.ident == "this" =>
                    // TODO - consider also spotting
                    // autocxx_gen_this as per above big comment.
                    {
                        found_this = true;
                        pp.ident = Ident::new("self", pp.ident.span());
                        syn::Pat::Ident(pp)
                    }
                    _ => old_pat,
                };
                let new_ty = self.convert_boxed_type(pt.ty);
                let conversion = self.conversion_required(&new_ty);
                pt.pat = Box::new(new_pat.clone());
                pt.ty = new_ty;
                (
                    FnArg::Typed(pt),
                    ArgumentAnalysis {
                        was_self: found_this,
                        name: new_pat,
                        conversion,
                    },
                )
            }
            _ => panic!("FnArg::Receiver not yet handled"),
        }
    }

    fn conversion_required(&self, ty: &Type) -> ArgumentConversion {
        match ty {
            Type::Path(p) => {
                if self
                    .byvalue_checker
                    .is_pod(&TypeName::from_cxx_type_path(p))
                {
                    ArgumentConversion::new_unconverted(ty.clone())
                } else {
                    ArgumentConversion::new_from_unique_ptr(ty.clone())
                }
            }
            _ => ArgumentConversion::new_unconverted(ty.clone()),
        }
    }

    fn requires_conversion(&self, ty: &Type) -> bool {
        match ty {
            Type::Path(typ) => !self
                .byvalue_checker
                .is_pod(&TypeName::from_cxx_type_path(typ)),
            _ => false,
        }
    }

    fn convert_return_type(&self, rt: ReturnType) -> (ReturnType, Option<ArgumentConversion>) {
        match rt {
            ReturnType::Default => (ReturnType::Default, None),
            ReturnType::Type(rarrow, boxed_type) => {
                let boxed_type = self.convert_boxed_type(boxed_type);
                let conversion = if self.requires_conversion(boxed_type.as_ref()) {
                    ArgumentConversion::new_to_unique_ptr(*boxed_type.clone())
                } else {
                    ArgumentConversion::new_unconverted(*boxed_type.clone())
                };
                (ReturnType::Type(rarrow, boxed_type), Some(conversion))
            }
        }
    }

    fn convert_boxed_type(&self, ty: Box<Type>) -> Box<Type> {
        Box::new(self.convert_type(*ty))
    }

    fn convert_type(&self, ty: Type) -> Type {
        match ty {
            Type::Path(p) => {
                let newp = self.convert_type_path(p);
                // Special handling because rust_Str (as emitted by bindgen)
                // doesn't simply get renamed to a different type _identifier_.
                // This plain type-by-value (as far as bindgen is concerned)
                // is actually a &str.
                if should_dereference_in_cpp(&newp) {
                    Type::Reference(parse_quote! {
                        &str
                    })
                } else {
                    Type::Path(newp)
                }
            }
            Type::Reference(mut r) => {
                r.elem = self.convert_boxed_type(r.elem);
                Type::Reference(r)
            }
            Type::Ptr(ptr) => Type::Reference(self.convert_ptr_to_reference(ptr)),
            _ => ty,
        }
    }

    fn convert_ptr_to_reference(&self, ptr: TypePtr) -> TypeReference {
        let mutability = ptr.mutability;
        let elem = self.convert_boxed_type(ptr.elem);
        // TODO - in the future, we should check if this is a rust::Str and throw
        // a wobbler if not. rust::Str should only be seen _by value_ in C++
        // headers; it manifests as &str in Rust but on the C++ side it must
        // be a plain value. We should detect and abort.
        parse_quote! {
            & #mutability #elem
        }
    }

    fn convert_type_path(&self, mut typ: TypePath) -> TypePath {
        if typ.path.segments.iter().next().unwrap().ident == "root" {
            typ.path.segments = typ
                .path
                .segments
                .into_iter()
                .skip(1) // skip root
                .map(|s| -> PathSegment {
                    let ident = &s.ident;
                    let args = match s.arguments {
                        PathArguments::AngleBracketed(mut ab) => {
                            ab.args = self.convert_punctuated(ab.args);
                            PathArguments::AngleBracketed(ab)
                        }
                        _ => s.arguments,
                    };
                    parse_quote!( #ident #args )
                })
                .collect();
        }
        self.replace_cpp_with_cxx(typ)
    }

    fn replace_cpp_with_cxx(&self, typ: TypePath) -> TypePath {
        let mut last_seg_args = None;
        let mut seg_iter = typ.path.segments.iter().peekable();
        while let Some(seg) = seg_iter.next() {
            if !seg.arguments.is_empty() {
                if seg_iter.peek().is_some() {
                    panic!("Found a type with path arguments not on the last segment")
                } else {
                    last_seg_args = Some(seg.arguments.clone());
                }
            }
        }
        drop(seg_iter);
        let tn = TypeName::from_cxx_type_path(&typ);
        // Let's see if this is a typedef.
        let typ = match self.resolve_typedef(&tn) {
            Some(newid) => newid.to_cxx_type_path(),
            None => typ,
        };

        // This line will strip off any path arguments...
        let mut typ = replace_type_path_without_arguments(typ);
        // but then we'll put them back again as necessary.
        if let Some(last_seg_args) = last_seg_args {
            let last_seg = typ.path.segments.last_mut().unwrap();
            last_seg.arguments = last_seg_args;
        }
        typ
    }

    fn resolve_typedef<'b>(&'b self, tn: &'b TypeName) -> Option<&'b TypeName> {
        match self.typedefs.get(&tn) {
            None => None,
            Some(TypedefTarget::NoArguments(original_tn)) => {
                match self.resolve_typedef(original_tn) {
                    None => Some(original_tn),
                    Some(further_resolution) => Some(further_resolution)
                }
            },
            _ => panic!("Asked to resolve typedef {} but it leads to something complex which autocxx cannot yet handle", tn.to_cpp_name())
        }
    }

    fn convert_punctuated<P>(
        &self,
        pun: Punctuated<GenericArgument, P>,
    ) -> Punctuated<GenericArgument, P>
    where
        P: Default,
    {
        let mut new_pun = Punctuated::new();
        for arg in pun.into_iter() {
            new_pun.push(match arg {
                GenericArgument::Type(t) => GenericArgument::Type(self.convert_type(t)),
                _ => arg,
            });
        }
        new_pun
    }

    /// Generate lots of 'use' statements to pull cxxbridge items into the output
    /// mod hierarchy according to C++ namespaces.
    fn generate_final_use_statements(&mut self) {
        let ns_entries = NamespaceEntries::new(&self.final_uses);
        Self::append_child_namespace(&ns_entries, &mut self.all_items);
    }

    fn append_child_namespace(ns_entries: &NamespaceEntries, output_items: &mut Vec<Item>) {
        for item in ns_entries.entries() {
            let id = &item.id;
            output_items.push(Item::Use(parse_quote!(
                pub use cxxbridge :: #id;
            )));
        }
        for (child_name, child_ns_entries) in ns_entries.children() {
            let child_id = make_ident(child_name);
            let mut new_mod: ItemMod = parse_quote!(
                pub mod #child_id {
                    use super::cxxbridge;
                }
            );
            Self::append_child_namespace(
                child_ns_entries,
                &mut new_mod.content.as_mut().unwrap().1,
            );
            output_items.push(Item::Mod(new_mod));
        }
    }
}

struct ArgumentAnalysis {
    conversion: ArgumentConversion,
    name: Pat,
    was_self: bool,
}
