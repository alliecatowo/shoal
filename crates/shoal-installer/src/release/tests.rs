use super::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;
use std::io::Write;
use tempfile::TempDir;

const ROOT: &str = "shoal-v0.1.0-x86_64-unknown-linux-gnu";
const EPOCH: u64 = 1_700_000_000;

fn entry(relative: &str, mode: u64, data: Vec<u8>) -> Entry {
    Entry {
        path: format!("{ROOT}/{relative}"),
        mode,
        uid: 0,
        gid: 0,
        mtime: EPOCH,
        uname: String::new(),
        gname: String::new(),
        data,
    }
}

fn fixture_entries() -> Vec<Entry> {
    let mut entries = allowed_relative_paths()
        .into_iter()
        .map(|(relative, mode)| {
            let data = match relative.as_str() {
                "Cargo.lock" => b"version = 4\n\n[[package]]\nname = \"dep\"\nversion = \"1.2.3\"\n\n[[package]]\nname = \"shoal\"\nversion = \"0.1.0\"\n".to_vec(),
                "BUILDINFO.json" => Vec::new(),
                _ => format!("fixture:{relative}").into_bytes(),
            };
            entry(&relative, mode, data)
        })
        .collect::<Vec<_>>();
    let binaries = CORE_BINARIES
        .into_iter()
        .chain(RELEASE_HELPERS)
        .map(|name| {
            let data = &entries.iter().find(|entry| entry.path == format!("{ROOT}/{name}")).unwrap().data;
            json!({"name": name, "bytes": data.len(), "sha256": format!("{:x}", Sha256::digest(data))})
        })
        .collect::<Vec<_>>();
    let buildinfo = json!({
        "schema": 1,
        "name": "shoal",
        "version": "0.1.0",
        "tag": "v0.1.0",
        "target": "x86_64-unknown-linux-gnu",
        "host": "x86_64-unknown-linux-gnu",
        "commit": "0123456789abcdef0123456789abcdef01234567",
        "source_date_epoch": EPOCH.to_string(),
        "archive": {"format": "ustar", "owner": 0, "group": 0, "order": "lexical", "timestamps": "source_date_epoch", "gzip_name_and_time": false, "apple_double": false},
        "binaries": binaries,
    });
    entries
        .iter_mut()
        .find(|entry| entry.path.ends_with("/BUILDINFO.json"))
        .unwrap()
        .data = serde_json::to_vec(&buildinfo).unwrap();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn valid_sbom() -> Value {
    let readme = b"fixture:README.md";
    json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": ROOT,
        "documentNamespace": format!("https://spdx.example/{ROOT}/fixture"),
        "creationInfo": {"created": "2023-11-14T22:13:20Z", "creators": ["Tool: fixture"]},
        "packages": [
            {"SPDXID": "SPDXRef-Root", "name": ROOT, "versionInfo": "0.1.0"},
            {"SPDXID": "SPDXRef-Dep", "name": "dep", "versionInfo": "1.2.3"},
            {"SPDXID": "SPDXRef-Shoal", "name": "shoal", "versionInfo": "0.1.0"}
        ],
        "files": [{
            "SPDXID": "SPDXRef-File-README",
            "fileName": "/README.md",
            "checksums": [{"algorithm": "SHA256", "checksumValue": format!("{:x}", Sha256::digest(readme))}]
        }],
        "relationships": [
            {"spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES", "relatedSpdxElement": "SPDXRef-Root"},
            {"spdxElementId": "SPDXRef-Root", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": "SPDXRef-Dep"},
            {"spdxElementId": "SPDXRef-Root", "relationshipType": "CONTAINS", "relatedSpdxElement": "SPDXRef-Shoal"}
        ]
    })
}

fn write_fixture(
    entries: &[Entry],
    sbom: &Value,
) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join(format!("{ROOT}.tar.gz"));
    let sbom_path = temporary.path().join(format!("{ROOT}.tar.gz.spdx.json"));
    std::fs::write(&archive, encode(entries, None)).unwrap();
    std::fs::write(&sbom_path, serde_json::to_vec(sbom).unwrap()).unwrap();
    (temporary, archive, sbom_path)
}

fn encode(entries: &[Entry], mutate_type_at: Option<usize>) -> Vec<u8> {
    let mut tar = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let mut header = [0_u8; 512];
        put_text(&mut header[0..100], &entry.path);
        put_octal(&mut header[100..108], entry.mode);
        put_octal(&mut header[108..116], entry.uid);
        put_octal(&mut header[116..124], entry.gid);
        put_octal(&mut header[124..136], entry.data.len() as u64);
        put_octal(&mut header[136..148], entry.mtime);
        header[148..156].fill(b' ');
        header[156] = if mutate_type_at == Some(index) {
            b'2'
        } else {
            b'0'
        };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum = header.iter().map(|byte| u64::from(*byte)).sum();
        put_checksum(&mut header[148..156], checksum);
        tar.extend_from_slice(&header);
        tar.extend_from_slice(&entry.data);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.resize(tar.len() + 1024, 0);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
    gzip.write_all(&tar).unwrap();
    gzip.finish().unwrap()
}

fn mutate_first_header(entries: &[Entry], mutation: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let encoded = encode(entries, None);
    let (mut tar, _) = decode_gzip_bounded(&encoded, MAX_ARCHIVE_DECOMPRESSED_BYTES).unwrap();
    mutation(&mut tar[..512]);
    tar[148..156].fill(b' ');
    let checksum = tar[..512].iter().map(|byte| u64::from(*byte)).sum();
    put_checksum(&mut tar[148..156], checksum);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
    gzip.write_all(&tar).unwrap();
    gzip.finish().unwrap()
}

fn assert_encoded_rejected(encoded: &[u8], needle: &str) {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join(format!("{ROOT}.tar.gz"));
    let sbom = temporary.path().join("fixture.spdx.json");
    std::fs::write(&archive, encoded).unwrap();
    std::fs::write(&sbom, serde_json::to_vec(&valid_sbom()).unwrap()).unwrap();
    let error = verify(&archive, &sbom).unwrap_err().to_string();
    assert!(
        error.contains(needle),
        "{error:?} does not contain {needle:?}"
    );
}

fn put_text(field: &mut [u8], value: &str) {
    assert!(value.len() < field.len());
    field[..value.len()].copy_from_slice(value.as_bytes());
}

fn put_octal(field: &mut [u8], value: u64) {
    let text = format!("{:0width$o}\0", value, width = field.len() - 1);
    field.copy_from_slice(text.as_bytes());
}

fn put_checksum(field: &mut [u8], value: u64) {
    let text = format!("{value:06o}\0 ");
    field.copy_from_slice(text.as_bytes());
}

fn assert_rejected(entries: &[Entry], sbom: &Value, needle: &str) {
    let (_temporary, archive, sbom_path) = write_fixture(entries, sbom);
    let error = verify(&archive, &sbom_path).unwrap_err().to_string();
    assert!(
        error.contains(needle),
        "{error:?} does not contain {needle:?}"
    );
}

#[test]
fn complete_semantic_fixture_passes() {
    let (_temporary, archive, sbom) = write_fixture(&fixture_entries(), &valid_sbom());
    verify(&archive, &sbom).unwrap();
}

#[test]
fn reordered_tar_fails() {
    let mut entries = fixture_entries();
    entries.swap(0, 1);
    assert_rejected(&entries, &valid_sbom(), "lexical order");
}

#[test]
fn wrong_uid_gid_mode_and_mtime_fail() {
    for mutation in 0..4 {
        let mut entries = fixture_entries();
        match mutation {
            0 => entries[0].uid = 1,
            1 => entries[0].gid = 1,
            2 => entries[0].mode = 0o777,
            _ => entries[0].mtime += 1,
        }
        assert_rejected(
            &entries,
            &valid_sbom(),
            if mutation == 2 {
                "mode"
            } else if mutation == 3 {
                "mtime"
            } else {
                "ownership"
            },
        );
    }
}

#[test]
fn noncanonical_ustar_header_fields_fail() {
    let entries = fixture_entries();
    assert_encoded_rejected(
        &mutate_first_header(&entries, |header| header[157] = b'x'),
        "link name",
    );
    assert_encoded_rejected(
        &mutate_first_header(&entries, |header| header[329] = b'0'),
        "device numbers",
    );
    assert_encoded_rejected(
        &mutate_first_header(&entries, |header| header[500] = 1),
        "reserved header bytes",
    );
    assert_encoded_rejected(
        &mutate_first_header(&entries, |header| {
            header[100..108].copy_from_slice(b"0000755 ")
        }),
        "non-canonical ustar mode encoding",
    );
    assert_encoded_rejected(
        &mutate_first_header(&entries, |header| header[265..269].copy_from_slice(b"root")),
        "owner/group names",
    );
}

#[test]
fn link_type_fails_before_extraction() {
    let entries = fixture_entries();
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join(format!("{ROOT}.tar.gz"));
    let sbom = temporary.path().join("fixture.spdx.json");
    std::fs::write(&archive, encode(&entries, Some(0))).unwrap();
    std::fs::write(&sbom, serde_json::to_vec(&valid_sbom()).unwrap()).unwrap();
    assert!(
        verify(&archive, &sbom)
            .unwrap_err()
            .to_string()
            .contains("type flag")
    );
}

#[test]
fn plausible_buildinfo_normalization_lie_fails() {
    let mut entries = fixture_entries();
    let entry = entries
        .iter_mut()
        .find(|entry| entry.path.ends_with("/BUILDINFO.json"))
        .unwrap();
    let mut info: Value = serde_json::from_slice(&entry.data).unwrap();
    info["source_date_epoch"] = Value::String((EPOCH + 1).to_string());
    entry.data = serde_json::to_vec(&info).unwrap();
    assert_rejected(&entries, &valid_sbom(), "mtime disagrees");
}

#[test]
fn minimal_valid_looking_spdx_fails() {
    let sbom = json!({"spdxVersion":"SPDX-2.3", "SPDXID":"SPDXRef-DOCUMENT", "packages":[{}]});
    assert_rejected(&fixture_entries(), &sbom, "dataLicense");
}

#[test]
fn wrong_root_version_fails() {
    let mut sbom = valid_sbom();
    sbom["packages"][0]["versionInfo"] = Value::String("9.9.9".to_owned());
    assert_rejected(&fixture_entries(), &sbom, "release-root package");
}

#[test]
fn missing_locked_dependency_fails() {
    let mut sbom = valid_sbom();
    sbom["packages"]
        .as_array_mut()
        .unwrap()
        .retain(|package| package["name"] != "dep");
    assert_rejected(&fixture_entries(), &sbom, "coverage mismatch");
}

#[test]
fn missing_locked_dependency_relationship_fails() {
    let mut sbom = valid_sbom();
    sbom["relationships"]
        .as_array_mut()
        .unwrap()
        .retain(|relationship| relationship["relatedSpdxElement"] != "SPDXRef-Dep");
    assert_rejected(&fixture_entries(), &sbom, "not reachable");
}

#[test]
fn dangling_self_and_wrong_direction_relationships_fail() {
    let mut dangling = valid_sbom();
    dangling["relationships"][1]["relatedSpdxElement"] =
        Value::String("SPDXRef-Missing".to_owned());
    assert_rejected(&fixture_entries(), &dangling, "unknown endpoint");

    let mut self_reference = valid_sbom();
    self_reference["relationships"][1]["relatedSpdxElement"] =
        Value::String("SPDXRef-Root".to_owned());
    assert_rejected(&fixture_entries(), &self_reference, "self-referential");

    let mut reversed = valid_sbom();
    reversed["relationships"][1]["spdxElementId"] = Value::String("SPDXRef-Dep".to_owned());
    reversed["relationships"][1]["relatedSpdxElement"] = Value::String("SPDXRef-Root".to_owned());
    assert_rejected(&fixture_entries(), &reversed, "not reachable");

    let mut unrelated = valid_sbom();
    unrelated["relationships"][1]["spdxElementId"] = Value::String("SPDXRef-Dep".to_owned());
    unrelated["relationships"][1]["relatedSpdxElement"] = Value::String("SPDXRef-Shoal".to_owned());
    assert_rejected(&fixture_entries(), &unrelated, "not reachable");
}

#[test]
fn represented_file_checksum_must_match_archive_bytes() {
    let mut sbom = valid_sbom();
    sbom["files"][0]["checksums"][0]["checksumValue"] = Value::String("0".repeat(64));
    assert_rejected(&fixture_entries(), &sbom, "SHA256 disagrees");
}

#[test]
fn concatenated_gzip_member_fails() {
    let entries = fixture_entries();
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join(format!("{ROOT}.tar.gz"));
    let sbom = temporary.path().join("fixture.spdx.json");
    let mut compressed = encode(&entries, None);
    compressed.extend_from_slice(&encode(&[], None));
    std::fs::write(&archive, compressed).unwrap();
    std::fs::write(&sbom, serde_json::to_vec(&valid_sbom()).unwrap()).unwrap();
    assert!(
        verify(&archive, &sbom)
            .unwrap_err()
            .to_string()
            .contains("trailing or concatenated")
    );
}

#[test]
fn bounded_read_rejects_oversized_semantic_inputs() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("oversized");
    std::fs::write(&input, b"12345").unwrap();
    let error = read_file_bounded(&input, 4, "fixture")
        .unwrap_err()
        .to_string();
    assert!(error.contains("4-byte limit"), "{error}");

    let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
    gzip.write_all(b"12345").unwrap();
    let compressed = gzip.finish().unwrap();
    let error = decode_gzip_bounded(&compressed, 4).unwrap_err().to_string();
    assert!(error.contains("4-byte limit"), "{error}");
}

#[test]
fn declared_archive_entry_size_is_bounded_before_payload_allocation() {
    let mut header = [0_u8; 512];
    put_text(&mut header[0..100], &format!("{ROOT}/huge"));
    put_octal(&mut header[100..108], 0o644);
    put_octal(&mut header[108..116], 0);
    put_octal(&mut header[116..124], 0);
    put_octal(&mut header[124..136], (MAX_ARCHIVE_ENTRY_BYTES as u64) + 1);
    put_octal(&mut header[136..148], EPOCH);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum();
    put_checksum(&mut header[148..156], checksum);
    let mut tar = header.to_vec();
    tar.resize(tar.len() + 1024, 0);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
    gzip.write_all(&tar).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("oversized.tar.gz");
    std::fs::write(&archive, gzip.finish().unwrap()).unwrap();

    let error = match read_archive(&archive) {
        Ok(_) => panic!("oversized declared entry unexpectedly passed"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("archive entry exceeds"),
        "the declared size must be rejected before truncation/payload handling: {error}",
    );
}

#[test]
fn invalid_creation_timestamp_and_duplicate_identity_fail() {
    let mut timestamp = valid_sbom();
    timestamp["creationInfo"]["created"] = Value::String("2023-99-99T99:99:99Z".to_owned());
    assert_rejected(&fixture_entries(), &timestamp, "creation timestamp");

    let mut duplicate = valid_sbom();
    let mut package = duplicate["packages"][1].clone();
    package["SPDXID"] = Value::String("SPDXRef-DuplicateDep".to_owned());
    duplicate["packages"].as_array_mut().unwrap().push(package);
    assert_rejected(
        &fixture_entries(),
        &duplicate,
        "duplicate SPDX package identity",
    );
}
