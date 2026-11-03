(class_declaration name: (identifier) @name) @item.class
(class name: (identifier) @name) @item.class
(function_declaration name: (identifier) @name) @item.function
(generator_function_declaration name: (identifier) @name) @item.function
(method_definition name: (_) @name) @item.method
(field_definition property: (_) @name) @item.field
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name)) @item.variable)
(program
  (variable_declaration
    (variable_declarator name: (identifier) @name)) @item.variable)
(program
  (export_statement
    (lexical_declaration
      (variable_declarator name: (identifier) @name)) @item.variable))
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression) (generator_function)])) @item.function
(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression) (generator_function)])) @item.function
(program
  (lexical_declaration
    "const"
    (variable_declarator name: (identifier) @name
      value: [(number) (string) (template_string) (true) (false) (null) (array) (object) (new_expression) (call_expression)])) @item.constant)
(program
  (export_statement
    (lexical_declaration
      "const"
      (variable_declarator name: (identifier) @name
        value: [(number) (string) (template_string) (true) (false) (null) (array) (object) (new_expression) (call_expression)])) @item.constant))
