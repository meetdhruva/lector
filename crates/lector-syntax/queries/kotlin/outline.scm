(class_declaration name: (identifier) @name) @item.class
(class_declaration "interface" name: (identifier) @name) @item.interface
(class_declaration (modifiers (class_modifier "enum")) name: (identifier) @name) @item.enum
(object_declaration name: (identifier) @name) @item.class
(companion_object "companion" @name) @item.class
(function_declaration name: (identifier) @name) @item.function
(secondary_constructor "constructor" @name) @item.method
(property_declaration (variable_declaration (identifier) @name)) @item.variable
(property_declaration (modifiers (property_modifier "const")) (variable_declaration (identifier) @name)) @item.constant
(enum_entry (identifier) @name) @item.field
(type_alias type: (identifier) @name) @item.type
