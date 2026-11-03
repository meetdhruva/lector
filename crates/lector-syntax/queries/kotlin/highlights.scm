; Highlights for tree-sitter-kotlin-ng (written for Lector; MIT OR Apache-2.0).

; Identifiers (lowest priority: later patterns override)
(identifier) @variable

((identifier) @boolean
  (#any-of? @boolean "true" "false"))
((identifier) @constant.builtin
  (#eq? @constant.builtin "null"))
((identifier) @constant
  (#match? @constant "^[A-Z][A-Z0-9_]+$"))
((identifier) @type
  (#match? @type "^[A-Z][a-z]"))

; Declarations
(class_declaration name: (identifier) @type)
(object_declaration name: (identifier) @type)
(companion_object name: (identifier) @type)
(type_alias type: (identifier) @type)
(function_declaration name: (identifier) @function)
(enum_entry (identifier) @constant)

; Types
(user_type (identifier) @type)
(type_parameter (identifier) @type)

; Parameters and properties
(parameter (identifier) @variable.parameter)
(class_parameter (identifier) @variable.parameter)
(lambda_parameters (variable_declaration (identifier) @variable.parameter))
(navigation_expression (identifier) @property .)
(value_argument (identifier) @variable.parameter)

; Calls
(call_expression (identifier) @function)
(call_expression (navigation_expression (identifier) @function.method .))
(constructor_invocation (type (user_type (identifier) @constructor)))

; Packages and imports
(package_header (qualified_identifier (identifier) @module))
(import (qualified_identifier (identifier) @module))

; Builtins
(this_expression) @variable.builtin
(super_expression) @variable.builtin

; Annotations and labels
(annotation) @attribute
(file_annotation) @attribute
(label) @label

; Literals
(number_literal) @number
(float_literal) @number
(character_literal) @string
(string_literal) @string
(multiline_string_literal) @string
(escape_sequence) @string.escape
(interpolation ["$" "${" "}"] @punctuation.special)
(interpolation (identifier) @variable)

; Comments
(line_comment) @comment
(block_comment) @comment
(shebang) @comment

; Keywords
["abstract" "actual" "annotation" "by" "class" "companion" "const" "constructor"
 "crossinline" "data" "delegate" "dynamic" "enum" "expect" "external" "field"
 "file" "final" "fun" "get" "inline" "inner" "interface" "internal" "lateinit"
 "noinline" "object" "open" "operator" "out" "override" "param" "private"
 "property" "protected" "public" "receiver" "sealed" "set" "setparam" "suspend"
 "tailrec" "val" "value" "var" "vararg" "where" "init" "infix"
 "typealias"] @keyword

["if" "else" "when" "for" "while" "do" "try" "catch" "finally" "throw"] @keyword.control
["return" "return@"] @keyword.return
["import" "package"] @keyword.import
["as" "as?" "is" "!is" "in" "!in"] @keyword.operator

; Operators
["+" "-" "*" "/" "%" "++" "--" "=" "+=" "-=" "*=" "/=" "%=" "==" "!=" "===" "!=="
 "<" ">" "<=" ">=" "&&" "||" "!" "!!" "?" "?." "?:" "->" ".." "..<" "&"] @operator

; Punctuation
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ";" ":" "::"] @punctuation.delimiter
