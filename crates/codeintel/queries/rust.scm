; Symbol-extraction query for Rust.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @struct, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.

(function_item name: (identifier) @name) @function
(struct_item name: (type_identifier) @name) @struct
(trait_item name: (type_identifier) @name) @trait
(enum_item name: (type_identifier) @name) @enum
(union_item name: (type_identifier) @name) @struct
(type_item name: (type_identifier) @name) @type_alias
(const_item name: (identifier) @name) @const
(static_item name: (identifier) @name) @const
(mod_item name: (identifier) @name) @module
(macro_definition name: (identifier) @name) @macro

; impls: the "name" is the implementing type. Cover the plain and generic forms.
(impl_item type: (type_identifier) @name) @impl
(impl_item type: (generic_type type: (type_identifier) @name)) @impl
