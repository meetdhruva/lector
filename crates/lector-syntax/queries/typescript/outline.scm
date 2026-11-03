(class_declaration name: (type_identifier) @name) @item.class
(abstract_class_declaration name: (type_identifier) @name) @item.class
(interface_declaration name: (type_identifier) @name) @item.interface
(enum_declaration name: (identifier) @name) @item.enum
(type_alias_declaration name: (type_identifier) @name) @item.type
(internal_module name: (_) @name) @item.module
(module name: (_) @name) @item.module
(function_declaration name: (identifier) @name) @item.function
(generator_function_declaration name: (identifier) @name) @item.function
(function_signature name: (identifier) @name) @item.function
(method_definition name: (_) @name) @item.method
(method_signature name: (_) @name) @item.method
(abstract_method_signature name: (_) @name) @item.method
(public_field_definition name: (_) @name) @item.field
(property_signature name: (_) @name) @item.field
(enum_assignment name: (_) @name) @item.field
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
      value: [(number) (string) (template_string) (true) (false) (null) (array) (object) (new_expression) (call_expression) (as_expression)])) @item.constant)
(program
  (export_statement
    (lexical_declaration
      "const"
      (variable_declarator name: (identifier) @name
        value: [(number) (string) (template_string) (true) (false) (null) (array) (object) (new_expression) (call_expression) (as_expression)])) @item.constant))
