(class_declaration name: (identifier) @name) @definition.class
(class_declaration "interface" name: (identifier) @name) @definition.interface
(object_declaration name: (identifier) @name) @definition.class
(function_declaration name: (identifier) @name) @definition.function
(type_alias type: (identifier) @name) @definition.type
(enum_entry (identifier) @name) @definition.constant
(call_expression (identifier) @name) @reference.call
(call_expression (navigation_expression (identifier) @name .)) @reference.call
(user_type (identifier) @name) @reference.type
