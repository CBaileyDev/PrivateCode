; Symbol-extraction query for Go.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @struct, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.

; Top-level functions and methods. (method_declaration's name is a field_identifier.)
(function_declaration name: (identifier) @name) @function
(method_declaration name: (field_identifier) @name) @method

; Named types. struct_type → @struct, interface_type → @interface; the dedicated
; `type Foo = Bar` form is type_alias. These never overlap (struct/interface use
; type_spec; aliases use type_alias), so a type is emitted exactly once.
(type_spec name: (type_identifier) @name type: (struct_type)) @struct
(type_spec name: (type_identifier) @name type: (interface_type)) @interface
(type_alias name: (type_identifier) @name) @type_alias

; Constants. A const_spec may declare several names (const A, B = …); the pattern
; binds @name to each, emitting one const symbol per name.
(const_spec name: (identifier) @name) @const
