(function_declaration name: (identifier) @name) @item.function
(method_declaration receiver: (parameter_list) @context name: (field_identifier) @name) @item.method
(type_spec name: (type_identifier) @name type: (struct_type)) @item.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @item.interface
(type_spec name: (type_identifier) @name type: [(type_identifier) (qualified_type) (pointer_type) (slice_type) (array_type) (map_type) (channel_type) (function_type) (generic_type)]) @item.type
(type_alias name: (type_identifier) @name) @item.type
(const_spec name: (identifier) @name) @item.constant
(var_spec name: (identifier) @name) @item.variable
(field_declaration name: (field_identifier) @name) @item.field
(method_elem name: (field_identifier) @name) @item.method
