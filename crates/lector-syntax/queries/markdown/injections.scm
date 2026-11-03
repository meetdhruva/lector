((fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content)
 (#set! injection.include-children))

((html_block) @injection.content
  (#set! injection.language "html")
  (#set! injection.include-children))

((minus_metadata) @injection.content
  (#set! injection.language "yaml")
  (#set! injection.include-children))

((plus_metadata) @injection.content
  (#set! injection.language "toml")
  (#set! injection.include-children))

((inline) @injection.content
  (#set! injection.language "markdown_inline")
  (#set! injection.include-children))
