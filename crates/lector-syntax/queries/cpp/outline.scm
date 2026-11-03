(namespace_definition name: (_) @name) @item.module
(class_specifier name: (_) @name body: (_)) @item.class
(struct_specifier name: (_) @name body: (_)) @item.struct
(union_specifier name: (_) @name body: (_)) @item.struct
(enum_specifier name: (_) @name body: (_)) @item.enum
(enumerator name: (identifier) @name) @item.field
(function_definition declarator: (function_declarator declarator: (_) @name)) @item.function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name))) @item.function
(function_definition declarator: (reference_declarator (function_declarator declarator: (_) @name))) @item.function
(function_definition declarator: (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name)))) @item.function
(field_declaration declarator: (field_identifier) @name) @item.field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @item.field
(field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @item.method
(type_definition declarator: (type_identifier) @name) @item.type
(alias_declaration name: (type_identifier) @name) @item.type
(concept_definition name: (identifier) @name) @item.type
(preproc_def name: (identifier) @name) @item.constant
(preproc_function_def name: (identifier) @name) @item.macro
