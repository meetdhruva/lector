(class_definition name: (identifier) @name) @item.class
(function_definition name: (identifier) @name) @item.function
(module
  (expression_statement
    (assignment left: (identifier) @name)) @item.variable)
(module
  (expression_statement
    (assignment left: (identifier) @name)) @item.constant
  (#match? @name "^[A-Z][A-Z0-9_]*$"))
(class_definition
  body: (block
    (expression_statement
      (assignment left: (identifier) @name)) @item.field))
