//! The `peko.toml` project manifest: typed model, parsing, discovery, and the
//! format-preserving edit used by `peko link`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};
use serde::Deserialize;
use toml_edit::DocumentMut;

use super::container::{Compression, encode_container};
use super::{ConfigError, MANIFEST_FILE, SOURCE_DIR, operating_system_from_str};
use crate::target::{Architecture, OperatingSystem};
use crate::{ExternalModuleInfo, ExternalModuleVersion};

/// How far up the directory tree discovery searches for a manifest.
const DISCOVERY_DEPTH: usize = 16;

/// The entry file name candidates for an application, in preference order.
const APP_ENTRY_CANDIDATES: [&str; 2] = ["main.peko", "app.peko"];

/// The default entry file name for a package.
const PACKAGE_ENTRY: &str = "lib.peko";

// ---------------------------------------------------------------------------
// Typed model
// ---------------------------------------------------------------------------

/// A parsed project manifest.
///
/// The application and package forms are mutually exclusive. A manifest is an
/// application when it carries a `[project]` table and a package when it
/// carries a `[package]` table.
#[derive(Debug, Clone)]
pub enum Manifest {
    /// An application: `[project]`, with `[ui]` for the UI form.
    Application(ApplicationManifest),
    /// A publishable package: `[package]` and `[lib]`.
    Package(PackageManifest),
}

/// The coarse kind of a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    /// An application that drives a user interface.
    UiApplication,
    /// An application that runs as a command-line program.
    CliApplication,
    /// A publishable package.
    Package,
}

/// An application manifest built from `[project]` and an optional `[ui]`.
#[derive(Debug, Clone)]
pub struct ApplicationManifest {
    /// The `[project]` table.
    pub project: Project,
    /// The `[ui]` table. `None` marks a command-line application.
    pub ui: Option<Ui>,
    /// The `[dependencies]` table.
    pub dependencies: BTreeMap<String, Dependency>,
    /// The `[platforms]` table.
    pub platforms: Platforms,
    /// The `[native]` table.
    pub native: Option<Native>,
}

/// A package manifest built from `[package]` and `[lib]`.
#[derive(Debug, Clone)]
pub struct PackageManifest {
    /// The `[package]` table.
    pub package: PackageMeta,
    /// The `[lib]` table.
    pub lib: Lib,
    /// The `[dependencies]` table.
    pub dependencies: BTreeMap<String, Dependency>,
    /// The `[platforms]` table.
    pub platforms: Platforms,
    /// The `[native]` table.
    pub native: Option<Native>,
}

/// The `[project]` table of an application manifest.
#[derive(Debug, Clone)]
pub struct Project {
    /// The display name of the application.
    pub name: String,
    /// The bundle identifier in reverse-DNS form.
    pub bundle: Option<String>,
    /// The application version.
    pub version: Version,
    /// The platform-assigned app id written by `peko link`.
    pub app_id: Option<String>,
    /// The operating systems this application builds for.
    pub target_platforms: Vec<OperatingSystem>,
}

/// The `[ui]` table of a UI application.
#[derive(Debug, Clone)]
pub struct Ui {
    /// The UI build path.
    pub framework: Framework,
    /// The path to a square PNG app icon, relative to the project root.
    pub icon: Option<PathBuf>,
}

/// The UI build path an application uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    /// The native PekoUI framework.
    Native,
    /// A third-party static site generator served through the webview.
    Static,
    /// A third-party server framework run by the platform.
    Server,
}

impl Framework {
    /// Map a framework identifier to a framework.
    ///
    /// The accepted identifiers are `native`, `static`, and `server`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(identifier: &str) -> Option<Framework> {
        match identifier {
            "native" => Some(Framework::Native),
            "static" => Some(Framework::Static),
            "server" => Some(Framework::Server),
            _ => None,
        }
    }

    /// The canonical identifier for this framework.
    pub fn as_str(self) -> &'static str {
        match self {
            Framework::Native => "native",
            Framework::Static => "static",
            Framework::Server => "server",
        }
    }
}

/// The `[package]` table of a package manifest.
#[derive(Debug, Clone)]
pub struct PackageMeta {
    /// The package name on the registry.
    pub name: String,
    /// The package version.
    pub version: Version,
    /// A short description.
    pub description: Option<String>,
    /// The license identifier.
    pub license: Option<String>,
    /// The package authors.
    pub authors: Vec<String>,
    /// The source repository url.
    pub repository: Option<String>,
    /// Search keywords.
    pub keywords: Vec<String>,
    /// Registry categories.
    pub categories: Vec<String>,
    /// The minimum compiler version the package requires.
    pub peko: Option<VersionReq>,
}

/// The `[lib]` table of a package manifest.
#[derive(Debug, Clone)]
pub struct Lib {
    /// The package root source file, relative to the project root.
    pub root: PathBuf,
}

/// One entry of the `[dependencies]` table.
#[derive(Debug, Clone)]
pub enum Dependency {
    /// A dependency resolved through the registry by version requirement.
    Registry {
        /// The accepted version range.
        version: VersionReq,
    },
    /// A dependency resolved from a local path for in-tree development.
    Path {
        /// The dependency directory, relative to the project root.
        path: PathBuf,
    },
}

/// A dependency value written into `[dependencies]` by an edit.
#[derive(Debug, Clone)]
pub enum DependencySpec {
    /// A registry version requirement, for example `^1.2`.
    Version(String),
    /// A local path dependency directory.
    Path(String),
}

/// The `[platforms]` table.
#[derive(Debug, Clone, Default)]
pub struct Platforms {
    /// The operating systems the project supports.
    pub supported: Vec<OperatingSystem>,
}

/// The `[native]` table describing the C build.
#[derive(Debug, Clone, Default)]
pub struct Native {
    /// The C and Objective-C source files to compile.
    pub sources: Vec<PathBuf>,
    /// The include directories.
    pub include: Vec<PathBuf>,
    /// The compile flags.
    pub flags: NativeFlags,
    /// The link arguments.
    pub link: NativeLink,
    /// Prebuilt static libraries to link, keyed by platform.
    pub libs: NativeLibs,
    /// The vendored C libraries.
    pub vendor: Vec<Vendor>,
}

/// Prebuilt static libraries from `[native.libs]`, keyed by a platform string
/// that is either an operating system name (`macos`) or an operating system
/// and architecture (`macos-arm`). The paths are relative to the package root.
#[derive(Debug, Clone, Default)]
pub struct NativeLibs {
    /// One entry per platform key, in declaration order.
    pub per_platform: Vec<(String, Vec<PathBuf>)>,
}

impl NativeLibs {
    /// Every library archive that applies to `os`/`arch`: entries keyed `all`,
    /// the operating system name, or the `os-arch` pair.
    pub fn for_target(&self, os: OperatingSystem, arch: Architecture) -> Vec<&PathBuf> {
        let os_key = os.name();
        let os_arch_key = format!("{}-{}", os.name(), arch.name());
        let mut result = Vec::new();
        for (key, paths) in &self.per_platform {
            if key == "all" || key == os_key || key == &os_arch_key {
                result.extend(paths.iter());
            }
        }
        result
    }
}

/// Compile flags from `[native.flags]`.
#[derive(Debug, Clone, Default)]
pub struct NativeFlags {
    /// Flags applied on every platform.
    pub all: Vec<String>,
    /// Flags applied on a single operating system, one entry per platform key.
    pub per_os: Vec<(OperatingSystem, Vec<String>)>,
}

impl NativeFlags {
    /// The flags set for the given operating system, or an empty slice when
    /// none are set.
    pub fn for_os(&self, os: OperatingSystem) -> &[String] {
        self.per_os
            .iter()
            .find(|(entry, _)| *entry == os)
            .map(|(_, values)| values.as_slice())
            .unwrap_or(&[])
    }
}

/// Link arguments from `[native.link]`.
#[derive(Debug, Clone, Default)]
pub struct NativeLink {
    /// Arguments applied on every platform.
    pub all: Vec<String>,
    /// Arguments applied on a single operating system, one entry per platform
    /// key.
    pub per_os: Vec<(OperatingSystem, Vec<String>)>,
}

impl NativeLink {
    /// The link arguments set for the given operating system, or an empty
    /// slice when none are set.
    pub fn for_os(&self, os: OperatingSystem) -> &[String] {
        self.per_os
            .iter()
            .find(|(entry, _)| *entry == os)
            .map(|(_, values)| values.as_slice())
            .unwrap_or(&[])
    }
}

/// One vendored C library from `[[native.vendor]]`.
#[derive(Debug, Clone)]
pub struct Vendor {
    /// The vendor name.
    pub name: String,
    /// The vendor source directory, relative to the project root.
    pub path: PathBuf,
    /// Flags applied when compiling the vendored sources.
    pub flags: Vec<String>,
}

/// A manifest together with the project root it was loaded from.
#[derive(Debug, Clone)]
pub struct LoadedManifest {
    /// The project root directory that contains `peko.toml`.
    pub root: PathBuf,
    /// The parsed manifest.
    pub manifest: Manifest,
}

// ---------------------------------------------------------------------------
// Typed accessors
// ---------------------------------------------------------------------------

impl Manifest {
    /// The coarse kind of this manifest.
    pub fn kind(&self) -> ManifestKind {
        match self {
            Manifest::Application(app) => match app.ui {
                Some(_) => ManifestKind::UiApplication,
                None => ManifestKind::CliApplication,
            },
            Manifest::Package(_) => ManifestKind::Package,
        }
    }

    /// The project name, drawn from `[project].name` or `[package].name`.
    pub fn name(&self) -> &str {
        match self {
            Manifest::Application(app) => &app.project.name,
            Manifest::Package(pkg) => &pkg.package.name,
        }
    }

    /// The `[native]` table, drawn from either manifest form. `None` when the
    /// manifest declares no native build.
    pub fn native(&self) -> Option<&Native> {
        match self {
            Manifest::Application(app) => app.native.as_ref(),
            Manifest::Package(pkg) => pkg.native.as_ref(),
        }
    }

    /// The project version, drawn from `[project].version` or
    /// `[package].version`.
    pub fn version(&self) -> &Version {
        match self {
            Manifest::Application(app) => &app.project.version,
            Manifest::Package(pkg) => &pkg.package.version,
        }
    }

    /// The registry description, drawn from `[package].description`.
    ///
    /// An application manifest carries no description and yields an empty
    /// string.
    pub fn description(&self) -> &str {
        match self {
            Manifest::Package(pkg) => pkg.package.description.as_deref().unwrap_or(""),
            Manifest::Application(_) => "",
        }
    }

    /// Build an [`ExternalModuleInfo`] view of this manifest rooted at `root`.
    ///
    /// The module name, version, and description come from the manifest. The
    /// source root and entry file are derived from [`Manifest::entry`].
    pub fn to_external_module(&self, root: &Path) -> ExternalModuleInfo {
        let entry = self.entry(root);
        let source_root = entry
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        let entry_file = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(APP_ENTRY_CANDIDATES[0])
            .to_owned();

        ExternalModuleInfo::new(
            self.name().to_owned(),
            self.description().to_owned(),
            vec![ExternalModuleVersion::new(
                self.version().to_string(),
                source_root,
                entry_file,
            )],
        )
    }

    /// The dependency table shared by both manifest forms.
    pub fn dependencies(&self) -> &BTreeMap<String, Dependency> {
        match self {
            Manifest::Application(app) => &app.dependencies,
            Manifest::Package(pkg) => &pkg.dependencies,
        }
    }

    /// The entry source file path under the given project root.
    ///
    /// A package entry comes from `[lib].root`. An application entry is the
    /// first existing candidate under the source directory, falling back to
    /// `source/main.peko` when none exist on disk.
    pub fn entry(&self, root: &Path) -> PathBuf {
        match self {
            Manifest::Package(pkg) => root.join(&pkg.lib.root),
            Manifest::Application(_) => {
                let source = root.join(SOURCE_DIR);
                for candidate in APP_ENTRY_CANDIDATES {
                    let path = source.join(candidate);
                    if path.is_file() {
                        return path;
                    }
                }
                source.join(APP_ENTRY_CANDIDATES[0])
            }
        }
    }
}

impl LoadedManifest {
    /// The entry source file path for this manifest.
    pub fn entry(&self) -> PathBuf {
        self.manifest.entry(&self.root)
    }

    /// An [`ExternalModuleInfo`] view of this manifest at its project root.
    pub fn to_external_module(&self) -> ExternalModuleInfo {
        self.manifest.to_external_module(&self.root)
    }

    /// Frame this project into a `.pkpkg` container.
    ///
    /// The verbatim `peko.toml` at the project root is embedded as the
    /// container metadata ahead of `payload` and an optional detached
    /// signature. The caller compresses `payload` and sets the matching
    /// [`Compression`] tag.
    pub fn to_container(
        &self,
        compression: Compression,
        payload: &[u8],
        signature: Option<&[u8]>,
    ) -> Result<Vec<u8>, ConfigError> {
        let path = self.root.join(MANIFEST_FILE);
        let manifest = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(encode_container(&manifest, compression, payload, signature))
    }
}

// ---------------------------------------------------------------------------
// Parsing, loading, discovery
// ---------------------------------------------------------------------------

impl Manifest {
    /// Parse a manifest from TOML text without touching the file system.
    ///
    /// The `source` path is used only to label errors.
    pub fn parse(text: &str, source: &Path) -> Result<Manifest, ConfigError> {
        let raw: RawManifest = toml::from_str(text).map_err(|err| ConfigError::Parse {
            path: source.to_path_buf(),
            source: err,
        })?;
        raw.validate(source)
    }

    /// Read and parse the manifest at the given path.
    ///
    /// The project root is the directory that contains the file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<LoadedManifest, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let manifest = Manifest::parse(&text, path)?;
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(LoadedManifest { root, manifest })
    }

    /// Find and load the manifest that owns `start` or one of its ancestors.
    ///
    /// The search walks upward until it finds a `peko.toml`, stopping after
    /// [`DISCOVERY_DEPTH`] directories.
    pub fn discover<P: AsRef<Path>>(start: P) -> Result<LoadedManifest, ConfigError> {
        let mut directory = start.as_ref().to_path_buf();
        for _ in 0..DISCOVERY_DEPTH {
            let candidate = directory.join(MANIFEST_FILE);
            if candidate.is_file() {
                return Manifest::load(candidate);
            }
            match directory.parent() {
                Some(parent) => directory = parent.to_path_buf(),
                None => break,
            }
        }
        Err(ConfigError::NotFound)
    }

    /// Write the platform-assigned app id into an application manifest.
    ///
    /// The file is parsed as a document, the `app_id` key under `[project]` is
    /// set, and the file is written back with its comments and formatting
    /// intact. A manifest without a `[project]` table is rejected, since the
    /// app id belongs to applications.
    pub fn write_app_id<P: AsRef<Path>>(path: P, app_id: &str) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut document = text
            .parse::<DocumentMut>()
            .map_err(|source| ConfigError::Edit {
                path: path.to_path_buf(),
                source,
            })?;

        let project = document
            .get_mut("project")
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| ConfigError::invalid(path, "no [project] table to write app_id into"))?;
        project["app_id"] = toml_edit::value(app_id);

        std::fs::write(path, document.to_string()).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Insert or replace a dependency in `[dependencies]`, preserving the
    /// file's formatting and comments.
    ///
    /// The `[dependencies]` table is created when absent.
    pub fn add_dependency<P: AsRef<Path>>(
        path: P,
        name: &str,
        spec: &DependencySpec,
    ) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let mut document = read_document(path)?;

        let dependencies = document
            .entry("dependencies")
            .or_insert_with(toml_edit::table)
            .as_table_mut()
            .ok_or_else(|| ConfigError::invalid(path, "[dependencies] is not a table"))?;

        match spec {
            DependencySpec::Version(version) => {
                dependencies[name] = toml_edit::value(version.clone());
            }
            DependencySpec::Path(dir) => {
                let mut entry = toml_edit::InlineTable::new();
                entry.insert("path", dir.clone().into());
                dependencies[name] = toml_edit::value(entry);
            }
        }

        write_document(path, &document)
    }

    /// Remove a dependency from `[dependencies]`, returning whether it was
    /// present. Formatting and comments are preserved.
    pub fn remove_dependency<P: AsRef<Path>>(path: P, name: &str) -> Result<bool, ConfigError> {
        let path = path.as_ref();
        let mut document = read_document(path)?;

        let removed = document
            .get_mut("dependencies")
            .and_then(|item| item.as_table_mut())
            .map(|dependencies| dependencies.remove(name).is_some())
            .unwrap_or(false);

        write_document(path, &document)?;
        Ok(removed)
    }
}

/// Read a manifest as an editable document.
fn read_document(path: &Path) -> Result<DocumentMut, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse::<DocumentMut>()
        .map_err(|source| ConfigError::Edit {
            path: path.to_path_buf(),
            source,
        })
}

/// Write an edited document back to disk.
fn write_document(path: &Path, document: &DocumentMut) -> Result<(), ConfigError> {
    std::fs::write(path, document.to_string()).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------
// Raw deserialization
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    project: Option<RawProject>,
    ui: Option<RawUi>,
    package: Option<RawPackage>,
    lib: Option<RawLib>,
    #[serde(default)]
    dependencies: BTreeMap<String, RawDependency>,
    platforms: Option<RawPlatforms>,
    native: Option<RawNative>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    name: String,
    bundle: Option<String>,
    version: String,
    app_id: Option<String>,
    #[serde(default)]
    target_platforms: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUi {
    framework: String,
    icon: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackage {
    name: String,
    version: String,
    description: Option<String>,
    license: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    repository: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
    peko: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLib {
    root: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlatforms {
    #[serde(default)]
    supported: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawDependency {
    Simple(String),
    Detailed(RawDependencyTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependencyTable {
    version: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawNative {
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    flags: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    link: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    libs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    vendor: Vec<RawVendor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVendor {
    name: String,
    path: String,
    #[serde(default)]
    flags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl RawManifest {
    /// Convert raw tables into a validated manifest, reporting the first
    /// inconsistency against `source`.
    fn validate(self, source: &Path) -> Result<Manifest, ConfigError> {
        let is_application = self.project.is_some();
        let is_package = self.package.is_some();

        if is_application && is_package {
            return Err(ConfigError::invalid(
                source,
                "a manifest may define [project] or [package], not both",
            ));
        }

        let dependencies = build_dependencies(self.dependencies, source)?;
        let platforms = build_platforms(self.platforms, source)?;
        let native = self
            .native
            .map(|raw| build_native(raw, source))
            .transpose()?;

        if is_application {
            let project = build_project(self.project.unwrap(), source)?;
            let ui = self.ui.map(|raw| build_ui(raw, source)).transpose()?;

            if self.lib.is_some() {
                return Err(ConfigError::invalid(
                    source,
                    "[lib] is not valid in an application manifest",
                ));
            }

            return Ok(Manifest::Application(ApplicationManifest {
                project,
                ui,
                dependencies,
                platforms,
                native,
            }));
        }

        if is_package {
            if self.ui.is_some() {
                return Err(ConfigError::invalid(
                    source,
                    "[ui] is not valid in a package manifest",
                ));
            }

            let package = build_package(self.package.unwrap(), source)?;
            let lib = Lib {
                root: self
                    .lib
                    .and_then(|raw| raw.root)
                    .map(PathBuf::from)
                    .unwrap_or_else(default_package_root),
            };

            return Ok(Manifest::Package(PackageManifest {
                package,
                lib,
                dependencies,
                platforms,
                native,
            }));
        }

        Err(ConfigError::invalid(
            source,
            "a manifest must define either [project] or [package]",
        ))
    }
}

/// The default package root source file `source/lib.peko`.
fn default_package_root() -> PathBuf {
    PathBuf::from(SOURCE_DIR).join(PACKAGE_ENTRY)
}

fn build_project(raw: RawProject, source: &Path) -> Result<Project, ConfigError> {
    let version = parse_version(&raw.version, "project.version", source)?;
    let target_platforms =
        parse_platforms(&raw.target_platforms, "project.target_platforms", source)?;
    Ok(Project {
        name: raw.name,
        bundle: raw.bundle,
        version,
        app_id: raw.app_id,
        target_platforms,
    })
}

fn build_ui(raw: RawUi, source: &Path) -> Result<Ui, ConfigError> {
    let framework = Framework::from_str(&raw.framework).ok_or_else(|| {
        ConfigError::invalid(
            source,
            format!(
                "unknown ui.framework '{}'; expected native, static, or server",
                raw.framework
            ),
        )
    })?;
    Ok(Ui {
        framework,
        icon: raw.icon.map(PathBuf::from),
    })
}

fn build_package(raw: RawPackage, source: &Path) -> Result<PackageMeta, ConfigError> {
    let version = parse_version(&raw.version, "package.version", source)?;
    let peko = raw
        .peko
        .map(|req| parse_version_req(&req, "package.peko", source))
        .transpose()?;
    Ok(PackageMeta {
        name: raw.name,
        version,
        description: raw.description,
        license: raw.license,
        authors: raw.authors,
        repository: raw.repository,
        keywords: raw.keywords,
        categories: raw.categories,
        peko,
    })
}

fn build_platforms(raw: Option<RawPlatforms>, source: &Path) -> Result<Platforms, ConfigError> {
    let Some(raw) = raw else {
        return Ok(Platforms::default());
    };
    let supported = parse_platforms(&raw.supported, "platforms.supported", source)?;
    Ok(Platforms { supported })
}

fn build_dependencies(
    raw: BTreeMap<String, RawDependency>,
    source: &Path,
) -> Result<BTreeMap<String, Dependency>, ConfigError> {
    let mut dependencies = BTreeMap::new();
    for (name, entry) in raw {
        dependencies.insert(name.clone(), build_dependency(&name, entry, source)?);
    }
    Ok(dependencies)
}

fn build_dependency(
    name: &str,
    raw: RawDependency,
    source: &Path,
) -> Result<Dependency, ConfigError> {
    match raw {
        RawDependency::Simple(req) => {
            let version = parse_version_req(&req, &format!("dependencies.{name}"), source)?;
            Ok(Dependency::Registry { version })
        }
        RawDependency::Detailed(table) => match (table.version, table.path) {
            (Some(_), Some(_)) => Err(ConfigError::invalid(
                source,
                format!("dependency '{name}' sets both version and path"),
            )),
            (Some(req), None) => {
                let version =
                    parse_version_req(&req, &format!("dependencies.{name}.version"), source)?;
                Ok(Dependency::Registry { version })
            }
            (None, Some(path)) => Ok(Dependency::Path {
                path: PathBuf::from(path),
            }),
            (None, None) => Err(ConfigError::invalid(
                source,
                format!("dependency '{name}' sets neither version nor path"),
            )),
        },
    }
}

fn build_native(raw: RawNative, source: &Path) -> Result<Native, ConfigError> {
    let flags = split_os_table(raw.flags, "native.flags", source)?;
    let link = split_os_table(raw.link, "native.link", source)?;
    Ok(Native {
        sources: raw.sources.into_iter().map(PathBuf::from).collect(),
        include: raw.include.into_iter().map(PathBuf::from).collect(),
        flags: NativeFlags {
            all: flags.0,
            per_os: flags.1,
        },
        link: NativeLink {
            all: link.0,
            per_os: link.1,
        },
        libs: NativeLibs {
            per_platform: raw
                .libs
                .into_iter()
                .map(|(key, paths)| (key, paths.into_iter().map(PathBuf::from).collect()))
                .collect(),
        },
        vendor: raw
            .vendor
            .into_iter()
            .map(|vendor| Vendor {
                name: vendor.name,
                path: PathBuf::from(vendor.path),
                flags: vendor.flags,
            })
            .collect(),
    })
}

/// A flag table split into its `all` entry and its per-operating-system
/// entries.
type OsFlagTable = (Vec<String>, Vec<(OperatingSystem, Vec<String>)>);

/// Split a flag table into its `all` entry and its per-operating-system
/// entries, rejecting any key that is neither `all` nor a known platform.
fn split_os_table(
    raw: BTreeMap<String, Vec<String>>,
    table: &str,
    source: &Path,
) -> Result<OsFlagTable, ConfigError> {
    let mut all = Vec::new();
    let mut per_os = Vec::new();
    for (key, values) in raw {
        if key == "all" {
            all = values;
            continue;
        }
        let os = operating_system_from_str(&key).ok_or_else(|| {
            ConfigError::invalid(source, format!("unknown platform '{key}' in [{table}]"))
        })?;
        per_os.push((os, values));
    }
    Ok((all, per_os))
}

/// Map a list of platform identifiers, reporting the field on an unknown
/// identifier.
fn parse_platforms(
    identifiers: &[String],
    field: &str,
    source: &Path,
) -> Result<Vec<OperatingSystem>, ConfigError> {
    let mut platforms = Vec::with_capacity(identifiers.len());
    for identifier in identifiers {
        let os = operating_system_from_str(identifier).ok_or_else(|| {
            ConfigError::invalid(
                source,
                format!("unknown platform '{identifier}' in {field}"),
            )
        })?;
        platforms.push(os);
    }
    Ok(platforms)
}

fn parse_version(text: &str, field: &str, source: &Path) -> Result<Version, ConfigError> {
    Version::parse(text)
        .map_err(|err| ConfigError::invalid(source, format!("invalid version in {field}: {err}")))
}

fn parse_version_req(text: &str, field: &str, source: &Path) -> Result<VersionReq, ConfigError> {
    VersionReq::parse(text).map_err(|err| {
        ConfigError::invalid(
            source,
            format!("invalid version requirement in {field}: {err}"),
        )
    })
}
