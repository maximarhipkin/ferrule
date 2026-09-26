//! The languages the map understands: a grammar, a tag query, and the node
//! kinds that make a definition local. One Cargo feature per grammar.

use crate::tags::Kind;

/// A language compiled into this build.
pub struct Lang {
    pub name: &'static str,
    pub grammar: fn() -> tree_sitter::Language,
    /// Captures: `@name` is the identifier; `@def.<kind>` marks a
    /// definition (the node whose first line is shown), `@ref` a reference.
    /// When one name node is captured by several patterns the earliest
    /// pattern wins, so specific patterns come first. Parts are joined.
    pub query: &'static [&'static str],
    /// A definition below one of these is local (a function body).
    pub local_scopes: &'static [&'static str],
    /// A function below one of these is a method.
    pub containers: &'static [&'static str],
}

/// Extensions of source files, whether or not their grammar is in this
/// build: what counts towards "this looks like a code repo".
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "ts", "mts", "cts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java",
];

/// The language for a file extension, if its grammar is compiled in.
pub fn for_extension(ext: &str) -> Option<&'static Lang> {
    match ext {
        #[cfg(feature = "lang-rust")]
        "rs" => Some(&RUST),
        #[cfg(feature = "lang-python")]
        "py" | "pyi" => Some(&PYTHON),
        #[cfg(feature = "lang-typescript")]
        "ts" | "mts" | "cts" => Some(&TYPESCRIPT),
        #[cfg(feature = "lang-typescript")]
        "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(&TSX),
        #[cfg(feature = "lang-go")]
        "go" => Some(&GO),
        #[cfg(feature = "lang-java")]
        "java" => Some(&JAVA),
        _ => None,
    }
}

/// Every language compiled in.
pub fn all() -> Vec<&'static Lang> {
    #[allow(unused_mut)]
    let mut out: Vec<&'static Lang> = Vec::new();
    #[cfg(feature = "lang-rust")]
    out.push(&RUST);
    #[cfg(feature = "lang-python")]
    out.push(&PYTHON);
    #[cfg(feature = "lang-typescript")]
    out.extend([&TYPESCRIPT, &TSX]);
    #[cfg(feature = "lang-go")]
    out.push(&GO);
    #[cfg(feature = "lang-java")]
    out.push(&JAVA);
    out
}

pub(crate) fn kind_of(capture: &str) -> Option<Kind> {
    Some(match capture.strip_prefix("def.")? {
        "function" => Kind::Function,
        "method" => Kind::Method,
        "class" => Kind::Class,
        "struct" => Kind::Struct,
        "enum" => Kind::Enum,
        "trait" => Kind::Trait,
        "interface" => Kind::Interface,
        "type" => Kind::Type,
        "module" => Kind::Module,
        "const" => Kind::Const,
        "macro" => Kind::Macro,
        _ => return None,
    })
}

#[cfg(feature = "lang-rust")]
static RUST: Lang = Lang {
    name: "rust",
    grammar: || tree_sitter_rust::LANGUAGE.into(),
    query: &[r#"
(function_item name: (identifier) @name) @def.function
(function_signature_item name: (identifier) @name) @def.method
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(trait_item name: (type_identifier) @name) @def.trait
(type_item name: (type_identifier) @name) @def.type
(mod_item name: (identifier) @name) @def.module
(const_item name: (identifier) @name) @def.const
(static_item name: (identifier) @name) @def.const
(macro_definition name: (identifier) @name) @def.macro

(call_expression function: (identifier) @name) @ref
(call_expression function: (field_expression field: (field_identifier) @name)) @ref
(call_expression function: (scoped_identifier name: (identifier) @name)) @ref
(call_expression function: (generic_function function: (identifier) @name)) @ref
(call_expression function: (generic_function function: (field_expression field: (field_identifier) @name))) @ref
(call_expression function: (generic_function function: (scoped_identifier name: (identifier) @name))) @ref
(scoped_identifier path: (identifier) @name) @ref
(scoped_type_identifier path: (identifier) @name) @ref
(macro_invocation macro: (identifier) @name) @ref
(use_declaration argument: (scoped_identifier name: (identifier) @name)) @ref
(use_declaration argument: (identifier) @name) @ref
(use_list (identifier) @name) @ref
(use_list (scoped_identifier name: (identifier) @name)) @ref
(use_as_clause path: (scoped_identifier name: (identifier) @name)) @ref
(use_as_clause path: (identifier) @name) @ref
(type_identifier) @name @ref
"#],
    local_scopes: &["block"],
    containers: &["impl_item", "trait_item"],
};

#[cfg(feature = "lang-python")]
static PYTHON: Lang = Lang {
    name: "python",
    grammar: || tree_sitter_python::LANGUAGE.into(),
    query: &[r#"
(class_definition name: (identifier) @name) @def.class
(function_definition name: (identifier) @name) @def.function
(module (expression_statement (assignment left: (identifier) @name) @def.const)
  (#match? @name "^[A-Z][A-Z0-9_]*$"))

(call function: (identifier) @name) @ref
(call function: (attribute attribute: (identifier) @name)) @ref
(decorator (identifier) @name) @ref
(decorator (call function: (identifier) @name)) @ref
(class_definition superclasses: (argument_list (identifier) @name)) @ref
(class_definition superclasses: (argument_list (attribute attribute: (identifier) @name))) @ref
(import_from_statement name: (dotted_name (identifier) @name)) @ref
(import_from_statement name: (aliased_import name: (dotted_name (identifier) @name))) @ref
(type (identifier) @name) @ref
(type (generic_type (identifier) @name)) @ref
(type (subscript value: (identifier) @name)) @ref
"#],
    local_scopes: &["function_definition", "lambda"],
    containers: &["class_definition"],
};

#[cfg(feature = "lang-typescript")]
const TS_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(function_signature name: (identifier) @name) @def.function
(class_declaration name: (type_identifier) @name) @def.class
(abstract_class_declaration name: (type_identifier) @name) @def.class
(method_definition name: (property_identifier) @name) @def.method
(method_signature name: (property_identifier) @name) @def.method
(abstract_method_signature name: (property_identifier) @name) @def.method
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.type
(enum_declaration name: (identifier) @name) @def.enum
(internal_module name: (identifier) @name) @def.module
(lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @def.function
(variable_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @def.function
(lexical_declaration (variable_declarator name: (identifier) @name) (#match? @name "^[A-Z][A-Z0-9_]*$")) @def.const

(call_expression function: (identifier) @name) @ref
(call_expression function: (member_expression property: (property_identifier) @name)) @ref
(new_expression constructor: (identifier) @name) @ref
(new_expression constructor: (member_expression property: (property_identifier) @name)) @ref
(import_specifier name: (identifier) @name) @ref
(import_clause (identifier) @name) @ref
(extends_clause value: (identifier) @name) @ref
(type_identifier) @name @ref
"#;

#[cfg(feature = "lang-typescript")]
const TSX_QUERY_EXTRA: &str = r#"
(jsx_opening_element name: (identifier) @name) @ref
(jsx_self_closing_element name: (identifier) @name) @ref
"#;

#[cfg(feature = "lang-typescript")]
static TYPESCRIPT: Lang = Lang {
    name: "typescript",
    grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    query: &[TS_QUERY],
    local_scopes: &["statement_block"],
    containers: &["class_body", "interface_body", "object_type"],
};

#[cfg(feature = "lang-typescript")]
static TSX: Lang = Lang {
    name: "tsx",
    grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
    query: &[TS_QUERY, TSX_QUERY_EXTRA],
    local_scopes: &["statement_block"],
    containers: &["class_body", "interface_body", "object_type"],
};

#[cfg(feature = "lang-go")]
static GO: Lang = Lang {
    name: "go",
    grammar: || tree_sitter_go::LANGUAGE.into(),
    query: &[r#"
(function_declaration name: (identifier) @name) @def.function
(method_declaration name: (field_identifier) @name) @def.method
(type_spec name: (type_identifier) @name type: (struct_type)) @def.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @def.interface
(type_spec name: (type_identifier) @name) @def.type
(type_alias name: (type_identifier) @name) @def.type
(method_elem name: (field_identifier) @name) @def.method
(const_spec name: (identifier) @name) @def.const

(call_expression function: (identifier) @name) @ref
(call_expression function: (selector_expression field: (field_identifier) @name)) @ref
(type_identifier) @name @ref
"#],
    local_scopes: &["block"],
    containers: &["interface_type"],
};

#[cfg(feature = "lang-java")]
static JAVA: Lang = Lang {
    name: "java",
    grammar: || tree_sitter_java::LANGUAGE.into(),
    query: &[r#"
(class_declaration name: (identifier) @name) @def.class
(record_declaration name: (identifier) @name) @def.class
(interface_declaration name: (identifier) @name) @def.interface
(annotation_type_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(method_declaration name: (identifier) @name) @def.method
(constructor_declaration name: (identifier) @name) @def.method

(method_invocation name: (identifier) @name) @ref
(object_creation_expression type: (type_identifier) @name) @ref
(import_declaration (scoped_identifier name: (identifier) @name)) @ref
(type_identifier) @name @ref
"#],
    local_scopes: &["block", "constructor_body"],
    containers: &[],
};
