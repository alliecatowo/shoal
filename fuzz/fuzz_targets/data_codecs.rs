#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_value::Value;

const FUZZ_INPUT_CAP: usize = 64 * 1024;
const CODEC_OUTPUT_CAP: usize = 16 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > FUZZ_INPUT_CAP {
        return;
    }
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let literal = serde_json::to_string(source).unwrap();
    let mut evaluator = shoal_eval::Evaluator::new(std::env::temp_dir());

    for namespace in ["json", "yaml", "toml", "csv"] {
        let program =
            format!("let parsed = {namespace}.parse({literal})\n{namespace}.stringify(parsed)");
        if let Ok(program) = shoal_syntax::parse(&program)
            && let Ok(Value::Str(output)) = evaluator.eval_program(&program)
        {
            assert!(output.len() <= CODEC_OUTPUT_CAP);
        }
        assert_eq!(
            evaluator
                .eval_program(&shoal_syntax::parse("40 + 2").unwrap())
                .unwrap(),
            Value::Int(42)
        );
    }
});
