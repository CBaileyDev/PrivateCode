; Symbol-extraction query for Ruby.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@method, @class, …). The extractor reads the kind
; from the non-"name" capture and the span/signature from its node.

(method name: (identifier) @name) @method
(singleton_method name: (identifier) @name) @method
(class name: (constant) @name) @class
(module name: (constant) @name) @module

; Top-level constant assignments (Ruby has no dedicated const node; an uppercase
; LHS is a constant by convention). Bound class/object values count too.
(assignment left: (constant) @name) @const
