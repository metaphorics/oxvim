#![allow(missing_docs)]

use differential::{Embedded, OXVIM, binary};
use rmpv::Value;

#[test]
fn verification_three_embedded_scenario() {
    let mut editor = Embedded::spawn(&binary(OXVIM)).expect("spawn release oxvim --embed");

    let info = result(editor.request(1, "nvim_get_api_info", vec![]).expect("get api info").0);
    let fields = info.as_array().expect("api info result array");
    assert_eq!(fields.first().and_then(Value::as_i64), Some(1));
    assert_eq!(map_get(map_get(fields.get(1).expect("metadata"), "version"), "api_level").as_i64(), Some(15));

    assert_eq!(
        result(editor.request(2, "nvim_buf_set_lines", vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true), Value::Array(vec![Value::from("ox"), Value::from("vim")])]).expect("set lines").0),
        Value::Nil,
    );
    assert_eq!(
        result(editor.request(3, "nvim_buf_get_lines", vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)]).expect("get lines").0),
        Value::Array(vec![Value::from("ox"), Value::from("vim")]),
    );
    assert_eq!(
        result(editor.request(4, "nvim_exec_lua", vec![Value::from("return vim.fn.has('nvim-0.13')"), Value::Array(Vec::new())]).expect("exec lua").0),
        Value::from(1),
    );
    assert_eq!(result(editor.request(5, "nvim_command", vec![Value::from("normal! ggdd")]).expect("normal delete").0), Value::Nil);
    assert_eq!(
        result(editor.request(6, "nvim_buf_get_lines", vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)]).expect("get remaining lines").0),
        Value::Array(vec![Value::from("vim")]),
    );
}

fn result(response: Value) -> Value {
    let fields = response.as_array().expect("response array");
    assert_eq!(fields.first().and_then(Value::as_i64), Some(1));
    assert_eq!(fields.get(2), Some(&Value::Nil), "RPC error: {:?}", fields.get(2));
    fields.get(3).cloned().expect("response result")
}

fn map_get<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .as_map()
        .and_then(|entries| entries.iter().find_map(|(key, value)| (key.as_str() == Some(name)).then_some(value)))
        .unwrap_or_else(|| panic!("missing map key {name}"))
}
