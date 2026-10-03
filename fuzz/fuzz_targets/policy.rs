#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    const SEPARATOR: &[u8] = b"\n---PLAN---\n";
    let Some(split) = data
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)
    else {
        return;
    };
    let Ok(policy_source) = std::str::from_utf8(&data[..split]) else {
        return;
    };
    let Ok(policy) = shoal_leash::Policy::from_toml(policy_source) else {
        return;
    };
    let Ok(plan) = serde_json::from_slice::<shoal_leash::Plan>(&data[split + SEPARATOR.len()..])
    else {
        return;
    };
    let _ = policy.evaluate_plan("agent", &plan);
    for effect in &plan.effects {
        let _ = policy.evaluate_effect("agent", effect);
    }
});
