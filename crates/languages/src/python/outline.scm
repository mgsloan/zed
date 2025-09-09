(decorator) @annotation

(class_definition
    "class" @context
    name: (identifier) @name
    superclasses: (_)? @signature) @item

(function_definition
    "async"? @context
    "def" @context
    name: (_) @name
    parameters: (_) @signature
    return_type: (_)? @signature) @item
