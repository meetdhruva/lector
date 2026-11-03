(package_declaration (_) @name) @item.module
(class_declaration name: (identifier) @name) @item.class
(record_declaration name: (identifier) @name) @item.class
(interface_declaration name: (identifier) @name) @item.interface
(annotation_type_declaration name: (identifier) @name) @item.interface
(enum_declaration name: (identifier) @name) @item.enum
(enum_constant name: (identifier) @name) @item.field
(method_declaration name: (identifier) @name) @item.method
(constructor_declaration name: (identifier) @name) @item.method
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @item.field
