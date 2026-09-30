//! Parsing of `com.canonical.dbusmenu` layouts and generic zvariant helpers.

use zbus::zvariant::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Toggle {
    None,
    Checkmark(bool),
    Radio(bool),
}

#[derive(Clone, Debug)]
pub struct MenuNode {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub separator: bool,
    pub toggle: Toggle,
    pub children: Vec<MenuNode>,
}

/// Strip nested variants (`v` inside `v`).
pub fn unwrap<'a>(mut v: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = v {
        v = inner;
    }
    v
}

pub fn as_str(v: &Value) -> Option<String> {
    match unwrap(v) {
        Value::Str(s) => Some(s.as_str().to_owned()),
        Value::ObjectPath(p) => Some(p.as_str().to_owned()),
        _ => None,
    }
}

pub fn as_bool(v: &Value) -> Option<bool> {
    match unwrap(v) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

pub fn as_i32(v: &Value) -> Option<i32> {
    match unwrap(v) {
        Value::I32(i) => Some(*i),
        Value::U32(u) => i32::try_from(*u).ok(),
        _ => None,
    }
}

pub fn as_bytes(v: &Value) -> Option<Vec<u8>> {
    match unwrap(v) {
        Value::Array(a) => Some(
            a.inner()
                .iter()
                .filter_map(|b| match b {
                    Value::U8(b) => Some(*b),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Parse one `(ia{sv}av)` layout node.
pub fn parse_node(v: &Value) -> Option<MenuNode> {
    let Value::Structure(s) = unwrap(v) else {
        return None;
    };
    let [id, props, children] = s.fields() else {
        return None;
    };
    let id = as_i32(id)?;

    let mut node = MenuNode {
        id,
        label: String::new(),
        enabled: true,
        visible: true,
        separator: false,
        toggle: Toggle::None,
        children: Vec::new(),
    };

    let mut toggle_type = String::new();
    let mut toggle_state = 0;
    if let Value::Dict(d) = unwrap(props) {
        for (k, val) in d.iter() {
            let Some(key) = as_str(k) else { continue };
            match key.as_str() {
                "label" => node.label = as_str(val).map(|l| strip_mnemonic(&l)).unwrap_or_default(),
                "enabled" => node.enabled = as_bool(val).unwrap_or(true),
                "visible" => node.visible = as_bool(val).unwrap_or(true),
                "type" => node.separator = as_str(val).as_deref() == Some("separator"),
                "toggle-type" => toggle_type = as_str(val).unwrap_or_default(),
                "toggle-state" => toggle_state = as_i32(val).unwrap_or(0),
                _ => {}
            }
        }
    }
    node.toggle = match toggle_type.as_str() {
        "checkmark" => Toggle::Checkmark(toggle_state == 1),
        "radio" => Toggle::Radio(toggle_state == 1),
        _ => Toggle::None,
    };

    if let Value::Array(a) = unwrap(children) {
        node.children = a.inner().iter().filter_map(parse_node).collect();
    }
    Some(node)
}

/// dbusmenu labels mark access keys with `_`; a literal underscore is `__`.
fn strip_mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mnemonic() {
        assert_eq!(strip_mnemonic("_Quit"), "Quit");
        assert_eq!(strip_mnemonic("snake__case"), "snake_case");
    }

    #[test]
    fn argb_conversion() {
        let icon = crate::icon::from_argb32(1, 1, &[0x80, 0x10, 0x20, 0x30]).unwrap();
        assert_eq!(icon.rgba, vec![0x10, 0x20, 0x30, 0x80]);
    }
}
