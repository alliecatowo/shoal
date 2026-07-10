pub fn color_for_value(v: &shoal_value::Value) -> &'static str {
    match v {
        shoal_value::Value::Int(_) => "\x1b[36m",
        _ => "",
    }
}
