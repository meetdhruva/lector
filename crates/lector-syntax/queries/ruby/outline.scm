(class name: (_) @name) @item.class
(module name: (_) @name) @item.module
(singleton_class value: (_) @name) @item.class
(method name: (_) @name) @item.method
(singleton_method name: (_) @name) @item.method
(program (assignment left: (constant) @name) @item.constant)
(class body: (body_statement (assignment left: (constant) @name) @item.constant))
(module body: (body_statement (assignment left: (constant) @name) @item.constant))
