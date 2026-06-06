; Symbol-extraction query for PHP.
; Each pattern captures the symbol's name (@name) plus the whole definition node
; tagged with a kind capture (@function, @class, …). The extractor reads the
; kind from the non-"name" capture and the span/signature from its node.
;
; PHP's identifier node type is `name` (not `identifier`). The grammar
; (LANGUAGE_PHP) parses mixed HTML/PHP, so source must contain a `<?php` open
; tag for these nodes to appear.

(function_definition name: (name) @name) @function

; Methods live in a class/interface/trait body.
(method_declaration name: (name) @name) @method

(class_declaration name: (name) @name) @class
(interface_declaration name: (name) @name) @interface
(enum_declaration name: (name) @name) @enum

; PHP traits have no dedicated kind tag; map to @class per the allowed set.
(trait_declaration name: (name) @name) @class

; `const NAME = …;` — the name lives on each const_element (no `name` field on
; const_declaration), so anchor per element to yield one symbol per binding.
(const_declaration (const_element (name) @name) @const)

; `namespace App\Models;` — the name is a namespace_name (dotted/backslashed),
; whose full text is the module identifier.
(namespace_definition name: (namespace_name) @name) @module
