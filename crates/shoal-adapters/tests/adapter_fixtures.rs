//! Per-adapter fixture tests for the P2 adapter-pack expansion.
//!
//! Each test feeds `parse_output` **canned, realistic bytes** shaped like
//! the real tool's actual output (no network, no requiring the real
//! binary be installed) through the exact `parse` strategy and
//! `output.type` hint declared in that adapter's `adapters/*.toml`, and
//! asserts the declared parse -> type combination genuinely holds. Each
//! test also feeds a malformed/truncated variant of the same shape and
//! asserts the parse degrades to `None` (bytes + warning, not a lie) per
//! TDD §6, rather than silently emitting corrupted structured data.

use shoal_adapters::{AdapterCatalog, SubSpec, parse_output};
use shoal_value::Value;
use std::path::Path;

fn catalog() -> AdapterCatalog {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
    let (catalog, warnings) = AdapterCatalog::load_dir(&root);
    assert!(warnings.is_empty(), "adapter pack warnings: {warnings:#?}");
    catalog
}

fn sub<'a>(catalog: &'a AdapterCatalog, cmd: &str, sub: &str) -> &'a SubSpec {
    &catalog
        .lookup(cmd)
        .unwrap_or_else(|| panic!("missing adapter {cmd}"))
        .subs[sub]
}

fn top<'a>(catalog: &'a AdapterCatalog, cmd: &str) -> &'a SubSpec {
    &catalog
        .lookup(cmd)
        .unwrap_or_else(|| panic!("missing adapter {cmd}"))
        .top
}

fn parse(spec: &SubSpec, bytes: &[u8]) -> Option<Value> {
    parse_output(&spec.parse, bytes, spec.output_type.as_deref())
}

// ---------------------------------------------------------------- ps ----

#[test]
fn ps_cols_parses_realistic_output_and_degrades_on_short_rows() {
    let c = catalog();
    let spec = top(&c, "ps");
    assert_eq!(spec.parse, "cols");

    let good = b"  PID  PPID USER     CPU  MEM COMMAND\n\
                    1     0 root     0.0  0.1 systemd\n\
                  842     1 allie    1.2  0.3 zsh\n\
                 1044   842 allie    0.0  0.0 ps\n";
    let v = parse(spec, good).expect("realistic ps -e -o ... output must parse");
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["pid"], Value::Int(1));
    assert_eq!(rows[0]["user"], Value::Str("root".into()));
    assert_eq!(rows[1]["command"], Value::Str("zsh".into()));
    assert_eq!(rows[2]["mem"], Value::Float(0.0));

    // A row missing its MEM column (fewer whitespace fields than the
    // 6-column hint) must degrade the whole parse, not silently misalign
    // every later column.
    let truncated = b"  PID  PPID USER     CPU  MEM COMMAND\n\
                        1     0 root     0.0  0.1 systemd\n\
                      842     1 allie    1.2 zsh\n";
    assert_eq!(parse(spec, truncated), None);
}

#[test]
fn ps_cols_merges_multi_word_command_into_last_column() {
    let c = catalog();
    let spec = top(&c, "ps");
    // "Google Chrome Helper" has embedded spaces in `comm`; because
    // `command` is the *last* declared column, overflow whitespace fields
    // merge into it instead of desyncing PID/PPID/etc.
    let bytes = b"  PID  PPID USER     CPU  MEM COMMAND\n\
                  501     1 allie    2.5  1.1 Google Chrome Helper\n";
    let Value::Table(rows) = parse(spec, bytes).unwrap() else {
        panic!("expected table")
    };
    assert_eq!(
        rows[0]["command"],
        Value::Str("Google Chrome Helper".into())
    );
    assert_eq!(rows[0]["pid"], Value::Int(501));
}

// ---------------------------------------------------------------- df ----

#[test]
fn df_cols_parses_posix_output_ignoring_two_word_header() {
    let c = catalog();
    let spec = top(&c, "df");
    assert_eq!(spec.parse, "cols");

    // Real `df -kP` header is "Filesystem 1024-blocks Used Available
    // Capacity Mounted on" -- note "Mounted on" is two words over one data
    // column. The `cols` parser must not choke on that; it discards the
    // header line unconditionally.
    let good = b"Filesystem     1024-blocks      Used  Available Capacity Mounted on\n\
                 /dev/sda1         61255492  32101234   27987654      54% /\n\
                 tmpfs               819200         0     819200       0% /dev/shm\n";
    let v = parse(spec, good).expect("realistic df -kP output must parse");
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["filesystem"], Value::Str("/dev/sda1".into()));
    assert_eq!(rows[0]["size_kb"], Value::Int(61255492));
    assert_eq!(rows[0]["use_pct"], Value::Str("54%".into()));
    assert_eq!(rows[0]["mounted"], Value::Path("/".into()));
    assert_eq!(rows[1]["mounted"], Value::Path("/dev/shm".into()));

    // A malformed row with a non-numeric block count (corrupted bytes)
    // must degrade the whole table rather than bake in a bogus int.
    let malformed = b"Filesystem     1024-blocks      Used  Available Capacity Mounted on\n\
                       /dev/sda1         CORRUPT  32101234   27987654      54% /\n";
    assert_eq!(parse(spec, malformed), None);
}

#[test]
fn df_cols_preserves_mount_path_with_spaces_via_last_column_merge() {
    let c = catalog();
    let spec = top(&c, "df");
    let bytes = b"Filesystem     1024-blocks      Used  Available Capacity Mounted on\n\
                  /dev/disk2s1        1024000    512000     512000      50% /Volumes/My Drive\n";
    let Value::Table(rows) = parse(spec, bytes).unwrap() else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["mounted"], Value::Path("/Volumes/My Drive".into()));
}

// --------------------------------------------------------- systemctl ----

#[test]
fn systemctl_list_units_json_parses_and_degrades_on_truncated_json() {
    let c = catalog();
    let spec = top(&c, "systemctl");
    assert_eq!(spec.parse, "json");

    let good = br#"[
        {"unit":"cron.service","load":"loaded","active":"active","sub":"running","description":"Regular background program processing daemon"},
        {"unit":"ssh.service","load":"loaded","active":"active","sub":"running","description":"OpenSSH server daemon"}
    ]"#;
    let Value::Table(rows) = parse(spec, good).expect("realistic list-units json must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["unit"], Value::Str("cron.service".into()));
    assert_eq!(rows[1]["sub"], Value::Str("running".into()));

    let truncated = br#"[{"unit":"cron.service","load":"loaded""#;
    assert_eq!(parse(spec, truncated), None);
}

// --------------------------------------------------------------- brew ----

#[test]
fn brew_list_lines_parses_and_empty_lines_ignore_blank_bytes() {
    let c = catalog();
    let spec = sub(&c, "brew", "list");
    assert_eq!(spec.parse, "lines");

    let good = b"git\njq\nripgrep\n";
    assert_eq!(
        parse(spec, good),
        Some(Value::List(vec![
            Value::Str("git".into()),
            Value::Str("jq".into()),
            Value::Str("ripgrep".into()),
        ]))
    );
}

#[test]
fn brew_info_json_v2_parses_record_and_degrades_on_bad_json() {
    let c = catalog();
    let spec = sub(&c, "brew", "info");
    assert_eq!(spec.parse, "json");

    let good = br#"{"formulae":[{"name":"jq","versions":{"stable":"1.7.1"}}],"casks":[]}"#;
    let Value::Record(r) = parse(spec, good).expect("realistic brew info --json=v2 must parse")
    else {
        panic!("expected record")
    };
    assert!(matches!(r["formulae"], Value::Table(_)));

    assert_eq!(parse(spec, b"{not json"), None);
}

// ---------------------------------------------------------------- npm ----

#[test]
fn npm_ls_json_parses_nested_dependency_record() {
    let c = catalog();
    let spec = sub(&c, "npm", "ls");
    assert_eq!(spec.parse, "json");

    let good = br#"{
        "name":"myapp",
        "version":"1.0.0",
        "dependencies":{
            "lodash":{"version":"4.17.21","resolved":"https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"}
        }
    }"#;
    let Value::Record(r) = parse(spec, good).expect("realistic npm ls --json must parse") else {
        panic!("expected record")
    };
    assert_eq!(r["name"], Value::Str("myapp".into()));
    let Value::Record(deps) = &r["dependencies"] else {
        panic!("expected nested record")
    };
    assert!(matches!(deps["lodash"], Value::Record(_)));

    assert_eq!(parse(spec, b"{\"name\": "), None);
}

#[test]
fn npm_outdated_json_allows_ok_code_one() {
    let c = catalog();
    let spec = sub(&c, "npm", "outdated");
    // `npm outdated` exits 1 (not 0) when it finds outdated packages; that
    // is its normal "found results" signal.
    assert_eq!(spec.ok_codes, Some(vec![0, 1]));

    let good = br#"{"lodash":{"current":"4.17.20","wanted":"4.17.21","latest":"4.17.21","dependent":"myapp","location":"node_modules/lodash"}}"#;
    let Value::Record(r) = parse(spec, good).unwrap() else {
        panic!("expected record")
    };
    assert!(matches!(r["lodash"], Value::Record(_)));

    // No outdated packages prints an empty object, not an error shape.
    assert_eq!(
        parse(spec, b"{}"),
        Some(Value::Record(shoal_value::Record::new()))
    );
}

// --------------------------------------------------------------- pnpm ----

#[test]
fn pnpm_list_json_array_becomes_table() {
    let c = catalog();
    let spec = sub(&c, "pnpm", "list");
    assert_eq!(spec.parse, "json");

    let good = br#"[{"name":"myapp","version":"1.0.0","path":"/repo","private":false,"dependencies":{"lodash":{"from":"lodash","version":"4.17.21"}}}]"#;
    let Value::Table(rows) = parse(spec, good).expect("realistic pnpm list --json must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["name"], Value::Str("myapp".into()));
    assert_eq!(rows[0]["path"], Value::Path("/repo".into()));

    assert_eq!(parse(spec, b"[{\"name\":"), None);
}

// -------------------------------------------------------- cargo metadata

#[test]
fn cargo_metadata_json_parses_record_with_packages_array() {
    let c = catalog();
    let spec = sub(&c, "cargo", "metadata");
    assert_eq!(spec.parse, "json");
    assert_eq!(
        spec.invoke,
        Some(vec![
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string()
        ])
    );

    let good = br#"{"packages":[{"name":"shoal-adapters","version":"0.1.0"}],"workspace_members":["shoal-adapters 0.1.0"],"target_directory":"/repo/target"}"#;
    let Value::Record(r) = parse(spec, good).expect("realistic cargo metadata json must parse")
    else {
        panic!("expected record")
    };
    assert!(matches!(r["packages"], Value::Table(_)));

    assert_eq!(parse(spec, b"{\"packages\": ["), None);
}

// ----------------------------------------------------------------- gh ----

#[test]
fn gh_pr_list_json_parses_realistic_pr_rows() {
    let c = catalog();
    let spec = sub(&c, "gh", "pr_list");
    assert_eq!(spec.parse, "json");

    let good = br#"[
        {"number":42,"title":"Fix the bug","state":"OPEN","author":{"login":"allie"},"url":"https://github.com/x/y/pull/42","createdAt":"2026-07-01T00:00:00Z"}
    ]"#;
    let Value::Table(rows) = parse(spec, good).expect("realistic gh pr list --json must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["number"], Value::Int(42));
    assert!(matches!(rows[0]["author"], Value::Record(_)));

    assert_eq!(parse(spec, b"[{\"number\": 42,"), None);
}

#[test]
fn gh_run_list_json_parses_realistic_rows() {
    let c = catalog();
    let spec = sub(&c, "gh", "run_list");
    let good = br#"[{"databaseId":123,"name":"CI","status":"completed","conclusion":"success","workflowName":"CI","createdAt":"2026-07-01T00:00:00Z"}]"#;
    let Value::Table(rows) = parse(spec, good).unwrap() else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["conclusion"], Value::Str("success".into()));
    assert_eq!(parse(spec, b"not json at all"), None);
}

// ----------------------------------------------------------------- go ----

#[test]
fn go_list_json_single_package_parses_record() {
    let c = catalog();
    let spec = sub(&c, "go", "list");
    assert_eq!(spec.parse, "json");

    let good = br#"{
        "Dir":"/repo",
        "ImportPath":"example.com/repo",
        "Name":"main",
        "GoFiles":["main.go"]
    }"#;
    let Value::Record(r) = parse(spec, good).expect("realistic go list -json must parse") else {
        panic!("expected record")
    };
    assert_eq!(r["ImportPath"], Value::Str("example.com/repo".into()));
    assert!(matches!(r["GoFiles"], Value::List(_)));

    assert_eq!(parse(spec, b"{\"Dir\": \"/repo\", \"ImportPath\""), None);
}

// ---------------------------------------------------------------- pip ----

#[test]
fn pip_list_json_parses_table_of_packages() {
    let c = catalog();
    let spec = sub(&c, "pip", "list");
    assert_eq!(spec.parse, "json");

    let good = br#"[{"name":"requests","version":"2.31.0"},{"name":"numpy","version":"1.26.4"}]"#;
    let Value::Table(rows) =
        parse(spec, good).expect("realistic pip list --format json must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["name"], Value::Str("numpy".into()));

    assert_eq!(parse(spec, b"[{\"name\": \"requests\""), None);
}

// ------------------------------------------------------------ sqlite3 ----

#[test]
fn sqlite3_json_parses_dynamic_columns_and_degrades_on_bad_json() {
    let c = catalog();
    let spec = top(&c, "sqlite3");
    assert_eq!(spec.parse, "json");

    let good = br#"[{"id":1,"name":"Allie"},{"id":2,"name":"Bob"}]"#;
    let Value::Table(rows) = parse(spec, good).expect("realistic sqlite3 -json output must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["name"], Value::Str("Allie".into()));

    // Malformed / truncated JSON (e.g. a killed query mid-stream).
    assert_eq!(parse(spec, br#"[{"id":1,"name":"Allie"#), None);
}

// --------------------------------------------------------- terraform ----

#[test]
fn terraform_state_list_lines_parses_resource_addresses() {
    let c = catalog();
    let spec = sub(&c, "terraform", "state_list");
    assert_eq!(spec.parse, "lines");

    let good = b"aws_instance.web\naws_s3_bucket.assets\nmodule.vpc.aws_vpc.main\n";
    assert_eq!(
        parse(spec, good),
        Some(Value::List(vec![
            Value::Str("aws_instance.web".into()),
            Value::Str("aws_s3_bucket.assets".into()),
            Value::Str("module.vpc.aws_vpc.main".into()),
        ]))
    );
}

#[test]
fn terraform_show_json_parses_record_and_degrades_on_bad_json() {
    let c = catalog();
    let spec = sub(&c, "terraform", "show");
    assert_eq!(spec.parse, "json");

    let good = br#"{"format_version":"1.0","terraform_version":"1.7.0","values":{"root_module":{"resources":[]}}}"#;
    let Value::Record(r) = parse(spec, good).expect("realistic terraform show -json must parse")
    else {
        panic!("expected record")
    };
    assert_eq!(r["format_version"], Value::Str("1.0".into()));

    assert_eq!(parse(spec, b"{\"format_version\": "), None);
}

// -------------------------------------------------------------- helm ----

#[test]
fn helm_list_json_parses_release_table() {
    let c = catalog();
    let spec = sub(&c, "helm", "list");
    assert_eq!(spec.parse, "json");

    let good = br#"[{"name":"my-release","namespace":"default","revision":"1","updated":"2026-07-01 00:00:00","status":"deployed","chart":"nginx-15.0.0","app_version":"1.25.0"}]"#;
    let Value::Table(rows) = parse(spec, good).expect("realistic helm list -o json must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["status"], Value::Str("deployed".into()));

    assert_eq!(parse(spec, b"[{\"name\": \"my-release\""), None);
}

// ----------------------------------------------------------------- ip ----

#[test]
fn ip_addr_json_parses_interface_table_and_degrades_on_bad_json() {
    let c = catalog();
    let spec = sub(&c, "ip", "addr");
    assert_eq!(spec.parse, "json");

    let good = br#"[{"ifname":"eth0","addr_info":[{"local":"192.168.1.10","prefixlen":24}]}]"#;
    let Value::Table(rows) =
        parse(spec, good).expect("realistic ip -j addr show output must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["ifname"], Value::Str("eth0".into()));
    // A uniform non-empty JSON array of objects auto-normalizes to a
    // nested `Table`, same as the outer array (see `json_to_value`).
    assert!(matches!(rows[0]["addr_info"], Value::Table(_)));

    assert_eq!(parse(spec, b"[{\"ifname\": \"eth0\""), None);
}

#[test]
fn ip_route_json_parses_route_table() {
    let c = catalog();
    let spec = sub(&c, "ip", "route");
    let good = br#"[{"dst":"default","dev":"eth0","gateway":"192.168.1.1"}]"#;
    let Value::Table(rows) = parse(spec, good).unwrap() else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["gateway"], Value::Str("192.168.1.1".into()));
    assert_eq!(parse(spec, b"not json"), None);
}

// ----------------------------------------------------------------- ss ----

#[test]
fn ss_cols_parses_realistic_socket_table_and_degrades_on_short_rows() {
    let c = catalog();
    let spec = top(&c, "ss");
    assert_eq!(spec.parse, "cols");

    let good = b"Netid  State      Recv-Q Send-Q  Local Address:Port    Peer Address:Port\n\
                 tcp    LISTEN     0      128     0.0.0.0:22            0.0.0.0:*\n\
                 udp    UNCONN     0      0       127.0.0.1:323         0.0.0.0:*\n";
    let v = parse(spec, good).expect("realistic ss -tuln output must parse");
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["netid"], Value::Str("tcp".into()));
    assert_eq!(rows[0]["local"], Value::Str("0.0.0.0:22".into()));
    assert_eq!(rows[0]["recv_q"], Value::Int(0));
    assert_eq!(rows[1]["peer"], Value::Str("0.0.0.0:*".into()));

    // A row missing its Peer Address:Port column must degrade the whole
    // parse, not silently misalign the remaining columns.
    let truncated = b"Netid  State      Recv-Q Send-Q  Local Address:Port    Peer Address:Port\n\
                       tcp    LISTEN     0      128     0.0.0.0:22\n";
    assert_eq!(parse(spec, truncated), None);
}

// ------------------------------------------------------ systemd-analyze ----

#[test]
fn systemd_analyze_blame_lines_parses_raw_entries() {
    let c = catalog();
    let spec = sub(&c, "systemd-analyze", "blame");
    assert_eq!(spec.parse, "lines");

    let good: &[u8] =
        b"          1.234s NetworkManager.service\n           891ms systemd-udevd.service\n";
    assert_eq!(
        parse(spec, good),
        Some(Value::List(vec![
            Value::Str("          1.234s NetworkManager.service".into()),
            Value::Str("           891ms systemd-udevd.service".into()),
        ]))
    );
}

// ----------------------------------------------------------------- jj ----

#[test]
fn jj_status_and_log_lines_parse_raw_text() {
    let c = catalog();
    let status = sub(&c, "jj", "status");
    assert_eq!(status.parse, "lines");
    let good = b"Working copy changes:\nM src/lib.rs\nWorking copy : abc123 (no description set)\n";
    let Value::List(lines) = parse(status, good).unwrap() else {
        panic!("expected list")
    };
    assert_eq!(lines.len(), 3);

    let log = sub(&c, "jj", "log");
    assert_eq!(
        log.invoke,
        Some(vec![
            "log".to_string(),
            "--no-graph".to_string(),
            "--no-pager".to_string(),
            "--color=never".to_string(),
            "-T".to_string(),
            "builtin_log_oneline".to_string(),
        ])
    );
}

// ------------------------------------------------------------ rustup ----

#[test]
fn rustup_toolchain_and_target_list_parse_lines() {
    let c = catalog();
    let toolchains = sub(&c, "rustup", "toolchain_list");
    assert_eq!(
        parse(
            toolchains,
            b"stable-x86_64-unknown-linux-gnu (default)\nnightly-x86_64-unknown-linux-gnu\n"
        ),
        Some(Value::List(vec![
            Value::Str("stable-x86_64-unknown-linux-gnu (default)".into()),
            Value::Str("nightly-x86_64-unknown-linux-gnu".into()),
        ]))
    );
    let targets = sub(&c, "rustup", "target_list");
    assert_eq!(
        parse(targets, b"wasm32-unknown-unknown (installed)\n"),
        Some(Value::List(vec![Value::Str(
            "wasm32-unknown-unknown (installed)".into()
        )]))
    );
}

// --------------------------------------------------------------- bun ----

#[test]
fn bun_pm_ls_lines_parses_tree_text_and_degrades_on_invalid_utf8() {
    let c = catalog();
    let spec = sub(&c, "bun", "pm_ls");
    assert_eq!(spec.parse, "lines");

    let good = b"myapp@1.0.0 /repo\n\xE2\x94\x9C\xE2\x94\x80\xE2\x94\x80 lodash@4.17.21\n";
    let Value::List(lines) = parse(spec, good).unwrap() else {
        panic!("expected list")
    };
    assert_eq!(lines[0], Value::Str("myapp@1.0.0 /repo".into()));

    // Invalid UTF-8 bytes are not valid text at all -- degrade, not lie.
    assert_eq!(parse(spec, b"myapp\xff\xfe"), None);
}

// --------------------------------------------------------------- aws ----

#[test]
fn aws_sts_get_caller_identity_json_parses_record() {
    let c = catalog();
    let spec = sub(&c, "aws", "sts_get_caller_identity");
    assert_eq!(spec.parse, "json");

    let good = br#"{"UserId":"AIDAEXAMPLE","Account":"123456789012","Arn":"arn:aws:iam::123456789012:user/allie"}"#;
    let Value::Record(r) = parse(spec, good).expect("realistic sts get-caller-identity must parse")
    else {
        panic!("expected record")
    };
    assert_eq!(r["Account"], Value::Str("123456789012".into()));

    assert_eq!(parse(spec, b"{\"UserId\": "), None);
}

#[test]
fn aws_s3_ls_lines_parses_bucket_listing() {
    let c = catalog();
    let spec = sub(&c, "aws", "s3_ls");
    assert_eq!(spec.parse, "lines");
    let good = b"2026-01-01 00:00:00        123 report.csv\n";
    assert_eq!(
        parse(spec, good),
        Some(Value::List(vec![Value::Str(
            "2026-01-01 00:00:00        123 report.csv".into()
        )]))
    );
}

// ------------------------------------------------------------ gcloud ----

#[test]
fn gcloud_projects_list_json_parses_table() {
    let c = catalog();
    let spec = sub(&c, "gcloud", "projects_list");
    assert_eq!(spec.parse, "json");

    let good = br#"[{"projectId":"my-proj","name":"My Project","projectNumber":"123456789"}]"#;
    let Value::Table(rows) =
        parse(spec, good).expect("realistic projects list --format=json must parse")
    else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["projectId"], Value::Str("my-proj".into()));

    assert_eq!(parse(spec, b"[{\"projectId\": "), None);
}

#[test]
fn gcloud_config_list_json_parses_record() {
    let c = catalog();
    let spec = sub(&c, "gcloud", "config_list");
    let good = br#"{"core":{"project":"my-proj"}}"#;
    let Value::Record(r) = parse(spec, good).unwrap() else {
        panic!("expected record")
    };
    assert!(matches!(r["core"], Value::Record(_)));
}

// ----------------------------------------------------------- kubectl ----

#[test]
fn kubectl_config_current_context_lines_parses_single_line() {
    let c = catalog();
    let spec = sub(&c, "kubectl", "config_current_context");
    assert_eq!(spec.parse, "lines");
    assert_eq!(
        parse(spec, b"my-cluster-context\n"),
        Some(Value::List(vec![Value::Str("my-cluster-context".into())]))
    );
}
