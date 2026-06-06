; Symbol-extraction query for C.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @struct, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.

; Function definitions: the identifier lives inside the function_declarator.
; Cover the plain form and the pointer-returning form (pointer_declarator wraps
; the function_declarator).
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (identifier) @name))) @function

; Named struct / union / enum specifiers.
(struct_specifier name: (type_identifier) @name) @struct
(union_specifier name: (type_identifier) @name) @struct
(enum_specifier name: (type_identifier) @name) @enum

; typedefs: the new type name is the (possibly innermost) type_identifier.
(type_definition declarator: (type_identifier) @name) @type_alias

; #define object-like macros (treated as constants).
(preproc_def name: (identifier) @name) @const
