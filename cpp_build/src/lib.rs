//! This crate is the `cpp` cargo build script implementation. It is useless
//! without the companion crates `cpp`, and `cpp_macro`.
//!
//! For more information, see the [`cpp` crate module level
//! documentation](https://docs.rs/cpp).

extern crate cpp_common;
extern crate cpp_syn as syn;
extern crate cpp_synom as synom;

extern crate cpp_synmap;

extern crate cc;

#[macro_use]
extern crate lazy_static;

use cpp_common::{
    flags, parsing, Capture, Class, Closure, ClosureSig, Macro, FILE_HASH, LIB_NAME, OUT_DIR,
    STRUCT_METADATA_MAGIC, VERSION,
};
use cpp_synmap::SourceMap;
use std::env;
use std::fs::{create_dir, remove_dir_all, File};
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use syn::visit::Visitor;
use syn::{Ident, Mac, Spanned, Token, TokenTree, DUMMY_SPAN};

fn warnln_impl(a: String) {
    for s in a.lines() {
        println!("cargo:warning={}", s);
    }
}

macro_rules! warnln {
    ($($all:tt)*) => {
        $crate::warnln_impl(format!($($all)*));
    }
}

// Add the #line directive (pointing to this file) to a string literal.
// Note: if the this macro is itself in a macro, it should be on on the same line of the macro
macro_rules! add_line {
    ($e:expr) => {
        concat!("#line ", line!(), " \"", file!(), "\"\n", $e)
    };
}

const INTERNAL_CPP_STRUCTS: &'static str = add_line!(r#"
/* THIS FILE IS GENERATED BY rust-cpp. DO NOT EDIT */

#include "stdint.h" // For {u}intN_t
#include <new> // For placement new
#include <cstdlib> // For abort
#include <type_traits>
#include <utility>

namespace rustcpp {

// We can't just pass or return any type from extern "C" rust functions (because the call
// convention may differ between the C++ type, and the rust type).
// So we make sure to pass trivial structure that only contains a pointer to the object we want to
// pass. The constructor of these helper class contains a 'container' of the right size which will
// be allocated on the stack.
template<typename T> struct return_helper {
    struct container {
#if defined (_MSC_VER) && (_MSC_VER + 0 < 1900)
        char memory[sizeof(T)];
        ~container() { reinterpret_cast<T*>(this)->~T(); }
#else
        // The fact that it is in an union means it is properly sized and aligned, but we have
        // to call the destructor and constructor manually
        union { T memory; };
        ~container() { memory.~T(); }
#endif
        container() {}
    };
    const container* data;
    return_helper(int, const container &c = container()) : data(&c) { }
};

template<typename T> struct argument_helper {
    using type = const T&;
};
template<typename T> struct argument_helper<T&> {
    T &ref;
    argument_helper(T &x) : ref(x) {}
    using type = argument_helper<T&> const&;
};

template<typename T>
typename std::enable_if<std::is_copy_constructible<T>::value>::type copy_helper(const void *src, void *dest)
{ new (dest) T (*static_cast<T const*>(src)); }
template<typename T>
typename std::enable_if<!std::is_copy_constructible<T>::value>::type copy_helper(const void *, void *)
{ std::abort(); }
template<typename T>
typename std::enable_if<std::is_default_constructible<T>::value>::type default_helper(void *dest)
{ new (dest) T(); }
template<typename T>
typename std::enable_if<!std::is_default_constructible<T>::value>::type default_helper(void *)
{ std::abort(); }

template<typename T> int compare_helper(const T &a, const T&b, int cmp) {
    switch (cmp) {
        using namespace std::rel_ops;
        case 0:
            if (a < b)
                return -1;
            if (b < a)
                return 1;
            return 0;
        case -2: return a < b;
        case 2: return a > b;
        case -1: return a <= b;
        case 1: return a >= b;
    }
    std::abort();
}
}

#define RUST_CPP_CLASS_HELPER(HASH, ...) \
    extern "C" { \
    void __cpp_destructor_##HASH(void *ptr) { typedef __VA_ARGS__ T; static_cast<T*>(ptr)->~T(); } \
    void __cpp_copy_##HASH(const void *src, void *dest) { rustcpp::copy_helper<__VA_ARGS__>(src, dest); } \
    void __cpp_default_##HASH(void *dest) { rustcpp::default_helper<__VA_ARGS__>(dest); } \
    }
"#);

lazy_static! {
    static ref CPP_DIR: PathBuf = OUT_DIR.join("rust_cpp");
    static ref CARGO_MANIFEST_DIR: PathBuf = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect(
        r#"
-- rust-cpp fatal error --

The CARGO_MANIFEST_DIR environment variable was not set.
NOTE: rust-cpp's build function must be run in a build script."#
    ));
}

enum ExpandSubMacroType<'a> {
    Lit,
    Closure(&'a mut u32), // the offset
}

// Given a string containing some C++ code with a rust! macro,
// this functions expand the rust! macro to a call to an extern
// function
fn expand_sub_rust_macro(input: String, mut t: ExpandSubMacroType) -> String {
    let mut result = input;
    let mut extra_decl = String::new();
    loop {
        let tmp = result.clone();
        if let synom::IResult::Done(_, rust_invocation) =
            parsing::find_rust_macro(synom::ParseState::new(&tmp))
        {
            let fn_name: Ident = match t {
                ExpandSubMacroType::Lit => {
                    extra_decl.push_str(&format!("extern \"C\" void {}();\n", rust_invocation.id));
                    rust_invocation.id.clone()
                }
                ExpandSubMacroType::Closure(ref mut offset) => {
                    **offset += 1;
                    format!(
                        "rust_cpp_callbacks{file_hash}[{offset}]",
                        file_hash = *FILE_HASH,
                        offset = **offset - 1
                    ).into()
                }
            };

            let mut decl_types = rust_invocation
                .arguments
                .iter()
                .map(|&(_, ref val)| format!("rustcpp::argument_helper<{}>::type", val))
                .collect::<Vec<_>>();
            let mut call_args = rust_invocation
                .arguments
                .iter()
                .map(|&(ref val, _)| val.as_ref())
                .collect::<Vec<_>>();

            let fn_call = match rust_invocation.return_type {
                None => format!(
                    "reinterpret_cast<void (*)({types})>({f})({args})",
                    f = fn_name,
                    types = decl_types.join(", "),
                    args = call_args.join(", ")
                ),
                Some(rty) => {
                    decl_types.push(format!("rustcpp::return_helper<{rty}>", rty = rty));
                    call_args.push("0");
                    format!(
                        "std::move(*reinterpret_cast<{rty}*(*)({types})>({f})({args}))",
                        rty = rty,
                        f = fn_name,
                        types = decl_types.join(", "),
                        args = call_args.join(", ")
                    )
                }
            };

            let fn_call = {
                // remove the rust! macro from the C++ snippet
                let orig = result.drain(rust_invocation.begin..rust_invocation.end);
                // add \ņ to the invocation in order to keep the same amount of line numbers
                // so errors point to the right line.
                orig.filter(|x| *x == '\n').fold(fn_call, |mut res, _| {
                    res.push('\n');
                    res
                })
            };
            // add the invocation of call where the rust! macro used to be.
            result.insert_str(rust_invocation.begin, &fn_call);
        } else {
            break;
        }
    }

    return extra_decl + &result;
}

fn gen_cpp_lib(visitor: &Handle) -> PathBuf {
    let result_path = CPP_DIR.join("cpp_closures.cpp");
    let mut output = File::create(&result_path).expect("Unable to generate temporary C++ file");

    write!(output, "{}", INTERNAL_CPP_STRUCTS).unwrap();

    if visitor.callbacks_count > 0 {
        write!(
            output,
            add_line!(
                r#"
extern "C" {{
    void (*rust_cpp_callbacks{file_hash}[{callbacks_count}])() = {{}};
}}
        "#
            ),
            file_hash = *FILE_HASH,
            callbacks_count = visitor.callbacks_count
        ).unwrap();
    }

    write!(output, "{}\n\n", &visitor.snippets).unwrap();

    let mut sizealign = vec![];
    for &Closure {
        ref body,
        ref sig,
        ref callback_offset,
    } in &visitor.closures
    {
        let &ClosureSig {
            ref captures,
            ref cpp,
            ..
        } = sig;

        let hash = sig.name_hash();
        let name = sig.extern_name();

        let is_void = cpp == "void";

        // Generate the sizes array with the sizes of each of the argument types
        if is_void {
            sizealign.push(format!(
                "{{{hash}ull, 0, 1, {callback_offset}ull << 32}}",
                hash = hash,
                callback_offset = callback_offset
            ));
        } else {
            sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value | {callback_offset}ull << 32
            }}", hash=hash, type=cpp, callback_offset = callback_offset));
        }
        for &Capture { ref cpp, .. } in captures {
            sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=cpp));
        }

        // Generate the parameters and function declaration
        let params = captures
            .iter()
            .map(
                |&Capture {
                     mutable,
                     ref name,
                     ref cpp,
                 }| {
                    if mutable {
                        format!("{} & {}", cpp, name)
                    } else {
                        format!("{} const& {}", cpp, name)
                    }
                },
            )
            .collect::<Vec<_>>()
            .join(", ");

        if is_void {
            write!(
                output,
                add_line!(
                    r#"
extern "C" {{
void {name}({params}) {{
{body}
}}
}}
"#
                ),
                name = &name,
                params = params,
                body = body.node
            ).unwrap();
        } else {
            let comma = if params.is_empty() { "" } else { "," };
            let args = captures
                .iter()
                .map(|&Capture { ref name, .. }| name.as_ref())
                .collect::<Vec<_>>()
                .join(", ");
            write!(
                output,
                add_line!(
                    r#"
static inline {ty} {name}_impl({params}) {{
{body}
}}
extern "C" {{
void {name}({params}{comma} void* __result) {{
    ::new(__result) ({ty})({name}_impl({args}));
}}
}}
"#
                ),
                name = &name,
                params = params,
                comma = comma,
                ty = cpp,
                args = args,
                body = body.node
            ).unwrap();
        }
    }

    for class in &visitor.classes {
        let hash = class.name_hash();

        // Generate the sizes array
        sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=class.cpp));

        // Generate helper function.
        // (this is done in a macro, which right after a #line directing pointing to the location of
        // the cpp_class! macro in order to give right line information in the possible errors)
        write!(
            output,
            "{line}RUST_CPP_CLASS_HELPER({hash}, {cpp_name})\n",
            line = class.line,
            hash = hash,
            cpp_name = class.cpp
        ).unwrap();

        if class.derives("PartialEq") {
            write!(output,
                "{line}extern \"C\" bool __cpp_equal_{hash}(const {name} *a, const {name} *b) {{ return *a == *b; }}\n",
                line = class.line, hash = hash, name = class.cpp).unwrap();
        }
        if class.derives("PartialOrd") {
            write!(output,
                "{line}extern \"C\" bool __cpp_compare_{hash}(const {name} *a, const {name} *b, int cmp) {{ return rustcpp::compare_helper(*a, *b, cmp); }}\n",
                line = class.line, hash = hash, name = class.cpp).unwrap();
        }
    }

    let mut magic = vec![];
    for mag in STRUCT_METADATA_MAGIC.iter() {
        magic.push(format!("{}", mag));
    }

    write!(output, add_line!(r#"

namespace rustcpp {{

template<typename T>
struct AlignOf {{
    struct Inner {{
        char a;
        T b;
    }};
    static const uintptr_t value = sizeof(Inner) - sizeof(T);
}};

template<typename T>
struct Flags {{
    static const uintptr_t value =
        (std::is_copy_constructible<T>::value << {flag_is_copy_constructible}) |
        (std::is_default_constructible<T>::value << {flag_is_default_constructible}) |
#if !defined(__GNUC__) || (__GNUC__ + 0 >= 5) || defined(__clang__)
        (std::is_trivially_destructible<T>::value << {flag_is_trivially_destructible}) |
        (std::is_trivially_copyable<T>::value << {flag_is_trivially_copyable}) |
        (std::is_trivially_default_constructible<T>::value << {flag_is_trivially_default_constructible}) |
#endif
        0;
}};

struct SizeAlign {{
    uint64_t hash;
    uint64_t size;
    uint64_t align;
    uint64_t flags;
}};

struct MetaData {{
    uint8_t magic[128];
    uint8_t version[16];
    uint64_t length;
    SizeAlign data[{length}];
}};

MetaData
#ifdef __GNUC__
    __attribute__((weak))
#endif
    metadata = {{
    {{ {magic} }},
    "{version}",
    {length},
    {{ {data} }}
}};

}} // namespace rustcpp
"#),
        data = sizealign.join(", "),
        length = sizealign.len(),
        magic = magic.join(", "),
        version = VERSION,
        flag_is_copy_constructible = flags::IS_COPY_CONSTRUCTIBLE,
        flag_is_default_constructible = flags::IS_DEFAULT_CONSTRUCTIBLE,
        flag_is_trivially_destructible = flags::IS_TRIVIALLY_DESTRUCTIBLE,
        flag_is_trivially_copyable = flags::IS_TRIVIALLY_COPYABLE,
        flag_is_trivially_default_constructible = flags::IS_TRIVIALLY_DEFAULT_CONSTRUCTIBLE,
    ).unwrap();

    result_path
}

fn clean_artifacts() {
    if CPP_DIR.is_dir() {
        remove_dir_all(&*CPP_DIR).expect(
            r#"
-- rust-cpp fatal error --

Failed to remove existing build artifacts from output directory."#,
        );
    }

    create_dir(&*CPP_DIR).expect(
        r#"
-- rust-cpp fatal error --

Failed to create output object directory."#,
    );
}

/// This struct is for advanced users of the build script. It allows providing
/// configuration options to `cpp` and the compiler when it is used to build.
///
/// ## API Note
///
/// Internally, `cpp` uses the `cc` crate to build the compilation artifact,
/// and many of the methods defined on this type directly proxy to an internal
/// `cc::Build` object.
pub struct Config {
    cc: cc::Build,
    std_flag_set: bool, // true if the -std flag was specified
}

impl Config {
    /// Create a new `Config` object. This object will hold the configuration
    /// options which control the build. If you don't need to make any changes,
    /// `cpp_build::build` is a wrapper function around this interface.
    pub fn new() -> Config {
        let mut cc = cc::Build::new();
        cc.cpp(true).include(&*CARGO_MANIFEST_DIR);
        Config {
            cc: cc,
            std_flag_set: false,
        }
    }

    /// Add a directory to the `-I` or include path for headers
    pub fn include<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.cc.include(dir);
        self
    }

    /// Specify a `-D` variable with an optional value
    pub fn define(&mut self, var: &str, val: Option<&str>) -> &mut Self {
        self.cc.define(var, val);
        self
    }

    // XXX: Make sure that this works with sizes logic
    /// Add an arbitrary object file to link in
    pub fn object<P: AsRef<Path>>(&mut self, obj: P) -> &mut Self {
        self.cc.object(obj);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler
    pub fn flag(&mut self, flag: &str) -> &mut Self {
        if flag.starts_with("-std=") {
            self.std_flag_set = true;
        }
        self.cc.flag(flag);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler if it supports it
    pub fn flag_if_supported(&mut self, flag: &str) -> &mut Self {
        if flag.starts_with("-std=") {
            self.std_flag_set = true;
        }
        self.cc.flag_if_supported(flag);
        self
    }

    // XXX: Make sure this works with sizes logic
    /// Add a file which will be compiled
    pub fn file<P: AsRef<Path>>(&mut self, p: P) -> &mut Self {
        self.cc.file(p);
        self
    }

    /// Set the standard library to link against when compiling with C++
    /// support.
    ///
    /// The default value of this property depends on the current target: On
    /// OS X `Some("c++")` is used, when compiling for a Visual Studio based
    /// target `None` is used and for other targets `Some("stdc++")` is used.
    ///
    /// A value of `None` indicates that no automatic linking should happen,
    /// otherwise cargo will link against the specified library.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_link_stdlib(&mut self, cpp_link_stdlib: Option<&str>) -> &mut Self {
        self.cc.cpp_link_stdlib(cpp_link_stdlib);
        self
    }

    /// Force the C++ compiler to use the specified standard library.
    ///
    /// Setting this option will automatically set `cpp_link_stdlib` to the same
    /// value.
    ///
    /// The default value of this option is always `None`.
    ///
    /// This option has no effect when compiling for a Visual Studio based
    /// target.
    ///
    /// This option sets the `-stdlib` flag, which is only supported by some
    /// compilers (clang, icc) but not by others (gcc). The library will not
    /// detect which compiler is used, as such it is the responsibility of the
    /// caller to ensure that this option is only used in conjuction with a
    /// compiler which supports the `-stdlib` flag.
    ///
    /// A value of `None` indicates that no specific C++ standard library should
    /// be used, otherwise `-stdlib` is added to the compile invocation.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_set_stdlib(&mut self, cpp_set_stdlib: Option<&str>) -> &mut Self {
        self.cc.cpp_set_stdlib(cpp_set_stdlib);
        self
    }

    // XXX: Add support for custom targets
    //
    // /// Configures the target this configuration will be compiling for.
    // ///
    // /// This option is automatically scraped from the `TARGET` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn target(&mut self, target: &str) -> &mut Self {
    //     self.cc.target(target);
    //     self
    // }

    /// Configures the host assumed by this configuration.
    ///
    /// This option is automatically scraped from the `HOST` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn host(&mut self, host: &str) -> &mut Self {
        self.cc.host(host);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level(&mut self, opt_level: u32) -> &mut Self {
        self.cc.opt_level(opt_level);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level_str(&mut self, opt_level: &str) -> &mut Self {
        self.cc.opt_level_str(opt_level);
        self
    }

    /// Configures whether the compiler will emit debug information when
    /// generating object files.
    ///
    /// This option is automatically scraped from the `PROFILE` environment
    /// variable by build scripts (only enabled when the profile is "debug"), so
    /// it's not required to call this function.
    pub fn debug(&mut self, debug: bool) -> &mut Self {
        self.cc.debug(debug);
        self
    }

    // XXX: Add support for custom out_dir
    //
    // /// Configures the output directory where all object files and static
    // /// libraries will be located.
    // ///
    // /// This option is automatically scraped from the `OUT_DIR` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn out_dir<P: AsRef<Path>>(&mut self, out_dir: P) -> &mut Self {
    //     self.cc.out_dir(out_dir);
    //     self
    // }

    /// Configures the compiler to be used to produce output.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn compiler<P: AsRef<Path>>(&mut self, compiler: P) -> &mut Self {
        self.cc.compiler(compiler);
        self
    }

    /// Configures the tool used to assemble archives.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn archiver<P: AsRef<Path>>(&mut self, archiver: P) -> &mut Self {
        self.cc.archiver(archiver);
        self
    }

    /// Define whether metadata should be emitted for cargo allowing it to
    /// automatically link the binary. Defaults to `true`.
    pub fn cargo_metadata(&mut self, cargo_metadata: bool) -> &mut Self {
        // XXX: Use this to control the cargo metadata which rust-cpp produces
        self.cc.cargo_metadata(cargo_metadata);
        self
    }

    /// Configures whether the compiler will emit position independent code.
    ///
    /// This option defaults to `false` for `i686` and `windows-gnu` targets and
    /// to `true` for all other targets.
    pub fn pic(&mut self, pic: bool) -> &mut Self {
        self.cc.pic(pic);
        self
    }

    /// Extracts `cpp` declarations from the passed-in crate root, and builds
    /// the associated static library to be linked in to the final binary.
    ///
    /// This method does not perform rust codegen - that is performed by `cpp`
    /// and `cpp_macros`, which perform the actual procedural macro expansion.
    ///
    /// This method may technically be called more than once for ergonomic
    /// reasons, but that usually won't do what you want. Use a different
    /// `Config` object each time you want to build a crate.
    pub fn build<P: AsRef<Path>>(&mut self, crate_root: P) {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            VERSION,
            "Internal Error: mismatched cpp_common and cpp_build versions"
        );

        // Clean up any leftover artifacts
        clean_artifacts();

        // Parse the crate
        let mut sm = SourceMap::new();
        let krate = match sm.add_crate_root(crate_root) {
            Ok(krate) => krate,
            Err(err) => {
                let mut err_s = err.to_string();
                if let Some(i) = err_s.find("unparsed tokens after") {
                    // Strip the long error message from syn
                    err_s = err_s[0..i].to_owned();
                }
                warnln!(
                    r#"-- rust-cpp parse error --
There was an error parsing the crate for the rust-cpp build script:
{}
In order to provide a better error message, the build script will exit successfully, such that rustc can provide an error message."#,
                    err_s
                );
                return;
            }
        };

        // Parse the macro definitions
        let mut visitor = Handle {
            closures: Vec::new(),
            classes: Vec::new(),
            snippets: String::new(),
            sm: &sm,
            callbacks_count: 0,
        };
        visitor.visit_crate(&krate);

        // Generate the C++ library code
        let filename = gen_cpp_lib(&visitor);

        // Ensure C++11 mode is enabled. We rely on some C++11 construct, so we
        // must enable C++11 by default.
        // MSVC, GCC >= 5, Clang >= 6 defaults to C++14, but since we want to
        // supports older compiler which defaults to C++98, we need to
        // explicitly set the "-std" flag.
        // Ideally should be done by https://github.com/alexcrichton/cc-rs/issues/191
        if !self.std_flag_set {
            self.cc.flag_if_supported("-std=c++11");
        }

        // Build the C++ library
        self.cc.file(filename).compile(LIB_NAME);
    }
}

/// Run the `cpp` build process on the crate with a root at the given path.
/// Intended to be used within `build.rs` files.
pub fn build<P: AsRef<Path>>(path: P) {
    Config::new().build(path)
}

struct Handle<'a> {
    closures: Vec<Closure>,
    classes: Vec<Class>,
    snippets: String,
    sm: &'a SourceMap,
    callbacks_count: u32,
}

fn line_directive(span: syn::Span, sm: &SourceMap) -> String {
    let loc = sm.locinfo(span).unwrap();
    let mut line = format!("#line {} {:?}\n", loc.line, loc.path);
    for _ in 0..loc.col {
        line.push(' ');
    }
    return line;
}

fn extract_with_span(spanned: &mut Spanned<String>, src: &str, offset: usize, sm: &SourceMap) {
    if spanned.span != DUMMY_SPAN {
        let src_slice = &src[spanned.span.lo..spanned.span.hi];
        spanned.span.lo += offset;
        spanned.span.hi += offset;
        spanned.node = line_directive(spanned.span, sm);
        spanned.node.push_str(src_slice);
    }
}

impl<'a> Visitor for Handle<'a> {
    fn visit_mac(&mut self, mac: &Mac) {
        if mac.path.segments.len() != 1 {
            return;
        }
        if mac.path.segments[0].ident.as_ref() == "cpp" {
            assert!(mac.tts.len() == 1);
            self.handle_cpp(&mac.tts[0]);
        } else if mac.path.segments[0].ident.as_ref() == "cpp_class" {
            assert!(mac.tts.len() == 1);
            self.handle_cpp_class(&mac.tts[0]);
        } else {
            self.parse_macro(&mac.tts);
        }
    }
}

impl<'a> Handle<'a> {
    fn handle_cpp(&mut self, tt: &TokenTree) {
        let span = tt.span();
        let src = self.sm.source_text(span).unwrap();
        let input = synom::ParseState::new(&src);
        match parsing::build_macro(input)
            .expect(&format!("cpp! macro at {}", self.sm.locinfo(span).unwrap()))
        {
            Macro::Closure(mut c) => {
                extract_with_span(&mut c.body, &src, span.lo, self.sm);
                c.callback_offset = self.callbacks_count;
                c.body.node = expand_sub_rust_macro(
                    c.body.node,
                    ExpandSubMacroType::Closure(&mut self.callbacks_count),
                );
                self.closures.push(c);
            }
            Macro::Lit(mut l) => {
                extract_with_span(&mut l, &src, span.lo, self.sm);
                self.snippets.push('\n');
                self.snippets.push_str(&expand_sub_rust_macro(
                    l.node.clone(),
                    ExpandSubMacroType::Lit,
                ));
            }
        }
    }

    fn handle_cpp_class(&mut self, tt: &TokenTree) {
        let span = tt.span();
        let src = self.sm.source_text(span).unwrap();
        let input = synom::ParseState::new(&src);
        let mut class = parsing::class_macro(input).expect(&format!(
            "cpp_class! macro at {}",
            self.sm.locinfo(span).unwrap()
        ));
        class.line = line_directive(span, self.sm);
        self.classes.push(class);
    }

    fn parse_macro(&mut self, tts: &Vec<TokenTree>) {
        let mut last_ident: Option<&Ident> = None;
        let mut is_macro = false;
        for t in tts {
            match t {
                TokenTree::Token(Token::Not, _) => is_macro = true,
                TokenTree::Token(Token::Ident(ref i), _) => {
                    is_macro = false;
                    last_ident = Some(&i);
                }
                TokenTree::Delimited(ref d, _) => {
                    if is_macro && last_ident.map_or(false, |i| i.as_ref() == "cpp") {
                        self.handle_cpp(&t)
                    } else if is_macro && last_ident.map_or(false, |i| i.as_ref() == "cpp_class") {
                        self.handle_cpp_class(&t)
                    } else {
                        self.parse_macro(&d.tts)
                    }
                    is_macro = false;
                    last_ident = None;
                }
                _ => {
                    is_macro = false;
                    last_ident = None;
                }
            }
        }
    }
}
