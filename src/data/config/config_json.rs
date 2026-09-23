//! Reading and writing a dot-notation path inside a config JSON document.
//!
//! `awman config get|set` addresses fields by dotted name — `work_items.dir`,
//! `dynamicWorkflows.guidance.2` — and these four navigate, create and remove
//! the corresponding JSON. Layer 0 (WI 0114 F-51): they know nothing about
//! which fields exist or what their values mean, only how a path maps onto a
//! `serde_json::Value`.

/// Look up a JSON field value, supporting dot-notation (e.g. "work_items.dir").
pub fn config_field_value(json: &serde_json::Value, field: &str) -> Option<String> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = json;
    for part in &parts {
        // A numeric segment indexes into an array (e.g. the
        // dynamicWorkflows.guidance.<n> entries); everything else is an
        // object key.
        current = match current {
            serde_json::Value::Array(arr) => arr.get(part.parse::<usize>().ok()?)?,
            _ => current.get(*part)?,
        };
    }
    Some(match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => return None,
        // Arrays of strings display comma-separated — the same shape the
        // user types when setting a list field, so edits round-trip.
        serde_json::Value::Array(arr) if arr.iter().all(|x| x.is_string()) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    })
}

/// Write a coerced value into the config JSON: `Null` removes the field
/// (used for `agentsToModels` entry deletion), anything else is set.
pub fn apply_config_field(json: &mut serde_json::Value, field: &str, value: serde_json::Value) {
    if value.is_null() {
        remove_config_field(json, field);
    } else {
        set_config_field(json, field, value);
    }
}

/// Remove a JSON field, supporting dot-notation for nested objects and a
/// trailing numeric segment that indexes into an array-of-strings field
/// (e.g. `dynamicWorkflows.guidance.1`). Removing an array element via
/// `Vec::remove` compacts the remaining elements, so subsequent entries are
/// automatically re-indexed (WI-0099). Missing intermediate objects make this
/// a no-op.
pub fn remove_config_field(json: &mut serde_json::Value, field: &str) {
    let parts: Vec<&str> = field.split('.').collect();
    let last = *parts.last().expect("split never yields an empty vec");
    // Trailing numeric segment: remove the element from the parent array.
    if let Ok(index) = last.parse::<usize>() {
        if parts.len() >= 2 {
            let mut current = json;
            for part in &parts[..parts.len() - 1] {
                match current.get_mut(*part) {
                    Some(v) => current = v,
                    None => return,
                }
            }
            if let serde_json::Value::Array(arr) = current {
                if index < arr.len() {
                    arr.remove(index);
                }
            }
            return;
        }
    }
    let mut current = json;
    for part in &parts[..parts.len() - 1] {
        match current.get_mut(*part) {
            Some(v) => current = v,
            None => return,
        }
    }
    if let serde_json::Value::Object(obj) = current {
        obj.remove(last);
    }
}

/// Set a JSON field, supporting dot-notation for nested objects.
/// E.g. "work_items.dir" sets `json["work_items"]["dir"]`.
pub fn set_config_field(json: &mut serde_json::Value, field: &str, value: serde_json::Value) {
    let parts: Vec<&str> = field.split('.').collect();
    // Trailing numeric segment: set (or append) an element in the parent
    // array-of-strings field (e.g. `dynamicWorkflows.guidance.<n>`). An index
    // at or past the current length appends, which is how the TUI Ctrl+N flow
    // adds a new entry (WI-0099). Intermediate objects and the array itself
    // are created on demand so guidance can be added to a config that has no
    // `dynamicWorkflows` block yet.
    if let Some(index) = parts.last().and_then(|p| p.parse::<usize>().ok()) {
        if parts.len() >= 2 {
            set_array_element(json, &parts[..parts.len() - 1], index, value);
            return;
        }
    }
    if parts.len() == 1 {
        // Top-level field
        if let serde_json::Value::Object(obj) = json {
            obj.insert(field.to_string(), value);
        }
    } else {
        // Navigate into nested objects, creating intermediate objects as needed.
        let mut current = json;
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                // Last segment: insert the value.
                if let serde_json::Value::Object(obj) = current {
                    obj.insert(part.to_string(), value);
                }
                return;
            }
            // Intermediate segment: ensure a nested object exists.
            if !current.get(*part).map(|v| v.is_object()).unwrap_or(false) {
                if let serde_json::Value::Object(obj) = current {
                    obj.insert(
                        part.to_string(),
                        serde_json::Value::Object(serde_json::Map::new()),
                    );
                }
            }
            current = current.get_mut(*part).expect("just inserted nested object");
        }
    }
}

/// Set or append an element in an array-of-strings field addressed by
/// `array_path` (the dot-path segments up to and including the array field,
/// e.g. `["dynamicWorkflows", "guidance"]`). Intermediate objects and the
/// array are created on demand. `index >= len` appends; `index < len`
/// overwrites in place. Used for the `dynamicWorkflows.guidance` array
/// (WI-0099).
pub fn set_array_element(
    json: &mut serde_json::Value,
    array_path: &[&str],
    index: usize,
    value: serde_json::Value,
) {
    let Some((array_field, obj_path)) = array_path.split_last() else {
        return;
    };
    // Navigate (creating as needed) the object path that holds the array.
    let mut current = json;
    for part in obj_path {
        if !current.get(*part).map(|v| v.is_object()).unwrap_or(false) {
            if let serde_json::Value::Object(obj) = current {
                obj.insert(
                    part.to_string(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            } else {
                return;
            }
        }
        current = current.get_mut(*part).expect("just inserted nested object");
    }
    // Ensure the array field exists and is an array.
    if !current
        .get(*array_field)
        .map(|v| v.is_array())
        .unwrap_or(false)
    {
        if let serde_json::Value::Object(obj) = current {
            obj.insert(
                array_field.to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        } else {
            return;
        }
    }
    if let Some(serde_json::Value::Array(arr)) = current.get_mut(*array_field) {
        if index < arr.len() {
            arr[index] = value;
        } else {
            arr.push(value);
        }
    }
}
