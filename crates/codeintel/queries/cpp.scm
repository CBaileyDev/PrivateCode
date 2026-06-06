; Symbol-extraction query for C++.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @class, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.
; Exactly two captures per match: @name on the identifier, the kind on the def.

; Free functions: void foo() { ... }  (declarator nests under function_declarator)
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @function

; In-class method definitions: the name is a field_identifier
(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @method

; Out-of-line definitions: Foo::bar() { ... } — take the rightmost name
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier
      name: (identifier) @name))) @method

; Method DECLARATIONS in headers: the function_declarator wrapper distinguishes
; a method from a plain data member (whose declarator is a bare field_identifier).
(field_declaration
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @method

(class_specifier name: (type_identifier) @name) @class
(struct_specifier name: (type_identifier) @name) @struct
(union_specifier name: (type_identifier) @name) @struct
(enum_specifier name: (type_identifier) @name) @enum
(namespace_definition name: (namespace_identifier) @name) @module

; Type aliases: `using X = Y;` and the simple `typedef Y X;` form.
(alias_declaration name: (type_identifier) @name) @type_alias
(type_definition declarator: (type_identifier) @name) @type_alias
