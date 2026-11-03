(element
  (start_tag (tag_name) @_tag)
  .
  (text) @name
  (#match? @_tag "^h[1-6]$")) @item.heading
(element
  (start_tag
    (tag_name) @context
    (attribute
      (attribute_name) @_attr
      (quoted_attribute_value (attribute_value) @name)))
  (#eq? @_attr "id")) @item.other
