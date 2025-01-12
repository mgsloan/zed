use std::{ops::Range, path::Path, rc::Rc};

use crate::{
    settings_diagnostics::{SettingsDiagnostic, SettingsPathRef},
    settings_store::parse_json_with_comments,
    SettingsAssets,
};
use anyhow::{anyhow, Result};
use collections::{BTreeMap, HashMap};
use gpui::{Action, AppContext, KeyBinding, KeyBindingContextPredicate, SharedString};
use json_spanned_value::Spanned;
use schemars::{
    gen::{SchemaGenerator, SchemaSettings},
    schema::{InstanceType, Schema, SchemaObject, SingleOrVec, SubschemaValidation},
    JsonSchema, Map,
};
use serde::Deserialize;
use serde_json::Value;
use util::asset_str;

#[derive(Debug, Deserialize, Default, Clone, JsonSchema)]
#[serde(transparent)]
pub struct KeymapFile(Vec<KeymapBlock>);

#[derive(Debug, Deserialize, Default, Clone, JsonSchema)]
pub struct KeymapBlock {
    #[serde(default)]
    context: Option<Spanned<String>>,
    #[serde(default)]
    use_key_equivalents: Option<bool>,
    bindings: BTreeMap<Spanned<String>, Spanned<KeymapAction>>,
}

impl KeymapBlock {
    pub fn bindings(&self) -> impl Iterator<Item = (&str, &KeymapAction)> {
        self.bindings
            .iter()
            .map(|(keystrokes, action)| (keystrokes.get_ref().as_ref(), action.get_ref()))
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(transparent)]
pub struct KeymapAction(Value);

impl std::fmt::Display for KeymapAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Value::String(s) => write!(f, "{}", s),
            Value::Array(arr) => {
                let strings: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
                write!(f, "{}", strings.join(", "))
            }
            _ => write!(f, "{}", self.0),
        }
    }
}

impl JsonSchema for KeymapAction {
    fn schema_name() -> String {
        "KeymapAction".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        Schema::Bool(true)
    }
}

impl KeymapFile {
    pub fn load_builtin(asset_path: &str, cx: &mut AppContext) -> Result<()> {
        let content = asset_str::<SettingsAssets>(&asset_path);
        let settings_path = SettingsPathRef::Builtin(asset_path);
        let keymap_file = Self::parse(settings_path, &content)?;
        keymap_file.register_bindings(settings_path, &content, cx)
    }

    pub fn parse_builtin(asset_path: &str) -> Result<KeymapFile> {
        let content = asset_str::<SettingsAssets>(&asset_path);
        let settings_path = SettingsPathRef::Builtin(asset_path);
        Self::parse(settings_path, &content)
    }

    pub fn parse_user(path: &Path, content: &str) -> Result<KeymapFile> {
        let settings_path = SettingsPathRef::Path(path);
        Self::parse(settings_path, &content)
    }

    fn parse(settings_path: SettingsPathRef, content: &str) -> Result<Self> {
        if content.is_empty() {
            return Ok(Self::default());
        }
        parse_json_with_comments::<Self>(content)
            .map_err(|err| anyhow!("Error in {settings_path}: {err}"))
    }

    pub fn register_builtin_bindings(
        &self,
        asset_path: &str,
        content: &str,
        cx: &mut AppContext,
    ) -> Result<()> {
        self.register_bindings(SettingsPathRef::Builtin(asset_path), content, cx)
    }

    pub fn register_user_bindings(
        &self,
        path: &Path,
        content: &str,
        cx: &mut AppContext,
    ) -> Result<()> {
        self.register_bindings(SettingsPathRef::Path(path), content, cx)
    }

    fn register_bindings(
        &self,
        settings_path: SettingsPathRef,
        content: &str,
        cx: &mut AppContext,
    ) -> Result<()> {
        let key_equivalents = crate::key_equivalents::get_key_equivalents(&cx.keyboard_layout());

        let mut diagnostics = Vec::new();

        for KeymapBlock {
            context,
            use_key_equivalents,
            bindings,
        } in self.0.iter()
        {
            let key_equivalents = if *use_key_equivalents == Some(true) {
                key_equivalents.as_ref()
            } else {
                None
            };

            let context_predicate: Option<Rc<KeyBindingContextPredicate>> =
                if let Some(context) = context.as_ref() {
                    match KeyBindingContextPredicate::parse(context) {
                        Ok(context_predicate) => Some(context_predicate.into()),
                        Err(err) => {
                            diagnostics.push(SettingsDiagnostic {
                                range: context.range(),
                                message: err.to_string(),
                            });
                            continue;
                        }
                    }
                } else {
                    None
                };

            let bindings = bindings
                .into_iter()
                .map(|(keystrokes, action_value)| {
                    Self::build_key_binding(
                        context.as_ref().map(|context| context.get_ref()),
                        keystrokes,
                        action_value,
                        context_predicate.clone(),
                        key_equivalents,
                        cx,
                    )
                })
                .filter_map(|result| result.map_err(|err| diagnostics.push(err)).ok())
                .collect::<Vec<_>>();

            cx.bind_keys(bindings);
        }

        if let Some(message) = settings_path.diagnostics_to_string(10, &content, diagnostics) {
            Err(anyhow!(message))
        } else {
            Ok(())
        }
    }

    fn build_key_binding(
        context: Option<&String>,
        keystrokes: &Spanned<String>,
        action_value: &Spanned<KeymapAction>,
        context_predicate: Option<Rc<KeyBindingContextPredicate>>,
        key_equivalents: Option<&HashMap<char, char>>,
        cx: &mut AppContext,
    ) -> std::result::Result<KeyBinding, SettingsDiagnostic> {
        let range = action_value.range();
        let action_value = &action_value.get_ref().0;

        let make_error_prefix =
            || format!("Invalid binding for \"{keystrokes}\" in context \"{context:?}\"");

        let make_unexpected_action_json_error = {
            |range: &Range<usize>| SettingsDiagnostic {
                range: range.clone(),
                message: format!(
                    "{}: Expected action to be a string  or a two-element array of [string, value]",
                    make_error_prefix()
                ),
            }
        };

        let action = match action_value {
            Value::Array(items) => {
                if items.len() != 2 {
                    return Err(make_unexpected_action_json_error(&range));
                }
                let name = &items[0];
                let data = &items[1];
                let serde_json::Value::String(name) = name else {
                    return Err(make_unexpected_action_json_error(&range));
                };
                match cx.build_action(&name, Some(data.clone())) {
                    Ok(action) => Ok(action),
                    Err(err) => Err(SettingsDiagnostic {
                        range: range.clone(),
                        message: format!("{}: {}", make_error_prefix(), err),
                    }),
                }
            }
            Value::String(name) => match cx.build_action(&name, None) {
                Ok(action) => Ok(action),
                Err(err) => Err(SettingsDiagnostic {
                    range: range.clone(),
                    message: format!("{}: {}", make_error_prefix(), err),
                }),
            },
            Value::Null => Ok(no_action()),
            _ => Err(make_unexpected_action_json_error(&range)),
        }?;

        KeyBinding::load(
            &keystrokes,
            action,
            context_predicate.clone(),
            key_equivalents,
        )
        .map_err(|err| SettingsDiagnostic {
            range: range.clone(),
            message: format!("{}: {}", make_error_prefix(), err),
        })
    }

    pub fn generate_json_schema(
        action_names: &[SharedString],
        deprecations: &[(SharedString, SharedString)],
    ) -> serde_json::Value {
        let mut root_schema = SchemaSettings::draft07()
            .with(|settings| settings.option_add_null_type = false)
            .into_generator()
            .into_root_schema_for::<KeymapFile>();

        let mut alternatives = vec![
            Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                enum_values: Some(
                    action_names
                        .iter()
                        .map(|name| Value::String(name.to_string()))
                        .collect(),
                ),
                ..Default::default()
            }),
            Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Array))),
                ..Default::default()
            }),
            Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Null))),
                ..Default::default()
            }),
        ];
        for (old, new) in deprecations {
            alternatives.push(Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                const_value: Some(Value::String(old.to_string())),
                extensions: Map::from_iter([(
                    // deprecationMessage is not part of the JSON Schema spec,
                    // but json-language-server recognizes it.
                    "deprecationMessage".to_owned(),
                    format!("Deprecated, use {new}").into(),
                )]),
                ..Default::default()
            }));
        }
        let action_schema = Schema::Object(SchemaObject {
            subschemas: Some(Box::new(SubschemaValidation {
                one_of: Some(alternatives),
                ..Default::default()
            })),
            ..Default::default()
        });

        root_schema
            .definitions
            .insert("KeymapAction".to_owned(), action_schema);

        serde_json::to_value(root_schema).unwrap()
    }

    pub fn blocks(&self) -> &[KeymapBlock] {
        &self.0
    }
}

fn no_action() -> Box<dyn gpui::Action> {
    gpui::NoAction.boxed_clone()
}

#[cfg(test)]
mod tests {
    use crate::{KeymapFile, SettingsPathRef};

    #[test]
    fn can_deserialize_keymap_with_trailing_comma() {
        let json = indoc::indoc! {"[
              // Standard macOS bindings
              {
                \"bindings\": {
                  \"up\": \"menu::SelectPrev\",
                },
              },
            ]
                  "

        };
        let path = Path::new("/keymap.json");
        KeymapFile::parse_user(path, json).unwrap();
    }
}
