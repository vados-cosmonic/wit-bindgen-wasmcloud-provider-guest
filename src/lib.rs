//! Macro for performing [`wit-bindgen`](https://github.com/bytecodealliance/wit-bindgen) in
//! addition to performing wasmCloud-specific functionality to produce a [capability provider](https://wasmcloud.com/docs/fundamentals/capabilities/create-provider/)
//!
//!  expect input for the macro to look like:
//!
//! ```
//! wasmcloud_provider_macros::generate!(YourProvider, ...wit-bindgen args)
//!
//! struct YourProvider;
//!
//! // Implementation of methods specific to your provider, along with
//! // methods required of any wasmCloud provider (ex. `_put_link()`)
//! impl YourProvider {
//!   ...
//! }
//!
//! // Implementation of your exported wasmcloud interface
//! impl crate::exports::wasmcloud::some_contract::some_interface::SomeFunction for YourProvider {
//!     ...
//! }
//!
//! export_contract!(YourProvider);
//! ```

use std::collections::HashMap;

use heck::ToUpperCamelCase;
use proc_macro2::{Ident, Punct, Spacing, Span, TokenTree};
mod vendor;
use quote::{format_ident, ToTokens, TokenStreamExt};
use syn::{
    punctuated::Punctuated, token::PathSep, visit_mut::visit_item_mut, visit_mut::VisitMut,
    AttrStyle, Attribute, Item, ItemFn, ItemMod, LitStr, Meta, MetaList, Path, PathSegment,
    ReturnType, Token,
};

use vendor::wit_bindgen_rust_macro::generate as wit_bindgen_generate;

type WitNamespaceName = String;
type WitPackageName = String;
type WitInterfaceName = String;

/// Error message shown when the macro receives invalid args
const INVALID_INPUT_ERROR_TEXT: &str = r#"

"#;

/// Performs procedural macro generation, utilizing [`wit-bindgen`](https://github.com/bytecodealliance/wit-bindgen), and making
/// changes to it's output.
///
/// This macro generates functionality necessary to use a WIT-enabled Rust providers (a [`wasmtime::component`])
#[proc_macro]
pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let item: proc_macro2::TokenStream = input.into();

    // Ensure that we have the args we expect (at least 5 tokens should be present):
    // (<impl struct name> <comma> <... wit-bindgen args>)
    let tokens = item.into_iter().collect::<Vec<TokenTree>>();
    if tokens.len() < 3 {
        panic!("invalid token length, {}", INVALID_INPUT_ERROR_TEXT);
    }

    // Extract the identifier for the impl struct name from the tokens supplied
    let (impl_struct_name, rest) = match tokens.split_at(2) {
        (&[TokenTree::Ident(ref struct_name), TokenTree::Punct(ref p)], rest)
            if p.as_char() == ',' =>
        {
            (struct_name, rest)
        }
        _ => {
            panic!(
                "missing/invalid arguments to macro, {}",
                INVALID_INPUT_ERROR_TEXT
            );
        }
    };

    // // Seperate the wit bindgen args
    let mut bindgen_args = proc_macro2::TokenStream::new();
    bindgen_args.extend(Vec::from(rest));

    // Perform wit-bindgen on the tokens that are bindgen args
    let wit_bindgen_ts: proc_macro2::TokenStream = wit_bindgen_generate(bindgen_args.into()).into();

    // TODO: detect bindgen failure -- tokens just don't get generated
    // ex. when exported world does not match package (as in package <ns>/<package>)

    // Parse the wit-bindgen generated tokens as a file
    let mut wit_bindgen_ast: syn::File =
        syn::parse2(wit_bindgen_ts).expect("failed to parse wit-bindgen generated code as file");

    // TODO: look for 'failed to parse'
    // TREE:
    // DEBUG: GENERATED AST? File { shebang: None, attrs: [], items: [Item::Macro { attrs: [], ident: None, mac: Macro { path: Path { leading_colon: Some(PathSep), segments: [PathSegment { ident: Ident { ident: "core", span: #5 bytes(0..66) }, arguments: PathArguments::None }, PathSep, PathSegment { ident: Ident { ident: "compile_error", span: #5 bytes(0..66) }, arguments: PathArguments::None }] }, bang_token: Not, delimiter: MacroDelimiter::Brace(Brace), tokens: TokenStream [Literal { kind: Str, symbol: "failed to parse package: /home/mrman/code/work/cosmonic/bindgen-test-kv/wit\\n\\nCaused by:\\n    expected `world`, `interface` or `use`, found an identifier\\n         --> /home/mrman/code/work/cosmonic/bindgen-test-kv/wit/keyvalue.wit:29:1\\n          |\\n       29 | default world keyvalue {

    // Visit the code that has been generated, to extract information we'll need to modify it
    let mut visitor = WitBindgenOutputVisitor::default();
    let _ = visitor.visit_file_mut(&mut wit_bindgen_ast);

    // Turn the function calls into object declarations for receiving from lattice
    let methods_by_iface = if let Some(pkg) = &visitor.wit_package {
        build_lattice_methods_by_wit_interface(
            pkg,
            &visitor.serde_extended_structs,
            &visitor.import_trait_fns,
        )
    } else {
        panic!("failed to parse top-level WIT package name while reading bindgen output")
    };

    // Convert AST that was generated by wit-bindgen to a TokenStream for use
    let wit_bindgen_ast_tokens = wit_bindgen_ast.to_token_stream();

    // Generate wit interface specific code for each interface
    let mut iface_tokens = proc_macro2::TokenStream::new();
    for (wit_iface_name, methods) in methods_by_iface.iter() {
        let wit_iface = Ident::new(wit_iface_name, Span::call_site());

        // Generate lists that will be iterated in tandem to build out functionality
        let struct_names = methods
            .clone()
            .into_iter()
            .map(|LatticeMethod { struct_name, .. }| struct_name)
            .collect::<Vec<proc_macro2::Ident>>();
        let struct_members = methods
            .clone()
            .into_iter()
            .map(|LatticeMethod { struct_members, .. }| struct_members)
            .collect::<Vec<proc_macro2::TokenStream>>();
        let lattice_method_names = methods
            .clone()
            .into_iter()
            .map(
                |LatticeMethod {
                     lattice_method_name,
                     ..
                 }| lattice_method_name,
            )
            .collect::<Vec<LitStr>>();
        let func_names = methods
            .clone()
            .into_iter()
            .map(|LatticeMethod { func_name, .. }| func_name)
            .collect::<Vec<Ident>>();
        let invocation_args = methods
            .clone()
            .into_iter()
            .map(
                |LatticeMethod {
                     invocation_args, ..
                 }| invocation_args,
            )
            .collect::<Vec<Vec<Ident>>>();
        let invocation_returns = methods
            .clone()
            .into_iter()
            .map(
                |LatticeMethod {
                     invocation_return, ..
                 }| invocation_return,
            )
            .collect::<Vec<ReturnType>>();

        // TODO: bug here -- multiple interfaces means multiple impl blocks for Message Dispatch
        // they must be combined

        iface_tokens.append_all(quote::quote!(
            // START => Generated imports for method invocations via lattice
            #(
                #[derive(Debug, ::serde::Serialize, ::serde::Deserialize)]
                struct #struct_names {
                    #struct_members
                }
            )*
            // END => Generated imports for method invocations via lattice

            /// MessageDispatch ensures that your provider can receive and
            /// process messages sent to it over the lattice
            ///
            /// This implementation is a stub and must be filled out by implementers
            #[async_trait]
            impl ::wasmcloud_provider_sdk::MessageDispatch for #impl_struct_name {
                async fn dispatch<'a>(
                    &'a self,
                    ctx: ::wasmcloud_provider_sdk::Context,
                    method: String,
                    body: std::borrow::Cow<'a, [u8]>,
                ) -> Result<Vec<u8>, ::wasmcloud_provider_sdk::error::ProviderInvocationError> {
                    match method.as_str() {
                        #(
                            #lattice_method_names => {
                                let input: #struct_names = ::wasmcloud_provider_sdk::deserialize(&body)?;
                                let result = self
                                    .#func_names(
                                        ctx,
                                        #(
                                            input.#invocation_args,
                                        )*
                                    )
                                    .await
                                    .map_err(|e| {
                                        ::wasmcloud_provider_sdk::error::ProviderInvocationError::Provider(e.to_string())
                                    })?;
                                Ok(::wasmcloud_provider_sdk::serialize(&result)?)
                            }
                        )*
                        _ => Err(::wasmcloud_provider_sdk::error::InvocationError::Malformed(format!(
                            "Invalid method name {method}",
                        ))
                                 .into()),
                    }
                }
            }

            #[async_trait]
            pub trait #wit_iface {
                #(
                    async fn #func_names (
                        &self,
                        ctx: ::wasmcloud_provider_sdk::Context,
                        #struct_members,
                    ) #invocation_returns;
                )*
            }

            #[async_trait]
            impl #wit_iface for #impl_struct_name {
                #(
                    async fn #func_names (
                        &self,
                        ctx: ::wasmcloud_provider_sdk::Context,
                        #struct_members,
                    ) #invocation_returns {
                        self.#func_names(
                            ctx,
                            #(
                                #invocation_args,
                            )*
                        ).await
                    }
                )*
            }

        ));
    }

    // Build the token stream that wasmcloud will add on (not wit-bindgen specific)
    let wasmcloud_ts = quote::quote!(
        use ::serde::{Serialize, Deserialize};
        use ::async_trait::async_trait;

        // START => Codegen performed by wit-bindgen
        #wit_bindgen_ast_tokens
        // END => Codegen performed by wit-bindgen

        /// ProviderHandler ensures that your provider handles the basic
        /// required functionality of all Providers on a wasmCloud lattice.
        ///
        /// This implementation is a stub and must be filled out by implementers
        #[async_trait]
        impl ::wasmcloud_provider_sdk::ProviderHandler for #impl_struct_name {
            async fn put_link(&self, ld: &::wasmcloud_provider_sdk::core::LinkDefinition) -> bool {
                self._put_link(ld).await
            }

            async fn delete_link(&self, actor_id: &str) {
                self._delete_link(actor_id).await
            }

            async fn shutdown(&self) {
                self._shutdown().await
            }
        }

        /// Given the implementation of ProviderHandler and MessageDispatch,
        /// the implementation for your struct is a guaranteed
        impl ::wasmcloud_provider_sdk::Provider for #impl_struct_name {}

        // START => per-interface traits & impl
        #iface_tokens
        // END => per-interface traits & impl

        // TODO: OTEL integration w/ cfg_attr
    );

    // Chain all bits of generated code together
    // let ts = wit_bindgen_ts.into_iter().chain(wasmcloud_ts.into_iter());
    // proc_macro2::TokenStream::from_iter(ts).into()
    wasmcloud_ts.into()
}

/// A struct for visiting the output of wit-bindgen
/// focused around gathering all the important declarations we care about
#[derive(Default)]
struct WitBindgenOutputVisitor {
    /// The detected namespace of the WIT file
    wit_ns: Option<WitNamespaceName>,

    /// The detected package of the WIT file
    wit_package: Option<WitPackageName>,

    /// Parents of the current module being traversed
    parents: Vec<Ident>,

    /// Top level module that does contains all WIT exports
    /// normally with internal modules starting from namespace
    /// ex. ('exports' -> <WIT namespace> -> <WIT pkg>)
    exports_ns_module: Option<ItemMod>,

    /// Structs that were modified and extended to derive Serialize/Deserialize
    serde_extended_structs: HashMap<String, Punctuated<syn::PathSegment, PathSep>>,

    /// Functions in traits that we'll have to stub eventually
    import_trait_fns: HashMap<WitInterfaceName, Vec<ItemFn>>,
}

impl WitBindgenOutputVisitor {
    fn is_wit_ns(&self, s: impl AsRef<str>) -> bool {
        if let Some(v) = &self.wit_ns {
            v == s.as_ref()
        } else {
            false
        }
    }

    fn current_module_level(&self) -> usize {
        self.parents.len()
    }

    fn current_module_name(&self) -> Option<String> {
        self.parents.last().map(|v| v.to_string())
    }

    /// Check whether a the current node is directly under the wasm namespace
    /// Primarily used for detecting the package
    /// i.e. '<ns>/<package>'
    fn at_wit_ns_module_child(&self) -> bool {
        self.parents
            .last()
            .is_some_and(|ps| self.is_wit_ns(ps.to_string()))
    }

    /// Check whether the direct parent has a given name value
    fn at_child_of_module(&self, name: impl AsRef<str>) -> bool {
        self.parents.last().is_some_and(|v| v == name.as_ref())
    }

    /// Check whether the direct parent has a given name value
    fn at_grandchild_of_module(&self, name: impl AsRef<str>) -> bool {
        match self.parents.len() {
            len if len >= 2 => self.parents[len - 2] == name.as_ref(),
            _ => false,
        }
    }

    /// Check whether we are currently at a module *below* the 'exports' known module name
    fn at_exported_module(&self) -> bool {
        self.parents.iter().any(|v| v == EXPORTS_MODULE_NAME)
    }
}

/// Rust module name that is used by wit-bindgen to generate all the modules
const EXPORTS_MODULE_NAME: &str = "exports";

impl VisitMut for WitBindgenOutputVisitor {
    fn visit_item_mod_mut(&mut self, node: &mut ItemMod) {
        debug_print(format!(
            "{}> [(lvl {}) module:{:?}]",
            "=".repeat(self.current_module_level()),
            self.current_module_level(),
            node.ident
        ));

        // Save the WIT namespace that we've recognized
        //
        // ASSUMPTION: The top level WIT namespace is always a module at @ level zero
        // of the generated output
        if self.current_module_level() == 0 && node.ident != EXPORTS_MODULE_NAME {
            self.wit_ns = Some(node.ident.to_string());
        }

        // Save the WIT package name
        //
        // ASSUMPTION: The level 1 modules in the detected top level wasm namespace
        // is the package the top level WIT package
        if self.current_module_level() == 1
        // If we're one level in and the closest parent is the wasm namespace,
        // we know this must be the package name
            && self.at_wit_ns_module_child()
            && !self.at_exported_module()
        {
            self.wit_package = Some(node.ident.to_string());
        }

        // Recognize the 'exports' module which contains
        // all the exported interfaces
        //
        // ASSUMPTION: all exported modules are put into a level 0 'exports' module
        // which contains the top level namespace again
        if self.current_module_level() == 1 && self.at_child_of_module(EXPORTS_MODULE_NAME) {
            // this would be the ('exports' -> <ns>) node, note 'exports' itself.
            self.exports_ns_module = Some(node.clone());
        }

        // ASSUMPTION: level 2 modules contain externally visible *or* used interfaces
        // (i.e. ones that are exported)
        // 'use' calls will  cause an interface to show up, but only if the
        // thing that uses it is imported/exported

        // Recur/Traverse deeper into the detected modules where possible
        if let Some((_, ref mut items)) = &mut node.content {
            // Save the current module before we go spelunking
            self.parents.push(node.ident.clone());

            for mut item in items {
                self.visit_item_mut(&mut item);
            }

            self.parents.pop();
        } else {
            debug_print(format!("empty module: [{}]", node.ident));
        }
    }

    fn visit_item_mut(&mut self, node: &mut syn::Item) {
        match node {
            Item::Fn(f) => {
                debug_print(format!(
                    "{}> [(lvl {}) module:{:?}] visiting fn {}",
                    "=".repeat(self.current_module_level()),
                    self.current_module_level(),
                    self.parents.last(),
                    f.sig.ident
                ));

                // If we're visiting a function that is inside a non-export, and the grand parent
                // is the top level package, we must gather the function calls to make lattice messages out of
                // the arguments so they can be received via the lattice
                match (&self.wit_package, &self.current_module_name()) {
                    (Some(pkg), Some(module_name))
                        if !self.at_exported_module() && self.at_grandchild_of_module(pkg) =>
                    {
                        // Find functions in traits that we must stub later
                        self.import_trait_fns
                            .entry(module_name.clone())
                            .or_default()
                            .push(f.clone());
                    }
                    _ => {}
                }
            }

            Item::Struct(s) => {
                debug_print(format!(
                    "{}> [(lvl {}) module:{:?}] visiting struct {:?}",
                    "=".repeat(self.current_module_level()),
                    self.current_module_level(),
                    self.parents.last(),
                    s.ident,
                ));

                // For all structs that we encounter defined natively in this package,
                // we want to inject serde's Serialize & Deserialize
                for attr in &mut s.attrs {
                    if let Attribute {
                        style: AttrStyle::Outer,
                        meta:
                            Meta::List(MetaList {
                                path,
                                ref mut tokens,
                                ..
                            }),
                        ..
                    } = attr
                    {
                        if path.get_ident().is_some_and(|v| v.to_string() == "derive") {
                            let mut serialize_macro = Punctuated::<Path, Token![::]>::new();
                            serialize_macro
                                .push(Path::from(Ident::new("serde", Span::call_site())));
                            serialize_macro
                                .push(Path::from(Ident::new("Serialize", Span::call_site())));

                            let mut deserialize_macro = Punctuated::<Path, Token![::]>::new();
                            deserialize_macro
                                .push(Path::from(Ident::new("serde", Span::call_site())));
                            deserialize_macro
                                .push(Path::from(Ident::new("Deserialize", Span::call_site())));

                            // Add Serialize/Serialize onto the derive
                            tokens.append_all(&[
                                Punct::new(',', Spacing::Alone).to_token_stream(),
                                serialize_macro.to_token_stream(),
                                Punct::new(',', Spacing::Alone).to_token_stream(),
                                deserialize_macro.to_token_stream(),
                            ]);

                            debug_print(format!(
                                "detected & appended serialize/deserialize to derive for: {:?}",
                                attr.path().get_ident()
                            ));
                        }
                    }
                }

                // Save import paths for structs that are extended
                let mut struct_import_path = Punctuated::<syn::PathSegment, Token![::]>::new();
                for p in self.parents.iter() {
                    struct_import_path.push(syn::PathSegment::from(p.clone()));
                }
                struct_import_path.push(syn::PathSegment::from(s.ident.clone()));
                self.serde_extended_structs
                    .insert(s.ident.to_string(), struct_import_path);
                // TODO: it is possible to have two similarly named structs but from different packages/interfaces
            }

            _ => visit_item_mut(self, node),
        }
    }
}

#[derive(Debug, Clone)]
struct LatticeMethod {
    /// The name of the method that would be used on the lattice
    lattice_method_name: LitStr,
    /// The name of the struct that can be deserialized to perform the invocation
    struct_name: Ident,
    /// Tokens that represent the struct member declarations
    struct_members: proc_macro2::TokenStream,
    /// Function name for the method that will be called after a lattice invocation is received
    func_name: Ident,
    /// Invocation arguments (i.e. invocation struct members)
    invocation_args: Vec<Ident>,
    /// Invocation arguments (i.e. invocation struct members)
    invocation_return: ReturnType,
}

/// Build <X>ArgumentObjects from functions that were detected as imports
fn build_lattice_methods_by_wit_interface(
    wit_pkg_name: &WitPackageName,
    struct_lookup: &HashMap<String, Punctuated<PathSegment, PathSep>>,
    map: &HashMap<WitInterfaceName, Vec<syn::ItemFn>>,
) -> HashMap<WitInterfaceName, Vec<LatticeMethod>> {
    let mut methods_by_name: HashMap<WitInterfaceName, Vec<LatticeMethod>> = HashMap::new();

    // Per module import we must build up a different structs
    for (wit_iface_name, funcs) in map.iter() {
        for f in funcs.iter() {
            // Create an identifier for the new struct that will represent the function invocation coming
            // across the lattice, in a <CamelCaseModule><CamelCaseInterface><CamelCaseFunctionName> pattern
            // (ex. MessagingConsumerRequestMultiInvocation)
            let lattice_method_name = LitStr::new(
                format!("Message.{}", f.sig.ident.to_string().to_upper_camel_case()).as_ref(),
                Span::call_site(),
            );

            let struct_name = format_ident!(
                "{}{}{}Invocation",
                wit_pkg_name.to_upper_camel_case(),
                wit_iface_name.to_upper_camel_case(),
                f.sig.ident.to_string().to_upper_camel_case()
            );

            // wit-bindgen generates functions that borrow (regardless of what opts.ownership is set to),
            // fucntions that look like the following could be generated:
            //
            // - fn request(subject : & str, body : Option < & [u8] >, timeout_ms : u32,) -> Result < BrokerMessage, wit_bindgen :: rt :: string :: String >
            // - fn request_multi(subject : & str, body : Option < & [u8] >, timeout_ms : u32, max_results : u32,) -> Result < wit_bindgen :: rt :: vec :: Vec :: < BrokerMessage >, wit_bindgen :: rt :: string :: String >
            // - fn publish(msg : & BrokerMessage,) -> Result < (), wit_bindgen :: rt :: string :: String >
            //
            // Since these arguments use lifetimes, we can't just convert them to structs without either naming or *removing* the lifetimes (via converting to owned data)

            // Build a list of invocation arguments similar to the structs
            let mut invocation_args: Vec<Ident> = Vec::new();

            // Transform the members and remove any lifetimes by manually converting references to owned data
            // (i.e. doing things like converting a type like &str to String mechanically)
            let struct_members = f
                .sig
                // Get all function inputs for the function signature
                .inputs
                .iter()
                .enumerate()
                .fold(proc_macro2::TokenStream::new(), |mut tokens, (idx, arg)| {
                    // If we're not the first index, add a comman
                    if idx != 0 {
                        tokens.append_all([&TokenTree::Punct(Punct::new(',', Spacing::Alone))]);
                    }

                    // Match on a single input argument in the function signature
                    match &arg
                            .to_token_stream()
                            .into_iter()
                            .collect::<Vec<TokenTree>>()[..]
                        {
                            // pattern: 'name: &T'
                            simple_ref @ &[
                                TokenTree::Ident(ref n), // name
                                TokenTree::Punct(_), // :
                                TokenTree::Punct(ref p), // &
                                TokenTree::Ident(ref t), // T
                            ] if p.as_char() == '&' => {
                                // Save the invocation argument for later
                                invocation_args.push(n.clone());

                                // Match the type that came out of the simple case
                                match t.to_string().as_str() {
                                    // A &str
                                    "str" => {
                                        tokens.append_all([
                                            &simple_ref[0],
                                            &simple_ref[1],
                                            // replace the type with an owned string
                                            &TokenTree::Ident(Ident::new("String", t.span())),
                                        ]);
                                    },

                                    // Unexpected non-standard type as reference
                                    // (likely a known custom type generated by wit-bindgen)
                                    _ => {

                                        // Add a modified group of tokens to the list for the struct
                                        tokens.append_all([
                                            &simple_ref[0], // name
                                            &simple_ref[1], // colon
                                        ]);

                                        // If we have a T that this module defined, we must use the full path to it
                                        // if not, it is likely a builtin, so we can use it directly
                                        if let Some(v) = struct_lookup.get(&simple_ref[3].to_string()) {
                                            tokens.append_all([ v.to_token_stream() ]);
                                        } else {
                                            tokens.append_all([ &simple_ref[3]]);
                                        };
                                    }
                                }
                            },

                            // pattern: 'name: Wrapper<&T>'
                            wrapped_ref @ &[
                                TokenTree::Ident(ref n),  // name
                                TokenTree::Punct(_),  // :
                                TokenTree::Ident(_),  // Wrapper
                                TokenTree::Punct(ref p),  // <
                                TokenTree::Punct(ref p2), // &
                                ..,  // T
                                TokenTree::Punct(_) // >
                            ] if p.as_char() == '<' && p2.as_char() == '&' => {
                                // Save the invocation argument for later
                                invocation_args.push(n.clone());

                                // Slice out the parts in between the < ... >
                                let type_section = &wrapped_ref[4..wrapped_ref.len()];

                                match &type_section[..] {
                                    // case: str
                                    [
                                        TokenTree::Punct(_), // <
                                        TokenTree::Ident(ref n),
                                        TokenTree::Punct(_) // >
                                    ] if n.to_string().as_str() == "str" => {
                                        tokens.append_all([
                                            &wrapped_ref[0], // name
                                            &wrapped_ref[1], // colon
                                            &wrapped_ref[2], // wrapper
                                            &wrapped_ref[3], // <
                                            &TokenTree::Ident(Ident::new("String", n.span())),
                                            &wrapped_ref[5], // >
                                        ]);
                                    },

                                    // case: [u8]
                                    [
                                        TokenTree::Punct(_), // <
                                        TokenTree::Group(g),
                                        TokenTree::Punct(_), // >
                                    ] if g.to_string().as_str() == "[u8]" => {
                                        tokens.append_all([
                                            &wrapped_ref[0], // name
                                            &wrapped_ref[1], // colon
                                            &wrapped_ref[2], // wrapper
                                            &wrapped_ref[3], // <
                                            &TokenTree::Ident(Ident::new("Vec", Span::call_site())), // Vec
                                            &TokenTree::Punct(Punct::new('<', Spacing::Joint)), // <
                                            &TokenTree::Ident(Ident::new("u8", Span::call_site())), // u8
                                            &TokenTree::Punct(Punct::new('>', Spacing::Joint)), // >
                                            &TokenTree::Punct(Punct::new('>', Spacing::Joint)), // >
                                        ]);
                                    },

                                    rest =>  {
                                        // If we have a < T >, and T is a struct this module defined, we must use the full path to it
                                        // if not, it is likely a builtin, so we can use it directly
                                        if let Some(v) = struct_lookup.get(&rest[1].to_string()) {
                                            tokens.append_all(&wrapped_ref[0..5]);
                                            tokens.append_all([ v.to_token_stream() ]);
                                            tokens.append_all(&wrapped_ref[6..]);
                                        } else {
                                            tokens.append_all(wrapped_ref);
                                        };
                                    },
                                }
                            },

                            // pattern: unknown
                            ts => {
                                // Save the first token (which should be the argument name) as an invocation argument for later
                                if let TokenTree::Ident(name) = &ts[0] {
                                    invocation_args.push(name.clone());
                                }

                                tokens.append_all(ts);
                            }
                        }

                    tokens
                });

            // Add the struct and it's members to a list that will be used in another quote
            // it cannot be added directly/composed to a TokenStream here to avoid import conflicts
            // in case bindgen-defined types are used.
            methods_by_name
                .entry(wit_iface_name.to_string().to_upper_camel_case())
                .or_default()
                .push(LatticeMethod {
                    lattice_method_name,
                    struct_name,
                    struct_members,
                    func_name: f.sig.ident.clone(),
                    invocation_args,
                    invocation_return: f.sig.output.clone(),
                });
        }
    }
    methods_by_name
}

// no-op when not in debug mode
#[cfg(not(feature = "debug"))]
fn debug_print(_s: impl AsRef<str>) {}

#[cfg(feature = "debug")]
fn debug_print(s: impl AsRef<str> + std::fmt::Display) {
    eprintln!("DEBUG: {}", s);
}