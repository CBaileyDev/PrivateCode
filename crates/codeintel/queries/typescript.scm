; Symbol-extraction query for TypeScript.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @class, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.
;
; Plain patterns also match exported forms: `export function f(){}` parses as an
; export_statement wrapping a function_declaration, and the pattern matches the
; nested declaration regardless of the wrapper.

(function_declaration name: (identifier) @name) @function
(generator_function_declaration name: (identifier) @name) @function

(class_declaration name: (type_identifier) @name) @class
(abstract_class_declaration name: (type_identifier) @name) @class

(interface_declaration name: (type_identifier) @name) @interface

(enum_declaration name: (identifier) @name) @enum

(type_alias_declaration name: (type_identifier) @name) @type_alias

; Methods live in a class body; the name is a property_identifier.
(method_definition name: (property_identifier) @name) @method

; `const NAME = …`: anchor on each variable_declarator so multi-binding consts
; (`const a = 1, b = 2`) yield one symbol per binding (exactly one @name each).
; Scoped to const via the enclosing lexical_declaration kind.
(lexical_declaration
  "const"
  (variable_declarator name: (identifier) @name) @const)
