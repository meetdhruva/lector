(namespace_declaration name: (_) @name) @item.module
(file_scoped_namespace_declaration name: (_) @name) @item.module
(class_declaration name: (identifier) @name) @item.class
(record_declaration name: (identifier) @name) @item.class
(struct_declaration name: (identifier) @name) @item.struct
(interface_declaration name: (identifier) @name) @item.interface
(enum_declaration name: (identifier) @name) @item.enum
(enum_member_declaration name: (identifier) @name) @item.field
(delegate_declaration name: (identifier) @name) @item.type
(method_declaration name: (identifier) @name) @item.method
(constructor_declaration name: (identifier) @name) @item.method
(destructor_declaration name: (identifier) @name) @item.method
(property_declaration name: (identifier) @name) @item.field
(event_field_declaration (variable_declaration (variable_declarator (identifier) @name))) @item.field
(field_declaration (variable_declaration (variable_declarator (identifier) @name))) @item.field
