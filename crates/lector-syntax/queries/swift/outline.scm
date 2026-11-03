(class_declaration declaration_kind: "class" name: (_) @name) @item.class
(class_declaration declaration_kind: "actor" name: (_) @name) @item.class
(class_declaration declaration_kind: "struct" name: (_) @name) @item.struct
(class_declaration declaration_kind: "enum" name: (_) @name) @item.enum
(class_declaration declaration_kind: "extension" @context name: (_) @name) @item.impl
(protocol_declaration name: (type_identifier) @name) @item.interface
(function_declaration name: (simple_identifier) @name) @item.function
(protocol_function_declaration name: (simple_identifier) @name) @item.function
(init_declaration "init" @name) @item.method
(deinit_declaration "deinit" @name) @item.method
(property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @item.variable
(protocol_property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @item.field
(enum_entry name: (simple_identifier) @name) @item.field
(typealias_declaration name: (type_identifier) @name) @item.type
(associatedtype_declaration name: (type_identifier) @name) @item.type
