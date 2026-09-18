//! Compiles the code examples in the documentation as doctests.
//!
//! Each fenced `rust` block in the matched markdown files becomes a doctest,
//! so `cargo test --doc -p infinity-mdtests` fails whenever a documented
//! example stops compiling. Blocks are marked `rust,no_run` in the docs
//! because they talk to real model providers and tool servers; the goal here
//! is compilation, not execution.

#[doc(hidden)]
#[cfg(doctest)]
mod docs {
    include_mdtests::include_mdtests!("docs/docs/infinity-runtime/quickstart/*.md");
}
