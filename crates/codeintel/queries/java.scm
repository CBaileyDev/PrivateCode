; Symbol-extraction query for Java.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@class, @method, …). The extractor reads the kind
; from the non-"name" capture and the span/signature from its node, so every
; pattern captures exactly two things: @name and one kind tag.

(class_declaration name: (identifier) @name) @class
(record_declaration name: (identifier) @name) @class
(interface_declaration name: (identifier) @name) @interface
(annotation_type_declaration name: (identifier) @name) @interface
(enum_declaration name: (identifier) @name) @enum
(method_declaration name: (identifier) @name) @method
(constructor_declaration name: (identifier) @name) @method

; Constants: only `final` fields (a structural constraint, not a capture, since
; the extractor ignores predicates). Each declarator yields one symbol.
(field_declaration
  (modifiers "final")
  declarator: (variable_declarator name: (identifier) @name)) @const
