use flate2::bufread::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;

const CORE_BINARIES: [&str; 10] = [
    "shoal",
    "shoal-kernel",
    "shoal-mcp",
    "shoal-lsp",
    "shoal-token",
    "shoal-secret",
    "shoal-history",
    "shoal-doctor",
    "shoal-sandbox-exec",
    "shoal-landlock-helper",
];
const RELEASE_HELPERS: [&str; 2] = ["shoal-install-transaction", "shoal-release-inspect"];
const MAX_ARCHIVE_COMPRESSED_BYTES: usize = 256 * 1024 * 1024;
const MAX_ARCHIVE_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
const MAX_ARCHIVE_ENTRY_BYTES: usize = 128 * 1024 * 1024;
const MAX_SBOM_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct BuildInfo {
    schema: u64,
    name: String,
    version: String,
    tag: String,
    target: String,
    host: String,
    commit: String,
    source_date_epoch: String,
    archive: ArchiveClaims,
    binaries: Vec<BinaryClaim>,
}

#[derive(Debug, Deserialize)]
struct ArchiveClaims {
    format: String,
    owner: u64,
    group: u64,
    order: String,
    timestamps: String,
    gzip_name_and_time: bool,
    apple_double: bool,
}

#[derive(Debug, Deserialize)]
struct BinaryClaim {
    name: String,
    bytes: u64,
    sha256: String,
}

struct Entry {
    path: String,
    mode: u64,
    uid: u64,
    gid: u64,
    mtime: u64,
    uname: String,
    gname: String,
    data: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct Verification {
    pub summary: String,
    pub tag: String,
    pub commit: String,
}

pub fn verify(archive: &Path, sbom: &Path) -> io::Result<Verification> {
    let file_name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("archive filename is not UTF-8"))?;
    let root = file_name
        .strip_suffix(".tar.gz")
        .ok_or_else(|| invalid("release archive must end in .tar.gz"))?;
    let entries = read_archive(archive)?;
    let by_path = validate_inventory(root, &entries)?;
    let buildinfo: BuildInfo = serde_json::from_slice(
        &by_path
            .get(&format!("{root}/BUILDINFO.json"))
            .ok_or_else(|| invalid("archive lacks BUILDINFO.json"))?
            .data,
    )
    .map_err(|error| invalid(format!("invalid BUILDINFO.json: {error}")))?;
    validate_buildinfo(root, &buildinfo, &entries, &by_path)?;
    validate_sbom(
        sbom,
        root,
        &buildinfo.version,
        &by_path,
        &by_path
            .get(&format!("{root}/Cargo.lock"))
            .ok_or_else(|| invalid("archive lacks Cargo.lock"))?
            .data,
    )?;
    Ok(Verification {
        summary: format!(
            "semantically verified {} ustar entries and SPDX dependency coverage",
            entries.len()
        ),
        tag: buildinfo.tag,
        commit: buildinfo.commit,
    })
}

fn read_archive(path: &Path) -> io::Result<Vec<Entry>> {
    let compressed = read_file_bounded(path, MAX_ARCHIVE_COMPRESSED_BYTES, "release archive")?;
    validate_gzip_header(&compressed)?;
    let (bytes, consumed) = decode_gzip_bounded(&compressed, MAX_ARCHIVE_DECOMPRESSED_BYTES)?;
    if consumed != compressed.len() as u64 {
        return Err(invalid(
            "gzip archive contains trailing or concatenated data",
        ));
    }
    if bytes.len() % 512 != 0 {
        return Err(invalid("tar stream is not block aligned"));
    }
    let mut offset = 0_usize;
    let mut entries = Vec::new();
    let mut end_blocks = 0;
    while offset < bytes.len() {
        let header = &bytes[offset..offset + 512];
        offset += 512;
        if header.iter().all(|byte| *byte == 0) {
            end_blocks += 1;
            continue;
        }
        if end_blocks != 0 {
            return Err(invalid("tar contains data after an end-of-archive block"));
        }
        validate_checksum(header)?;
        if &header[257..263] != b"ustar\0" || &header[263..265] != b"00" {
            return Err(invalid("archive entry is not POSIX ustar"));
        }
        let entry_type = header[156];
        if entry_type != b'0' {
            return Err(invalid(format!(
                "archive contains forbidden type flag 0x{entry_type:02x}"
            )));
        }
        let name = field(&header[0..100], "name")?;
        if header[157..257].iter().any(|byte| *byte != 0) {
            return Err(invalid("regular ustar entry carries a link name"));
        }
        let uname = field(&header[265..297], "uname")?;
        let gname = field(&header[297..329], "gname")?;
        if header[329..345].iter().any(|byte| *byte != 0) {
            return Err(invalid("regular ustar entry carries device numbers"));
        }
        let prefix = field(&header[345..500], "prefix")?;
        if header[500..512].iter().any(|byte| *byte != 0) {
            return Err(invalid("ustar reserved header bytes are not zero"));
        }
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let mode = octal(&header[100..108], "mode")?;
        let uid = octal(&header[108..116], "uid")?;
        let gid = octal(&header[116..124], "gid")?;
        let size = usize::try_from(octal(&header[124..136], "size")?)
            .map_err(|_| invalid("archive entry is too large"))?;
        if size > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(invalid(format!(
                "archive entry exceeds the {MAX_ARCHIVE_ENTRY_BYTES}-byte limit: {path}"
            )));
        }
        let mtime = octal(&header[136..148], "mtime")?;
        let padded = size
            .checked_add(511)
            .ok_or_else(|| invalid("archive entry size overflow"))?
            / 512
            * 512;
        let end = offset
            .checked_add(padded)
            .ok_or_else(|| invalid("archive entry size overflow"))?;
        if end > bytes.len() {
            return Err(invalid(format!("truncated archive entry: {path}")));
        }
        if bytes[offset + size..end].iter().any(|byte| *byte != 0) {
            return Err(invalid(format!("non-zero ustar padding for {path}")));
        }
        if entries.len() >= MAX_ARCHIVE_ENTRIES {
            return Err(invalid(format!(
                "archive contains more than {MAX_ARCHIVE_ENTRIES} entries"
            )));
        }
        entries.push(Entry {
            path,
            mode,
            uid,
            gid,
            mtime,
            uname,
            gname,
            data: bytes[offset..offset + size].to_vec(),
        });
        offset = end;
    }
    if end_blocks < 2 {
        return Err(invalid("tar lacks two end-of-archive blocks"));
    }
    Ok(entries)
}

fn read_file_bounded(path: &Path, max: usize, label: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(max.min(64 * 1024));
    File::open(path)?
        .take(max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(invalid(format!("{label} exceeds the {max}-byte limit")));
    }
    Ok(bytes)
}

fn decode_gzip_bounded(compressed: &[u8], max: usize) -> io::Result<(Vec<u8>, u64)> {
    let mut bytes = Vec::with_capacity(max.min(64 * 1024));
    let mut decoder = GzDecoder::new(Cursor::new(compressed));
    decoder
        .by_ref()
        .take(max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(invalid(format!(
            "release archive expands beyond the {max}-byte limit"
        )));
    }
    let consumed = decoder.into_inner().position();
    Ok((bytes, consumed))
}

fn validate_gzip_header(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() < 18 || bytes[0..3] != [0x1f, 0x8b, 8] {
        return Err(invalid("archive is not a gzip stream"));
    }
    if bytes[3] != 0 || bytes[4..8] != [0, 0, 0, 0] {
        return Err(invalid(
            "gzip header carries a name, timestamp, or extension",
        ));
    }
    Ok(())
}

fn validate_checksum(header: &[u8]) -> io::Result<()> {
    let expected = octal(&header[148..156], "checksum")?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    if actual != expected {
        return Err(invalid("invalid ustar header checksum"));
    }
    Ok(())
}

fn field(bytes: &[u8], label: &str) -> io::Result<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if bytes[end..].iter().any(|byte| *byte != 0) {
        return Err(invalid(format!("non-canonical ustar {label}")));
    }
    std::str::from_utf8(&bytes[..end])
        .map(str::to_owned)
        .map_err(|_| invalid(format!("non-UTF-8 ustar {label}")))
}

fn octal(bytes: &[u8], label: &str) -> io::Result<u64> {
    if bytes.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err(invalid(format!("base-256 ustar {label} is forbidden")));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid(format!("invalid {label}")))?;
    let text = text.trim_matches(['\0', ' ']);
    if text.is_empty() || !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(invalid(format!("non-octal ustar {label}")));
    }
    let value =
        u64::from_str_radix(text, 8).map_err(|_| invalid(format!("overflowing ustar {label}")))?;
    let canonical = if label == "checksum" {
        format!("{value:06o}\0 ")
    } else {
        format!("{:0width$o}\0", value, width = bytes.len() - 1)
    };
    if bytes != canonical.as_bytes() {
        return Err(invalid(format!("non-canonical ustar {label} encoding")));
    }
    Ok(value)
}

fn allowed_relative_paths() -> BTreeMap<String, u64> {
    let mut paths = BTreeMap::new();
    for name in CORE_BINARIES.into_iter().chain(RELEASE_HELPERS) {
        paths.insert(name.to_owned(), 0o755);
    }
    for name in ["install.shl", "verify-release.shl"] {
        paths.insert(name.to_owned(), 0o755);
    }
    for name in [
        "README.md",
        "Cargo.lock",
        "BUILDINFO.json",
        "LICENSE-APACHE",
        "LICENSE-MIT",
    ] {
        paths.insert(name.to_owned(), 0o644);
    }
    for name in CORE_BINARIES {
        paths.insert(format!("man/{name}.1"), 0o644);
    }
    paths
}

fn validate_inventory<'a>(
    root: &str,
    entries: &'a [Entry],
) -> io::Result<BTreeMap<String, &'a Entry>> {
    if entries.is_empty() {
        return Err(invalid("release archive is empty"));
    }
    let actual_order = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    let mut sorted_order = actual_order.clone();
    sorted_order.sort_unstable();
    if actual_order != sorted_order {
        return Err(invalid("release archive entries are not in lexical order"));
    }
    let allowed = allowed_relative_paths();
    let mut by_path = BTreeMap::new();
    for entry in entries {
        let relative = entry
            .path
            .strip_prefix(&format!("{root}/"))
            .ok_or_else(|| {
                invalid(format!(
                    "archive entry escapes exact release root: {}",
                    entry.path
                ))
            })?;
        if relative.is_empty()
            || relative.starts_with('/')
            || relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || relative.split('/').any(|part| part.starts_with("._"))
        {
            return Err(invalid(format!("unsafe archive path: {}", entry.path)));
        }
        let expected_mode = allowed
            .get(relative)
            .ok_or_else(|| invalid(format!("unexpected release archive entry: {relative}")))?;
        if entry.mode != *expected_mode {
            return Err(invalid(format!(
                "non-portable mode for {relative}: {:04o} != {expected_mode:04o}",
                entry.mode
            )));
        }
        if entry.uid != 0 || entry.gid != 0 {
            return Err(invalid(format!(
                "non-root normalized ownership for {relative}"
            )));
        }
        if by_path.insert(entry.path.clone(), entry).is_some() {
            return Err(invalid(format!("duplicate archive path: {}", entry.path)));
        }
    }
    if by_path.len() != allowed.len() {
        let missing = allowed
            .keys()
            .filter(|relative| !by_path.contains_key(&format!("{root}/{relative}")))
            .cloned()
            .collect::<Vec<_>>();
        return Err(invalid(format!(
            "release archive inventory is incomplete: {missing:?}"
        )));
    }
    Ok(by_path)
}

fn validate_buildinfo(
    root: &str,
    info: &BuildInfo,
    entries: &[Entry],
    by_path: &BTreeMap<String, &Entry>,
) -> io::Result<()> {
    if info.schema != 1 || info.name != "shoal" || info.tag != format!("v{}", info.version) {
        return Err(invalid("BUILDINFO release identity is inconsistent"));
    }
    if root != format!("shoal-{}-{}", info.tag, info.target) || info.host != info.target {
        return Err(invalid(
            "BUILDINFO root/host/target identity is inconsistent",
        ));
    }
    if info.commit.len() != 40
        || !info
            .commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid(
            "BUILDINFO commit is not a lowercase 40-digit object ID",
        ));
    }
    let epoch = info
        .source_date_epoch
        .parse::<u64>()
        .map_err(|_| invalid("BUILDINFO source epoch is invalid"))?;
    if entries.iter().any(|entry| entry.mtime != epoch) {
        return Err(invalid(
            "archive entry mtime disagrees with BUILDINFO source epoch",
        ));
    }
    let expected_owner_name = if info.target.ends_with("-apple-darwin") {
        "root"
    } else {
        ""
    };
    if entries
        .iter()
        .any(|entry| entry.uname != expected_owner_name || entry.gname != expected_owner_name)
    {
        return Err(invalid(
            "archive owner/group names are not the normalized target profile",
        ));
    }
    let claims = &info.archive;
    if claims.format != "ustar"
        || claims.owner != 0
        || claims.group != 0
        || claims.order != "lexical"
        || claims.timestamps != "source_date_epoch"
        || claims.gzip_name_and_time
        || claims.apple_double
    {
        return Err(invalid(
            "BUILDINFO archive claims are not the enforced normalized profile",
        ));
    }
    let expected = CORE_BINARIES
        .into_iter()
        .chain(RELEASE_HELPERS)
        .collect::<BTreeSet<_>>();
    let actual = info
        .binaries
        .iter()
        .map(|binary| binary.name.as_str())
        .collect::<BTreeSet<_>>();
    if actual != expected || actual.len() != info.binaries.len() {
        return Err(invalid(
            "BUILDINFO executable inventory is not exact and unique",
        ));
    }
    for binary in &info.binaries {
        let entry = by_path
            .get(&format!("{root}/{}", binary.name))
            .ok_or_else(|| invalid("BUILDINFO executable is absent"))?;
        let digest = format!("{:x}", Sha256::digest(&entry.data));
        if entry.data.len() as u64 != binary.bytes || digest != binary.sha256 {
            return Err(invalid(format!(
                "BUILDINFO bytes/digest lie for {}",
                binary.name
            )));
        }
    }
    Ok(())
}

fn validate_sbom(
    path: &Path,
    root: &str,
    version: &str,
    archive_entries: &BTreeMap<String, &Entry>,
    cargo_lock: &[u8],
) -> io::Result<()> {
    let sbom = read_file_bounded(path, MAX_SBOM_BYTES, "release SBOM")?;
    let document: Value = serde_json::from_slice(&sbom)
        .map_err(|error| invalid(format!("invalid SPDX JSON: {error}")))?;
    let object = document
        .as_object()
        .ok_or_else(|| invalid("SPDX document is not an object"))?;
    require_string(object, "spdxVersion", "SPDX-2.3")?;
    require_string(object, "dataLicense", "CC0-1.0")?;
    require_string(object, "SPDXID", "SPDXRef-DOCUMENT")?;
    let name = string(object.get("name"), "SPDX name")?;
    if name != root {
        return Err(invalid("SPDX document name does not match release root"));
    }
    let namespace = string(object.get("documentNamespace"), "SPDX namespace")?;
    if !(namespace.starts_with("https://") || namespace.starts_with("urn:"))
        || !namespace.contains(root)
    {
        return Err(invalid("SPDX namespace is not release-specific"));
    }
    let creation = object
        .get("creationInfo")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("SPDX creationInfo is missing"))?;
    let created = string(creation.get("created"), "SPDX creation timestamp")?;
    if !valid_utc_timestamp(created) {
        return Err(invalid("SPDX creation timestamp is not normalized UTC"));
    }
    let creators = creation
        .get("creators")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("SPDX creators are missing"))?;
    if creators.is_empty()
        || creators
            .iter()
            .any(|creator| creator.as_str().is_none_or(str::is_empty))
    {
        return Err(invalid("SPDX creators are missing"));
    }
    let mut element_ids = BTreeSet::from(["SPDXRef-DOCUMENT".to_owned()]);
    if let Some(files) = object.get("files").and_then(Value::as_array) {
        for file in files {
            let file = file
                .as_object()
                .ok_or_else(|| invalid("SPDX file is not an object"))?;
            let id = string(file.get("SPDXID"), "file SPDXID")?;
            if !element_ids.insert(id.to_owned()) {
                return Err(invalid(format!("duplicate SPDX element ID: {id}")));
            }
            let name = string(file.get("fileName"), "SPDX fileName")?;
            let relative = name
                .strip_prefix("./")
                .or_else(|| name.strip_prefix('/'))
                .unwrap_or(name);
            let relative = relative
                .strip_prefix(&format!("{root}/"))
                .unwrap_or(relative);
            let Some(entry) = archive_entries.get(&format!("{root}/{relative}")) else {
                continue;
            };
            let Some(checksums) = file.get("checksums").and_then(Value::as_array) else {
                continue;
            };
            for checksum in checksums {
                let checksum = checksum
                    .as_object()
                    .ok_or_else(|| invalid("SPDX file checksum is not an object"))?;
                if string(checksum.get("algorithm"), "SPDX checksum algorithm")? == "SHA256" {
                    let expected = string(checksum.get("checksumValue"), "SPDX SHA256")?;
                    let actual = format!("{:x}", Sha256::digest(&entry.data));
                    if expected != actual {
                        return Err(invalid(format!(
                            "SPDX SHA256 disagrees with packaged bytes: {relative}"
                        )));
                    }
                }
            }
        }
    }
    let packages = object
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("SPDX packages are missing"))?;
    let mut package_by_id = BTreeMap::new();
    let mut identities = BTreeSet::new();
    for package in packages {
        let package = package
            .as_object()
            .ok_or_else(|| invalid("SPDX package is not an object"))?;
        let id = string(package.get("SPDXID"), "package SPDXID")?;
        let package_name = string(package.get("name"), "package name")?;
        let package_version = string(package.get("versionInfo"), "package versionInfo")?;
        if package_by_id
            .insert(
                id.to_owned(),
                (package_name.to_owned(), package_version.to_owned()),
            )
            .is_some()
        {
            return Err(invalid("duplicate SPDX package ID"));
        }
        if !element_ids.insert(id.to_owned()) {
            return Err(invalid(format!("duplicate SPDX element ID: {id}")));
        }
        if !identities.insert((package_name.to_owned(), package_version.to_owned())) {
            return Err(invalid(format!(
                "duplicate SPDX package identity: {package_name} {package_version}"
            )));
        }
    }
    let root_ids = package_by_id
        .iter()
        .filter_map(|(id, identity)| {
            (identity.0 == root && identity.1 == version).then_some(id.clone())
        })
        .collect::<Vec<_>>();
    if root_ids.len() != 1 {
        return Err(invalid(
            "SPDX must contain exactly one release-root package at the workspace version",
        ));
    }
    let lock: toml::Value = toml::from_slice(cargo_lock)
        .map_err(|error| invalid(format!("invalid packaged Cargo.lock: {error}")))?;
    let locked = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| invalid("Cargo.lock package list is missing"))?;
    // SPDX package identities do not preserve Cargo source identity uniformly. Collapse exact
    // name+version duplicates from multiple Cargo sources, but require every distinct pair.
    let locked_identities = locked
        .iter()
        .map(|package| {
            let table = package
                .as_table()
                .ok_or_else(|| invalid("Cargo.lock package is not a table"))?;
            Ok((
                toml_string(table.get("name"), "Cargo.lock name")?.to_owned(),
                toml_string(table.get("version"), "Cargo.lock version")?.to_owned(),
            ))
        })
        .collect::<io::Result<BTreeSet<_>>>()?;
    let allowed = locked_identities
        .iter()
        .cloned()
        .chain([(root.to_owned(), version.to_owned())])
        .collect::<BTreeSet<_>>();
    let unexpected = identities.difference(&allowed).cloned().collect::<Vec<_>>();
    let missing = locked_identities
        .difference(&identities)
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() || !missing.is_empty() {
        return Err(invalid(format!(
            "SPDX/Cargo.lock package coverage mismatch; missing={missing:?}, unexpected={unexpected:?}"
        )));
    }
    let relationships = object
        .get("relationships")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("SPDX relationships are missing"))?;
    let mut described = BTreeSet::new();
    let mut dependency_edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for relationship in relationships {
        let row = relationship
            .as_object()
            .ok_or_else(|| invalid("SPDX relationship is not an object"))?;
        let from = string(row.get("spdxElementId"), "relationship source")?;
        let kind = string(row.get("relationshipType"), "relationship type")?;
        let to = string(row.get("relatedSpdxElement"), "relationship target")?;
        if !element_ids.contains(from) || !element_ids.contains(to) {
            return Err(invalid(format!(
                "SPDX relationship has an unknown endpoint: {from} {kind} {to}"
            )));
        }
        if from == to {
            return Err(invalid(format!(
                "SPDX relationship is self-referential: {from} {kind}"
            )));
        }
        if kind == "DESCRIBES" && from == "SPDXRef-DOCUMENT" {
            described.insert(to.to_owned());
        }
        let directed = match kind {
            "DEPENDS_ON" | "CONTAINS" => Some((from, to)),
            "DEPENDENCY_OF" | "CONTAINED_BY" => Some((to, from)),
            _ => None,
        };
        if let Some((owner, dependency)) = directed
            && package_by_id.contains_key(owner)
            && package_by_id.contains_key(dependency)
        {
            dependency_edges
                .entry(owner.to_owned())
                .or_default()
                .insert(dependency.to_owned());
        }
    }
    if !described.contains(&root_ids[0]) {
        return Err(invalid(
            "SPDX document does not DESCRIBE the release-root package",
        ));
    }
    let mut reachable = BTreeSet::from([root_ids[0].clone()]);
    let mut pending = vec![root_ids[0].clone()];
    while let Some(id) = pending.pop() {
        if let Some(dependencies) = dependency_edges.get(&id) {
            for dependency in dependencies {
                if reachable.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
        }
    }
    for (id, identity) in &package_by_id {
        if locked_identities.contains(identity) && !reachable.contains(id) {
            return Err(invalid(format!(
                "locked package is not reachable from the release root through dependency relationships: {} {}",
                identity.0, identity.1
            )));
        }
    }
    Ok(())
}

fn require_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    expected: &str,
) -> io::Result<()> {
    if string(object.get(key), key)? != expected {
        return Err(invalid(format!("SPDX {key} is not {expected}")));
    }
    Ok(())
}

fn string<'a>(value: Option<&'a Value>, label: &str) -> io::Result<&'a str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("{label} is missing")))
}

fn toml_string<'a>(value: Option<&'a toml::Value>, label: &str) -> io::Result<&'a str> {
    value
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("{label} is missing")))
}

fn valid_utc_timestamp(value: &str) -> bool {
    let Some(value) = value.strip_suffix('Z') else {
        return false;
    };
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    let date = date.split('-').collect::<Vec<_>>();
    let mut time = time.split(':');
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(seconds), None) = (
        date.first().and_then(|part| part.parse::<u32>().ok()),
        date.get(1).and_then(|part| part.parse::<u32>().ok()),
        date.get(2).and_then(|part| part.parse::<u32>().ok()),
        time.next().and_then(|part| part.parse::<u32>().ok()),
        time.next().and_then(|part| part.parse::<u32>().ok()),
        time.next(),
        time.next(),
    ) else {
        return false;
    };
    if date.len() != 3 || date[0].len() != 4 || date[1].len() != 2 || date[2].len() != 2 {
        return false;
    }
    let (second, fraction) = seconds
        .split_once('.')
        .map_or((seconds, None), |parts| (parts.0, Some(parts.1)));
    if second.len() != 2
        || second.parse::<u32>().ok().is_none_or(|second| second > 59)
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
    {
        return false;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=maximum_day).contains(&day)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
#[path = "release/tests.rs"]
mod tests;
