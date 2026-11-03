(function_definition declarator: (function_declarator declarator: (identifier) @name)) @item.function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @item.function
(function_definition declarator: (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)))) @item.function
(struct_specifier name: (type_identifier) @name body: (_)) @item.struct
(union_specifier name: (type_identifier) @name body: (_)) @item.struct
(enum_specifier name: (type_identifier) @name body: (_)) @item.enum
(enumerator name: (identifier) @name) @item.field
(field_declaration declarator: (field_identifier) @name) @item.field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @item.field
(type_definition declarator: (type_identifier) @name) @item.type
(preproc_def name: (identifier) @name) @item.constant
(preproc_function_def name: (identifier) @name) @item.macro
