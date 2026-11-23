use super::ProjectConfig;
use crate::mail::request::{GetAttachment, ListMail, MailRef, SearchMail, SendMail, SetRead};
use schemars::{
    Schema, SchemaGenerator, generate::SchemaSettings, transform::transform_subschemas,
};
use serde_json::{Value, json};

pub fn document() -> Schema {
    let mut generator = SchemaGenerator::default();
    generator.subschema_for::<ProjectConfig>();
    let mut definitions = generator.take_definitions(true);
    for (name, schema) in &mut definitions {
        if !matches!(
            name.as_str(),
            "EnvReference" | "FileReference" | "ValueSource" | "EnvName"
        ) {
            allow_value_references(schema);
        }
    }
    generator.subschema_for::<SendMail>();
    generator.subschema_for::<ListMail>();
    generator.subschema_for::<SearchMail>();
    generator.subschema_for::<MailRef>();
    generator.subschema_for::<GetAttachment>();
    generator.subschema_for::<SetRead>();

    definitions.extend(generator.take_definitions(true));
    let mut fragment: Value = fragment().into();
    allow_value_references(&mut fragment);
    for section in ["agents", "mcp", "harnesses"] {
        if let Some(project) = definitions.get_mut("ProjectConfig") {
            let registry = project["properties"][section]
                .as_object_mut()
                .expect("registry schema");
            // Keep the ID patterns and additional-properties rule together:
            // additionalProperties only sees patterns in its own subschema.
            let mut strict = serde_json::Map::new();
            for key in ["additionalProperties", "patternProperties"] {
                if let Some(rule) = registry.remove(key) {
                    strict.insert(key.into(), rule);
                }
            }
            registry.insert("if".into(), json!({"required": ["default"]}));
            registry.insert(
                "then".into(),
                json!({"$ref": format!("#/$defs/ProjectFragment/properties/{section}")}),
            );
            registry.insert("else".into(), Value::Object(strict));
        }
    }
    definitions.insert("ProjectFragment".into(), fragment);

    // Keep the fragment behind allOf so VS Code's JSON language service can
    // resolve its references against the root's $defs.
    let mut document = schemars::json_schema!({
        "allOf": [{"$ref": "#/$defs/ProjectFragment"}]
    });
    document.insert(
        "$schema".into(),
        "https://json-schema.org/draft/2020-12/schema".into(),
    );
    document.insert("title".into(), "agents configuration".into());
    document.insert("$defs".into(), definitions.into());
    document
}

fn allow_value_references(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for (name, child) in properties {
            if !matches!(name.as_str(), "$schema" | "password") {
                allow_value_references(child);
            }
        }
    }
    for key in ["additionalProperties", "items", "not", "if", "then", "else"] {
        if let Some(child) = object.get_mut(key) {
            allow_value_references(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                allow_value_references(child);
            }
        }
    }
    if object.get("type").is_some_and(|kind| {
        kind == "string"
            || kind
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind == "string"))
    }) || object.get("const").is_some_and(Value::is_string)
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.iter().any(Value::is_string))
    {
        *schema = json!({"anyOf": [schema.take(), {"$ref":"#/$defs/EnvReference"}, {"$ref":"#/$defs/FileReference"}]});
    }
}

fn fragment() -> Schema {
    fn optional_properties(schema: &mut Schema) {
        if let Some(object) = schema.as_object_mut() {
            object.remove("required");
            if let Some(branches) = object.remove("oneOf") {
                object.insert("anyOf".into(), branches);
            }
        }
        transform_subschemas(&mut optional_properties, schema);
    }
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    settings.meta_schema = None;
    let mut schema = settings
        .into_generator()
        .into_root_schema_for::<ProjectConfig>();
    optional_properties(&mut schema);
    schema.insert("title".into(), "ProjectConfigFragment".into());
    schema
}
