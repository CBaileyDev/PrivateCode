; Symbol-extraction query for Python.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @class). The extractor reads the kind
; from the non-"name" capture and the span/signature from its node (start → the
; node's "body" field).

(function_definition name: (identifier) @name) @function
(class_definition name: (identifier) @name) @class
