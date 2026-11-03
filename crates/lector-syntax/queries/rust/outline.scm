(mod_item name: (identifier) @name) @item.module
(struct_item name: (type_identifier) @name) @item.struct
(union_item name: (type_identifier) @name) @item.struct
(enum_item name: (type_identifier) @name) @item.enum
(enum_variant name: (identifier) @name) @item.field
(trait_item name: (type_identifier) @name) @item.trait
(impl_item
  "impl" @context
  trait: (_)? @context
  "for"? @context
  type: (_) @name) @item.impl
(function_item name: (identifier) @name) @item.function
(function_signature_item name: (identifier) @name) @item.function
(const_item name: (identifier) @name) @item.constant
(static_item name: (identifier) @name) @item.variable
(type_item name: (type_identifier) @name) @item.type
(associated_type name: (type_identifier) @name) @item.type
(macro_definition name: (identifier) @name) @item.macro
(field_declaration name: (field_identifier) @name) @item.field
