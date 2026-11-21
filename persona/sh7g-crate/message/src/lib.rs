use proc_macro::{Literal, TokenStream, TokenTree};

#[proc_macro]
pub fn greeting(_: TokenStream) -> TokenStream {
    TokenTree::Literal(Literal::byte_string(b"SH7-g hello from a proc macro\n")).into()
}
