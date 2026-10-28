use std::collections::{BTreeSet, HashMap, VecDeque};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use apple_bundle::prelude::{InfoPlist as AppleInfoPlist, *};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
struct CargoToml {
    package: CargoTomlPackage,
}

#[derive(Debug, Serialize, Deserialize)]
struct CargoTomlPackage {
    metadata: CargoTomlPackageMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
struct CargoTomlPackageMetadata {
    bundle: BundleConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleConfig {
    #[serde(rename(serialize = "CFBundleIdentifier"))]
    bundle_identifier: String,
    #[serde(rename(serialize = "CFBundleName"))]
    bundle_name: String,
    // #[serde(rename(serialize = "CFBundleShortVersionString"))]
    // version: String,
    #[serde(flatten)]
    input_method: InputMethod,
}

#[derive(Debug, Serialize, Deserialize)]
struct InfoPlist {
    #[serde(flatten)]
    apple: AppleInfoPlist,
    #[serde(flatten)]
    input_method: InputMethod,
    #[serde(flatten)]
    fastab: FastabKeys,
}

/// Build facts the helper reads back at runtime instead of compiling them in.
/// Any per-build string in the executable changes it on every release, and the
/// IME must only be replaced when its code changed (see [`code_digest`]).
#[derive(Debug, Default, Serialize, Deserialize)]
struct FastabKeys {
    #[serde(rename = "FastabCodeDigest")]
    code_digest: String,
    #[serde(rename = "FastabBuildCommit", skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(rename = "FastabBuildDate", skip_serializing_if = "Option::is_none")]
    built_at: Option<String>,
    #[serde(rename = "FastabBuildTarget", skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(rename = "FastabBuildRunId", skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InputMethod {
    #[serde(
        rename(serialize = "InputMethodConnectionName"),
        serialize_with = "serialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    input_method_connection_name: Option<String>,
    #[serde(
        rename(serialize = "InputMethodServerControllerClass"),
        default = "input_method_server_controller_class_default"
    )]
    input_method_server_controller_class: String,
    #[serde(rename(serialize = "TISInputSourceID"))]
    input_source_identifier: String,
    #[serde(rename(serialize = "InputMethodType"))]
    input_method_type: String,
    #[serde(rename(serialize = "ComponentInvisibleInSystemUI"))]
    invisible_in_system_ui: bool,
    #[serde(
        rename(serialize = "tsInputMethodIconFileKey"),
        default = "input_method_file_icon_key_default"
    )]
    input_method_file_icon_key: String,
    #[serde(rename(serialize = "TISIntendedLanguage"), default = "intended_language_default")]
    intended_language: String,
}

fn serialize_option<S, T>(value: &Option<T>, ser: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize,
{
    value
        .as_ref()
        .expect(r#"`serialize_option` must be used with `skip_serializing_if = "Option::is_none"`"#)
        .serialize(ser)
}

fn build_env(key: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={key}");
    env::var(key).ok().filter(|value| !value.is_empty())
}

fn input_method_server_controller_class_default() -> String {
    "RustInputMethodServerController".to_string()
}

fn input_method_file_icon_key_default() -> String {
    "AppIcon".to_string()
}

fn intended_language_default() -> String {
    "en".to_string()
}

/// Digest of what decides this helper's behaviour: its sources, the files it
/// includes by `#[path]`, `fastab_hitoolbox`, the locked external crates it
/// links, their feature selections, and the compiler and profile.
///
/// It is computed from inputs rather than from the built executable on
/// purpose. Every release bumps the workspace version, which changes Cargo's
/// `-C metadata` for every crate and with it symbol hashes and the emission
/// order of generic code, so identical sources do not produce identical bytes
/// across releases. Signing then binds the Info.plist, whose version changes
/// too. Installs compare this digest, so an update whose IME code did not change
/// leaves the running helper, and every terminal window bound to it, alone.
fn code_digest(manifest_dir: &Path) -> String {
    let workspace = manifest_dir.join("../..");
    let hitoolbox = workspace.join("crates/fastab_hitoolbox");
    let mut hasher = Sha256::new();

    let mut files = BTreeSet::new();
    collect_files(&manifest_dir.join("src"), &mut files);
    files.insert(manifest_dir.join("build.rs"));
    files.insert(manifest_dir.join("Cargo.toml"));
    for source in files.clone() {
        files.extend(path_includes(&source));
    }
    collect_files(&hitoolbox.join("src"), &mut files);
    files.insert(hitoolbox.join("Cargo.toml"));
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed={}", hitoolbox.display());
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let relative = file.strip_prefix(&workspace).unwrap_or(file);
        hash_field(&mut hasher, "file", &relative.to_string_lossy());
        hash_field(&mut hasher, "contents", &fs::read_to_string(file).unwrap_or_default());
    }

    let lock_path = workspace.join("Cargo.lock");
    let root_manifest_path = workspace.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    println!("cargo:rerun-if-changed={}", root_manifest_path.display());
    let lock: toml::Value = toml::from_str(&fs::read_to_string(&lock_path).unwrap()).unwrap();
    let root_manifest: toml::Value = toml::from_str(&fs::read_to_string(&root_manifest_path).unwrap()).unwrap();
    let own_manifest: toml::Value =
        toml::from_str(&fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap()).unwrap();
    let hitoolbox_manifest: toml::Value =
        toml::from_str(&fs::read_to_string(hitoolbox.join("Cargo.toml")).unwrap()).unwrap();

    // Runtime dependencies only: build-dependencies shape Info.plist, and the
    // dev-dependencies exist for the equivalence tests.
    let mut direct: Vec<String> = runtime_dependency_names(&own_manifest);
    direct.extend(runtime_dependency_names(&hitoolbox_manifest));
    direct.sort();
    direct.dedup();
    let workspace_dependencies = root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"));
    for name in &direct {
        if let Some(spec) = workspace_dependencies.and_then(|dependencies| dependencies.get(name)) {
            hash_field(&mut hasher, "workspace-dependency", &format!("{name} = {spec}"));
        }
    }
    for package in locked_closure(&lock, env!("CARGO_PKG_NAME"), &direct) {
        hash_field(&mut hasher, "locked", &package);
    }

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let version = Command::new(rustc).arg("-vV").output().map_or_else(
        |_| String::new(),
        |output| String::from_utf8_lossy(&output.stdout).into_owned(),
    );
    hash_field(&mut hasher, "rustc", &version);
    for key in ["TARGET", "PROFILE", "OPT_LEVEL", "DEBUG", "CARGO_ENCODED_RUSTFLAGS"] {
        hash_field(&mut hasher, key, &env::var(key).unwrap_or_default());
    }

    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

fn hash_field(hasher: &mut Sha256, label: &str, value: &str) {
    // Length-prefixed, so adjacent fields cannot run into each other.
    hasher.update(label.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn collect_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.insert(path);
        }
    }
}

/// Files a Rust source pulls in with `#[path = "..."]`, resolved like rustc
/// does for an inline module: relative to the including file's directory.
fn path_includes(source: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(source) else {
        return Vec::new();
    };
    let dir = source.parent().unwrap_or(Path::new("."));
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("#[path = \""))
        .filter_map(|rest| rest.split('"').next())
        .map(|relative| dir.join(relative))
        .collect()
}

fn runtime_dependency_names(manifest: &toml::Value) -> Vec<String> {
    let mut names = Vec::new();
    let mut tables = vec![manifest.get("dependencies")];
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        tables.extend(targets.values().map(|target| target.get("dependencies")));
    }
    for table in tables.into_iter().flatten().filter_map(toml::Value::as_table) {
        names.extend(table.keys().cloned());
    }
    names
}

/// Every package reachable from `roots` through the lockfile, as `name` for
/// workspace members (their sources are hashed above, and their version moves
/// with every release) or `name version checksum` for locked crates.
fn locked_closure(lock: &toml::Value, own: &str, roots: &[String]) -> BTreeSet<String> {
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut by_name: HashMap<&str, Vec<&toml::Value>> = HashMap::new();
    for package in packages {
        if let Some(name) = package.get("name").and_then(toml::Value::as_str) {
            by_name.entry(name).or_default().push(package);
        }
    }
    let field = |package: &toml::Value, key: &str| package.get(key).and_then(toml::Value::as_str).map(str::to_owned);
    // The result borrows from `by_name`, not from `reference`; leave it inferred.
    let resolve = |reference: &str| {
        let mut parts = reference.split(' ');
        let name = parts.next()?;
        let version = parts.next();
        by_name
            .get(name)?
            .iter()
            .copied()
            .find(|package| version.is_none_or(|version| field(package, "version").as_deref() == Some(version)))
    };
    let dependencies_of = |package: &toml::Value| -> Vec<String> {
        package
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .map_or_else(Vec::new, |items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
    };

    let mut queue: VecDeque<&toml::Value> = VecDeque::new();
    if let Some(package) = resolve(own) {
        // This package's lock entry also lists build- and dev-dependencies;
        // start from the runtime names only.
        for reference in dependencies_of(package) {
            let name = reference.split(' ').next().unwrap_or_default();
            if roots.iter().any(|root| root == name) {
                queue.extend(resolve(&reference));
            }
        }
    }
    for root in roots {
        let missing = queue
            .iter()
            .all(|package| field(package, "name").as_deref() != Some(root.as_str()));
        if missing && by_name.contains_key(root.as_str()) {
            queue.extend(resolve(root));
        }
    }

    let mut seen = BTreeSet::new();
    let mut out = BTreeSet::new();
    while let Some(package) = queue.pop_front() {
        let name = field(package, "name").unwrap_or_default();
        let version = field(package, "version").unwrap_or_default();
        if !seen.insert(format!("{name} {version}")) {
            continue;
        }
        match field(package, "checksum") {
            Some(checksum) => out.insert(format!("{name} {version} {checksum}")),
            None if package.get("source").is_some() => out.insert(format!("{name} {version}")),
            None => out.insert(name.clone()),
        };
        for reference in dependencies_of(package) {
            queue.extend(resolve(&reference));
        }
    }
    out
}

fn main() {
    // println!("cargo:warning=Running build.rs");
    // Tell Cargo that if the given file changes, to rerun this build script.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    // The build_identity leaf reads version, commit and dates from Info.plist
    // rather than compiling them in; see `FastabKeys`.
    println!("cargo:rustc-cfg=build_identity_from_bundle");

    // Read Cargo.toml and load [bundle]
    let config = fs::read_to_string("Cargo.toml").unwrap();

    let manifest = toml::from_str::<CargoToml>(&config).unwrap();
    let mut bundle = manifest.package.metadata.bundle;

    // Set InputMethodServerControllerClass env var which is used by Input Method at compile time
    println!(
        "cargo:rustc-env=InputMethodServerControllerClass={}",
        bundle.input_method.input_method_server_controller_class
    );

    // Derieve InputMethodConnectionName from bundle identifier
    let connection_name = match bundle.input_method.input_method_connection_name {
        Some(name) => name,
        None => format!("{}_Connection", bundle.bundle_identifier),
    };

    bundle.input_method.input_method_connection_name = Some(connection_name.clone());

    println!("cargo:rustc-env=InputMethodConnectionName={connection_name}");

    let properties = InfoPlist {
        apple: AppleInfoPlist {
            localization: Localization {
                bundle_development_region: Some("en".to_owned()),
                ..Default::default()
            },
            launch: Launch {
                bundle_executable: Some(env!("CARGO_PKG_NAME").to_owned()),
                ..Default::default()
            },
            identification: Identification {
                bundle_identifier: bundle.bundle_identifier,
                ..Default::default()
            },
            bundle_version: BundleVersion {
                bundle_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                bundle_info_dictionary_version: Some("6.0".to_string()),
                bundle_short_version_string: Some(env!("CARGO_PKG_VERSION").to_string()),
                human_readable_copyright: Some("Copyright © 2026 Fastab contributors".to_owned()),
            },
            naming: Naming {
                bundle_name: Some(bundle.bundle_name),
                ..Default::default()
            },
            categorization: Categorization {
                bundle_package_type: Some("APPL".to_owned()),
                ..Default::default()
            },
            background_execution: BackgroundExecution {
                background_only: Some(true),
                ..Default::default()
            },
            termination: Termination {
                supports_sudden_termination: Some(true),
                ..Default::default()
            },
            operating_system_version: OperatingSystemVersion {
                minimum_system_version: Some("10.13".to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        input_method: bundle.input_method,
        fastab: FastabKeys {
            code_digest: code_digest(&PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())),
            commit: build_env("AMAZON_Q_BUILD_HASH"),
            built_at: build_env("AMAZON_Q_BUILD_DATETIME"),
            target: build_env("AMAZON_Q_BUILD_TARGET_TRIPLE"),
            run_id: build_env("FASTAB_BUILD_RUN_ID"),
        },
    };

    // println!("cargo:warning=Regenerating Info.plist");

    // Create Info.plist file
    let file = std::fs::File::create("Info.plist").unwrap();
    plist::to_writer_xml(file, &properties).unwrap();
}
