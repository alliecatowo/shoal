use super::*;
use crate::abi::shoal::plugin::types::{GuestValue, ValueKind};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};

fn fixture_bytes(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let component = temp.path().join("p.wasm");
    fs::write(&component, bytes).unwrap();
    let manifest = temp.path().join("p.toml");
    fs::write(
        &manifest,
        "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n",
    )
    .unwrap();
    (temp, manifest)
}

fn fixture(wat: &str) -> (tempfile::TempDir, PathBuf) {
    fixture_bytes(wat.as_bytes())
}

fn valid_component(edit_core_wat: impl FnOnce(&mut String)) -> Vec<u8> {
    let mut resolve = Resolve::new();
    let package = resolve
        .push_str(
            "shoal-plugin.wit",
            include_str!("../../wit/shoal-plugin.wit"),
        )
        .unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let core = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    let mut core_wat = wasmprinter::print_bytes(core).unwrap();
    core_wat = core_wat
        .replace("(memory (;0;) 0)", "(memory (;0;) 1)")
        .replace("unreachable", "i32.const 0");
    edit_core_wat(&mut core_wat);
    let mut core = wat::parse_str(core_wat).unwrap();
    embed_component_metadata(&mut core, &resolve, world, StringEncoding::UTF8).unwrap();
    ComponentEncoder::default()
        .module(&core)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn valid_fixture() -> (tempfile::TempDir, PathBuf) {
    fixture_bytes(&valid_component(|_| {}))
}

fn set_func_i32_result(wat: &mut String, index: usize, result: u32) {
    let function = wat.find(&format!("(func (;{index};)")).unwrap();
    let body = wat[function..].find("i32.const 0").unwrap() + function;
    wat.replace_range(
        body..body + "i32.const 0".len(),
        &format!("i32.const {result}"),
    );
}

fn wat_data(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn invokable_component(call_time: bool) -> Vec<u8> {
    invokable_component_edited(call_time, |_| {})
}

fn invokable_component_edited(call_time: bool, edit_core_wat: impl FnOnce(&mut String)) -> Vec<u8> {
    valid_component(|wat| {
        set_func_i32_result(wat, 4, 16);
        set_func_i32_result(wat, 6, 128);
        if call_time {
            let function = wat.find("(func (;6;)").unwrap();
            let result = wat[function..].find("i32.const 128").unwrap() + function;
            wat.insert_str(result, "i32.const 200\ncall 0\n");
        }
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
                end,
                concat!(
                    "(data (i32.const 0) \"\\20\\00\\00\\00\\01\\00\\00\\00\")\n",
                    "(data (i32.const 32) \"\\40\\00\\00\\00\\04\\00\\00\\00\\44\\00\\00\\00\\02\\00\\00\\00\")\n",
                    "(data (i32.const 64) \"test{}\")\n",
                    "(data (i32.const 128) \"\\00\\00\\00\\00\\01\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\")\n",
                ),
            );
        edit_core_wat(wat);
    })
}

fn set_invocation_result_pointer(wat: &mut String, result: u32) {
    let function = wat.find("(func (;6;)").unwrap();
    let body = wat[function..].find("i32.const 128").unwrap() + function;
    wat.replace_range(
        body..body + "i32.const 128".len(),
        &format!("i32.const {result}"),
    );
}

fn invocation_fixture(component: &[u8], limits: Limits) -> (tempfile::TempDir, Registry) {
    let (temp, path) = fixture_bytes(component);
    fs::write(
        &path,
        "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
    )
    .unwrap();
    let mut registry = Registry::new(limits).unwrap();
    registry.load_manifest(&path).unwrap();
    (temp, registry)
}

struct RecordingCapabilities {
    allow: bool,
    effects: std::sync::Mutex<Vec<Effect>>,
    now_calls: AtomicUsize,
    read_calls: AtomicUsize,
    read_result: Vec<u8>,
}

struct CancelledCapabilities;

impl CapabilityProvider for CancelledCapabilities {
    fn authorize(&self, _effect: &Effect) -> Result<(), CapabilityError> {
        Ok(())
    }

    fn now_ns(&self) -> Result<u64, CapabilityError> {
        Ok(0)
    }

    fn read_file(&self, _path: &Path, _max_bytes: usize) -> Result<Vec<u8>, CapabilityError> {
        Ok(Vec::new())
    }

    fn cancelled(&self) -> bool {
        true
    }
}

impl RecordingCapabilities {
    fn new(allow: bool) -> Self {
        Self {
            allow,
            effects: std::sync::Mutex::new(Vec::new()),
            now_calls: AtomicUsize::new(0),
            read_calls: AtomicUsize::new(0),
            read_result: b"content".to_vec(),
        }
    }
}

impl CapabilityProvider for RecordingCapabilities {
    fn authorize(&self, effect: &Effect) -> Result<(), CapabilityError> {
        self.effects.lock().unwrap().push(effect.clone());
        if self.allow {
            Ok(())
        } else {
            Err(CapabilityError {
                message: "policy denied".into(),
            })
        }
    }

    fn now_ns(&self) -> Result<u64, CapabilityError> {
        self.now_calls.fetch_add(1, Ordering::Relaxed);
        Ok(42)
    }

    fn read_file(&self, _path: &Path, _max_bytes: usize) -> Result<Vec<u8>, CapabilityError> {
        self.read_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.read_result.clone())
    }
}

fn state(
    declared_effects: Vec<Effect>,
    capabilities: Arc<dyn CapabilityProvider>,
    hostcall_bytes: usize,
) -> State {
    State {
        limits: StoreLimitsBuilder::new().build(),
        capabilities,
        hostcall_bytes,
        hostcall_remaining_bytes: hostcall_bytes,
        hostcall_remaining_calls: 1,
        declared_effects,
    }
}

#[test]
fn accepts_component_implementing_the_versioned_world() {
    let (_temp, path) = valid_fixture();
    let mut registry = Registry::new(Limits::default()).unwrap();
    registry.load_manifest(&path).unwrap();
    assert_eq!(registry.len(), 1);
    assert!(!registry.get("test").unwrap().bytes().is_empty());
}

#[test]
fn compilation_admission_wait_is_bounded_and_slots_are_reclaimed() {
    let admission = CompilationAdmission::default();
    let first = acquire_compilation(&admission, 1, Duration::from_millis(1), "first").unwrap();
    let error = acquire_compilation(&admission, 1, Duration::from_millis(1), "second")
        .err()
        .expect("a full compiler budget must time out");
    assert!(error.to_string().contains("admission limit (1)"));
    drop(first);
    let replacement =
        acquire_compilation(&admission, 1, Duration::from_millis(1), "replacement").unwrap();
    assert_eq!(*admission.active.lock().unwrap(), 1);
    drop(replacement);
    assert_eq!(*admission.active.lock().unwrap(), 0);
}

#[test]
fn compilation_configuration_cannot_raise_process_wide_ceilings() {
    let too_many = Limits {
        compilation_jobs: MAX_COMPILATION_JOBS + 1,
        ..Limits::default()
    };
    assert!(
        Host::new(too_many)
            .err()
            .expect("an excessive compilation limit must fail")
            .to_string()
            .contains("ceiling")
    );

    let too_long = Limits {
        compilation_wait: MAX_COMPILATION_WAIT + Duration::from_nanos(1),
        ..Limits::default()
    };
    assert!(
        Host::new(too_long)
            .err()
            .expect("an excessive compilation wait must fail")
            .to_string()
            .contains("ceiling")
    );
}

#[test]
fn wasmtime_parallel_compilation_feature_stays_disabled() {
    let manifest = include_str!("../../Cargo.toml");
    assert!(manifest.contains("default-features=false"));
    assert!(!manifest.contains("parallel-compilation"));
}

#[test]
fn rejects_ambient_import() {
    let (_temp, path) = fixture("(component (import \"wasi:filesystem/types@0.2.0\" (instance)))");
    let mut registry = Registry::new(Limits::default()).unwrap();
    let error = registry.load_manifest(&path).unwrap_err().to_string();
    assert!(error.contains("Shoal ABI v1"), "{error}");
}

#[test]
fn rejects_core_module_bad_manifest_and_empty_file() {
    let (_temp, path) = fixture("(module)");
    let mut registry = Registry::new(Limits::default()).unwrap();
    assert!(registry.load_manifest(&path).is_err());

    let temp = tempfile::tempdir().unwrap();
    let bad_manifest = temp.path().join("x.toml");
    fs::write(&bad_manifest, "name='x'\nunknown=1").unwrap();
    assert!(Manifest::load(&bad_manifest).is_err());

    let (_temp, path) = fixture("");
    assert!(registry.load_manifest(&path).is_err());

    let (_temp, path) = fixture("(component)");
    let error = registry.load_manifest(&path).unwrap_err().to_string();
    assert!(error.contains("Shoal ABI v1"), "{error}");
}

#[test]
fn manifest_metadata_must_match_guest_exports() {
    let (temp, path) = valid_fixture();
    fs::write(
        &path,
        "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='claimed'\nsignature='{}'\n",
    )
    .unwrap();
    let mut registry = Registry::new(Limits::default()).unwrap();
    let error = registry.load_manifest(&path).unwrap_err().to_string();
    assert!(error.contains("does not match its manifest"), "{error}");
    drop(temp);
}

#[test]
fn command_invocation_uses_the_retained_component_and_typed_value_abi() {
    let (temp, path) = fixture_bytes(&invokable_component(false));
    fs::write(
        &path,
        "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
    )
    .unwrap();
    let mut registry = Registry::new(Limits::default()).unwrap();
    registry.load_manifest(&path).unwrap();
    let metadata = registry.command("test").unwrap();
    assert_eq!(metadata.plugin, "test");
    assert_eq!(metadata.declaration.name, "test");
    assert_eq!(registry.command_names().collect::<Vec<_>>(), vec!["test"]);
    fs::write(temp.path().join("p.wasm"), b"replaced after validation").unwrap();
    let value = registry
        .invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities))
        .unwrap();
    assert_eq!(value, PluginValue::Null);
}

#[cfg(target_has_atomic = "64")]
#[test]
fn session_cancellation_interrupts_compute_only_guest_code() {
    let component = valid_component(|wat| {
        set_func_i32_result(wat, 4, 16);
        set_func_i32_result(wat, 6, 128);
        let function = wat.find("(func (;6;)").unwrap();
        let result = wat[function..].find("i32.const 128").unwrap() + function;
        wat.insert_str(result, "(loop $forever (br $forever))\n");
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
                end,
                concat!(
                    "(data (i32.const 0) \"\\20\\00\\00\\00\\01\\00\\00\\00\")\n",
                    "(data (i32.const 32) \"\\40\\00\\00\\00\\04\\00\\00\\00\\44\\00\\00\\00\\02\\00\\00\\00\")\n",
                    "(data (i32.const 64) \"test{}\")\n",
                ),
            );
    });
    let (_temp, path) = fixture_bytes(&component);
    fs::write(
        &path,
        "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
    )
    .unwrap();
    let mut registry = Registry::new(Limits {
        fuel: u64::MAX,
        wall_time: Duration::from_secs(5),
        ..Limits::default()
    })
    .unwrap();
    registry.load_manifest(&path).unwrap();
    let started = Instant::now();
    let error = registry
        .invoke_declared_command("test", Vec::new(), Arc::new(CancelledCapabilities))
        .unwrap_err();
    assert!(matches!(error, PluginError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn guest_hostcall_cannot_exceed_declared_and_authorized_effects() {
    let (_temp, path) = fixture_bytes(&invokable_component(true));
    fs::write(
        &path,
        "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\n\
             effects=[{kind='time'}]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
    )
    .unwrap();
    let mut registry = Registry::new(Limits::default()).unwrap();
    registry.load_manifest(&path).unwrap();

    let denied = Arc::new(RecordingCapabilities::new(false));
    assert_eq!(
        registry
            .invoke_command("test", "test", Vec::new(), denied.clone())
            .unwrap(),
        PluginValue::Null
    );
    assert_eq!(denied.effects.lock().unwrap().as_slice(), &[Effect::Time]);
    assert_eq!(denied.now_calls.load(Ordering::Relaxed), 0);

    let allowed = Arc::new(RecordingCapabilities::new(true));
    assert_eq!(
        registry
            .invoke_command("test", "test", Vec::new(), allowed.clone())
            .unwrap(),
        PluginValue::Null
    );
    assert_eq!(allowed.now_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn typed_effects_and_abi_version_are_strict_manifest_data() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("plugin.toml");
    fs::write(
        &path,
        "name='p'\nversion='1'\nabi_version=1\ncomponent='p.wasm'\neffects=[{kind='time'}]\n",
    )
    .unwrap();
    let manifest = Manifest::load(&path).unwrap();
    assert_eq!(manifest.effects, vec![Effect::Time]);

    fs::write(
        &path,
        "name='p'\nversion='1'\nabi_version=9\ncomponent='p.wasm'\neffects=[]\n",
    )
    .unwrap();
    assert!(Manifest::load(&path).is_err());
}

#[test]
fn value_envelope_is_versioned_typed_and_bounded() {
    let values = [
        PluginValue::Null,
        PluginValue::Bool(true),
        PluginValue::Signed(-7),
        PluginValue::Unsigned(9),
        PluginValue::Float(1.25),
        PluginValue::Text("hello".into()),
        PluginValue::Bytes(vec![0, 255]),
        PluginValue::Json(serde_json::json!({"a": [1, true]})),
    ];
    for value in values {
        let guest = value.clone().into_guest(1024, 16, 128).unwrap();
        assert_eq!(
            PluginValue::from_guest(guest, 1024, 16, 128).unwrap(),
            value
        );
    }
    assert!(
        PluginValue::Bytes(vec![0; 5])
            .into_guest(4, 16, 128)
            .is_err()
    );
    assert!(
        PluginValue::from_guest(
            GuestValue {
                abi_version: 99,
                kind: ValueKind::Null,
                payload: Vec::new(),
            },
            4,
            16,
            128,
        )
        .is_err()
    );
}

#[test]
fn hostcalls_require_both_declaration_and_runtime_authorization() {
    let allowed = Arc::new(RecordingCapabilities::new(true));
    let mut undeclared = state(Vec::new(), allowed.clone(), 1024);
    let error = <State as abi::shoal::plugin::host::Host>::now_ns(&mut undeclared).unwrap_err();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(allowed.effects.lock().unwrap().is_empty());
    assert_eq!(allowed.now_calls.load(Ordering::Relaxed), 0);

    let denied = Arc::new(RecordingCapabilities::new(false));
    let mut declared = state(vec![Effect::Time], denied.clone(), 1024);
    let error = <State as abi::shoal::plugin::host::Host>::now_ns(&mut declared).unwrap_err();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert_eq!(denied.effects.lock().unwrap().as_slice(), &[Effect::Time]);
    assert_eq!(denied.now_calls.load(Ordering::Relaxed), 0);

    let allowed = Arc::new(RecordingCapabilities::new(true));
    let mut declared = state(vec![Effect::Time], allowed.clone(), 1024);
    assert_eq!(
        <State as abi::shoal::plugin::host::Host>::now_ns(&mut declared).unwrap(),
        42
    );
    assert_eq!(allowed.now_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn filesystem_hostcall_authorizes_exact_path_and_bounds_output() {
    let capabilities = Arc::new(RecordingCapabilities {
        allow: true,
        effects: std::sync::Mutex::new(Vec::new()),
        now_calls: AtomicUsize::new(0),
        read_calls: AtomicUsize::new(0),
        read_result: vec![0; 33],
    });
    let effect = Effect::FsRead {
        paths: vec![PathBuf::from("/work/input")],
    };
    let mut state = state(vec![effect.clone()], capabilities.clone(), 32);
    let error =
        <State as abi::shoal::plugin::host::Host>::read_file(&mut state, "/work/input".into())
            .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ResourceLimit);
    assert_eq!(capabilities.effects.lock().unwrap().as_slice(), &[effect]);
    assert_eq!(capabilities.read_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn duplicate_is_deterministic() {
    let (_temp, path) = valid_fixture();
    let mut registry = Registry::new(Limits::default()).unwrap();
    registry.load_manifest(&path).unwrap();
    assert!(matches!(
        registry.load_manifest(&path),
        Err(PluginError::Duplicate(_))
    ));
}

#[test]
fn artifact_is_bound_to_the_validated_bytes_and_digest() {
    let (temp, path) = valid_fixture();
    let original = fs::read(temp.path().join("p.wasm")).unwrap();
    let mut registry = Registry::new(Limits::default()).unwrap();
    registry.load_manifest(&path).unwrap();
    fs::write(temp.path().join("p.wasm"), b"not wasm anymore").unwrap();

    let plugin = registry.get("test").unwrap();
    assert_eq!(plugin.bytes(), original);
    assert_eq!(plugin.digest(), blake3::hash(&original));
    assert_eq!(
        plugin
            .component
            .component_type()
            .imports(plugin.component.engine())
            .count(),
        2
    );
}

#[test]
fn manifest_and_component_reads_are_bounded() {
    let (_temp, path) = valid_fixture();
    let mut registry = Registry::new(Limits {
        manifest_bytes: 8,
        ..Limits::default()
    })
    .unwrap();
    assert!(registry.load_manifest(&path).is_err());

    let (_temp, path) = valid_fixture();
    let mut registry = Registry::new(Limits {
        component_bytes: 4,
        ..Limits::default()
    })
    .unwrap();
    let error = registry.load_manifest(&path).unwrap_err().to_string();
    assert!(error.contains("4-byte validation limit"), "{error}");
}

#[test]
fn aggregate_memory_count_and_per_memory_size_are_enforced() {
    let component = valid_component(|wat| {
        *wat = wat.replace("(memory (;0;) 1)", "(memory (;0;) 2)");
    });
    let (_temp, path) = fixture_bytes(&component);
    let mut registry = Registry::new(Limits {
        memory_bytes: 64 * 1024,
        ..Limits::default()
    })
    .unwrap();
    assert!(registry.load_manifest(&path).is_err());

    let component = valid_component(|wat| {
        let end = wat.rfind(')').unwrap();
        wat.insert_str(end, "(memory 1)\n");
    });
    let (_temp, path) = fixture_bytes(&component);
    let mut registry = Registry::new(Limits {
        memories: 1,
        instances: 4,
        ..Limits::default()
    })
    .unwrap();
    assert!(registry.load_manifest(&path).is_err());
}

#[cfg(target_has_atomic = "64")]
#[test]
fn wall_deadline_interrupts_nonterminating_start_code() {
    let component = valid_component(|wat| {
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
            end,
            "(func $start (loop $forever (br $forever)))\n(start $start)\n",
        );
    });
    let (_temp, path) = fixture_bytes(&component);
    let mut registry = Registry::new(Limits {
        fuel: u64::MAX,
        wall_time: Duration::from_millis(20),
        ..Limits::default()
    })
    .unwrap();
    let start = Instant::now();
    assert!(registry.load_manifest(&path).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn invocation_fuel_memory_growth_and_stack_are_fail_closed_and_repeatable() {
    for component in [
        invokable_component_edited(false, |wat| {
            let function = wat.find("(func (;6;)").unwrap();
            let result = wat[function..].find("i32.const 128").unwrap() + function;
            wat.insert_str(result, "(loop $forever (br $forever))\n");
        }),
        invokable_component_edited(false, |wat| {
            let end = wat.rfind(')').unwrap();
            wat.insert_str(end, "(func $recurse (call $recurse))\n");
            let function = wat.find("(func (;6;)").unwrap();
            let result = wat[function..].find("i32.const 128").unwrap() + function;
            wat.insert_str(result, "call $recurse\n");
        }),
        invokable_component_edited(false, |wat| {
            let function = wat.find("(func (;6;)").unwrap();
            let result = wat[function..].find("i32.const 128").unwrap() + function;
            wat.insert_str(result, "i32.const 1\nmemory.grow\ndrop\n");
        }),
    ] {
        let (_temp, registry) = invocation_fixture(
            &component,
            Limits {
                fuel: 20_000,
                memory_bytes: 64 * 1024,
                wasm_stack_bytes: 64 * 1024,
                ..Limits::default()
            },
        );
        for _ in 0..2 {
            let error = registry
                .invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities))
                .unwrap_err();
            assert!(matches!(error, PluginError::Component { .. }));
        }
        assert!(registry.command("test").is_some());
    }
}

#[test]
fn malformed_and_oversized_guest_results_do_not_poison_later_calls() {
    let malformed_pointer = invokable_component_edited(false, |wat| {
        set_invocation_result_pointer(wat, 65_530);
    });
    let (_temp, malformed) = invocation_fixture(&malformed_pointer, Limits::default());
    for _ in 0..2 {
        assert!(matches!(
            malformed.invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities)),
            Err(PluginError::Component { .. })
        ));
    }

    let malformed_utf8 = invokable_component_edited(false, |wat| {
        let mut result = [0_u8; 20];
        result[4..8].copy_from_slice(&ABI_VERSION.to_le_bytes());
        result[8] = 5; // value-kind.text
        result[12..16].copy_from_slice(&256_u32.to_le_bytes());
        result[16..20].copy_from_slice(&2_u32.to_le_bytes());
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
            end,
            &format!(
                "(data (i32.const 128) \"{}\")\n(data (i32.const 256) \"\\ff\\ff\")\n",
                wat_data(&result)
            ),
        );
    });
    let (_temp, malformed_utf8) = invocation_fixture(&malformed_utf8, Limits::default());
    assert!(matches!(
        malformed_utf8.invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities)),
        Err(PluginError::Value(_))
    ));

    let oversized = invokable_component_edited(false, |wat| {
        let mut result = [0_u8; 20];
        result[4..8].copy_from_slice(&ABI_VERSION.to_le_bytes());
        result[8] = 6; // value-kind.bytes
        result[12..16].copy_from_slice(&256_u32.to_le_bytes());
        result[16..20].copy_from_slice(&4096_u32.to_le_bytes());
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
            end,
            &format!("(data (i32.const 128) \"{}\")\n", wat_data(&result)),
        );
    });
    let (_temp, oversized) = invocation_fixture(
        &oversized,
        Limits {
            value_bytes: 128,
            ..Limits::default()
        },
    );
    for _ in 0..2 {
        let error = oversized
            .invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities))
            .unwrap_err();
        assert!(matches!(error, PluginError::Value(_)));
    }
}

#[test]
fn deep_json_is_rejected_before_serialization_and_after_guest_lifting() {
    let mut json = serde_json::Value::Null;
    for _ in 0..9 {
        json = serde_json::Value::Array(vec![json]);
    }
    assert!(PluginValue::Json(json).into_guest(4096, 8, 128).is_err());

    let payload = format!("{}0{}", "[".repeat(9), "]".repeat(9));
    let deep_result = invokable_component_edited(false, |wat| {
        let mut result = [0_u8; 20];
        result[4..8].copy_from_slice(&ABI_VERSION.to_le_bytes());
        result[8] = 7; // value-kind.json
        result[12..16].copy_from_slice(&256_u32.to_le_bytes());
        result[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let end = wat.rfind(')').unwrap();
        wat.insert_str(
            end,
            &format!(
                "(data (i32.const 128) \"{}\")\n(data (i32.const 256) \"{}\")\n",
                wat_data(&result),
                wat_data(payload.as_bytes())
            ),
        );
    });
    let (_temp, registry) = invocation_fixture(
        &deep_result,
        Limits {
            value_depth: 8,
            ..Limits::default()
        },
    );
    assert!(matches!(
        registry.invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities)),
        Err(PluginError::Value(_))
    ));
}

#[test]
fn registry_hostcall_and_discovery_retention_are_bounded() {
    let capabilities = Arc::new(RecordingCapabilities::new(true));
    let mut state = state(vec![Effect::Time], capabilities, 64);
    state.hostcall_remaining_bytes = 16;
    assert!(<State as abi::shoal::plugin::host::Host>::now_ns(&mut state).is_ok());
    assert_eq!(
        <State as abi::shoal::plugin::host::Host>::now_ns(&mut state)
            .unwrap_err()
            .kind,
        ErrorKind::ResourceLimit
    );

    let component = valid_component(|_| {});
    let (temp, first) = fixture_bytes(&component);
    let second = temp.path().join("second.toml");
    fs::write(
        &second,
        "name='second'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n",
    )
    .unwrap();
    let mut registry = Registry::new(Limits {
        plugins: 1,
        discovery_entries: 1,
        ..Limits::default()
    })
    .unwrap();
    registry.load_manifest(&first).unwrap();
    assert!(registry.load_manifest(&second).is_err());

    fs::write(temp.path().join("third.toml"), "invalid=1").unwrap();
    let errors = registry.load_dir(temp.path());
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("discovery entry limit"))
    );

    let mut overflow_registry = Registry::new(Limits {
        discovery_entries: 1,
        ..Limits::default()
    })
    .unwrap();
    assert!(!overflow_registry.load_dir(temp.path()).is_empty());
    assert!(overflow_registry.is_empty());

    let retained_limit = component.len() + 1;
    let mut registry = Registry::new(Limits {
        component_bytes: component.len(),
        registry_component_bytes: retained_limit,
        plugins: 2,
        ..Limits::default()
    })
    .unwrap();
    registry.load_manifest(&first).unwrap();
    let error = registry.load_manifest(&second).unwrap_err().to_string();
    assert!(error.contains("retained limit"), "{error}");
}
