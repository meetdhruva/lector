((script_element
  (raw_text) @injection.content)
 (#set! injection.language "javascript")
 (#set! injection.include-children))

((style_element
  (raw_text) @injection.content)
 (#set! injection.language "css")
 (#set! injection.include-children))
