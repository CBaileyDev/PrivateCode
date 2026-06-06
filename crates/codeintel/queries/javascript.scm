; Symbol-extraction query for JavaScript.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @class, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.

(function_declaration name: (identifier) @name) @function
(generator_function_declaration name: (identifier) @name) @function

(class_declaration name: (identifier) @name) @class

; Methods inside a class body. The name is a property_identifier.
(method_definition name: (property_identifier) @name) @method

; `const NAME = ...` only (not let/var). Restricting on the "const" keyword and
; an identifier binding skips destructuring patterns we can't name cleanly.
(lexical_declaration
  "const"
  (variable_declarator name: (identifier) @name)) @const
