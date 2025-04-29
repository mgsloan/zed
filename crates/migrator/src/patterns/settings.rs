pub const SETTINGS_ROOT_KEY_VALUE_PATTERN: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @name)
            value: (_)  @value
        )
    )
)"#;

pub const SETTINGS_NESTED_KEY_VALUE_PATTERN: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @parent_key)
            value: (object
                (pair
                    key: (string (string_content) @setting_name)
                    value: (_) @setting_value
                )
            )
        )
    )
)"#;

pub const SETTINGS_LANGUAGES_PATTERN: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @languages)
            value: (object
            (pair
                key: (string)
                value: (object
                    (pair
                        key: (string (string_content) @setting_name)
                        value: (_) @value
                    )
                )
            ))
        )
    )
    (#eq? @languages "languages")
)"#;

pub const SETTINGS_ASSISTANT_TOOLS_PATTERN: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @assistant)
            value: (object
                (pair
                    key: (string (string_content) @profiles)
                    value: (object
                        (pair
                            key: (_)
                            value: (object
                                (pair
                                    key: (string (string_content) @tools_key)
                                    value: (object
                                        (pair
                                            key: (string (string_content) @tool_name)
                                            value: (_) @tool_value
                                        )
                                    )
                                )
                            )
                        )
                    )
                )
            )
        )
    )
    (#eq? @assistant "assistant")
    (#eq? @profiles "profiles")
    (#eq? @tools_key "tools")
)"#;

pub const SETTINGS_ASSISTANT_MODEL_NAMES: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @assistant)
            value: (object
                (pair
                    key: (string (string_content) @model_type)
                    value: (object
                        (pair
                            key: (string (string_content) @model)
                            value: (string (string_content) @model_name)
                        )
                    )
                )
            )
        )
    )
    (#eq? @assistant "assistant")
    (#any-of? @model_type
        "default_model"
        "inline_assistant_model"
        "commit_message_model"
        "thread_summary_model")
    (#eq? @model "model")
)"#;

pub const SETTINGS_ASSISTANT_INLINE_ALTERNATIVE_MODEL_NAMES: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @assistant)
            value: (object
                (pair
                    key: (string (string_content) @inline_alternatives)
                    value: (array
                        (object
                            (pair
                                key: (string (string_content) @model)
                                value: (string (string_content) @model_name)
                            )
                        )
                    )
                )
            )
        )
    )
    (#eq? @assistant "assistant")
    (#eq? @inline_alternatives "inline_alternatives")
    (#eq? @model "model")
)"#;

pub const SETTINGS_LANGUAGE_MODELS_AVAILABLE_MODEL_NAMES: &str = r#"(document
    (object
        (pair
            key: (string (string_content) @language_models)
            value: (object
                (pair
                    key: (string (string_content) @provider_name)
                    value: (object
                        (pair
                            key: (string (string_content) @available_models)
                            value: (array
                                (object
                                    (pair
                                        key: (string (string_content) @name)
                                        value: (string (string_content) @model_name)
                                    )
                                )
                            )
                        )
                    )
                )
            )
        )
    )
    (#eq? @language_models "language_models")
    (#eq? @available_models "available_models")
    (#eq? @name "name")
)"#;
