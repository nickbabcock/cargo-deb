use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;
use std::collections::{HashMap, HashSet};
use std::borrow::Cow;
use listener::Listener;
use toml;
use file;
use glob;
use dependencies::resolve;
use serde_json;
use error::*;
use try::Try;
use config::CargoConfig;

fn is_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('[') || s.contains(']') || s.contains('!')
}

#[derive(Debug, Clone)]
pub enum AssetSource {
    /// Copy file from the path (and strip binary if needed).
    Path(PathBuf),
    /// Write data to destination as-is.
    Data(Vec<u8>),
}

impl AssetSource {
    pub fn path(&self) -> Option<&Path> {
        match *self {
            AssetSource::Path(ref p) => Some(p),
            _ => None,
        }
    }

    pub fn len(&self) -> Option<u64> {
        match *self {
            // FIXME: may not be accurate if the executable is not stripped yet?
            AssetSource::Path(ref p) => {
                fs::metadata(p).ok().map(|m| m.len())
            },
            AssetSource::Data(ref d) => {
                Some(d.len() as u64)
            },
        }
    }

    pub fn data(&self) -> CDResult<Cow<[u8]>> {
        Ok(match *self {
            AssetSource::Path(ref p) => {
                let data = file::get(p)
                    .map_err(|e| CargoDebError::IoFile("unable to read asset to add to archive", e, p.to_owned()))?;
                Cow::Owned(data)
            },
            AssetSource::Data(ref d) => {
                Cow::Borrowed(d)
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub source: AssetSource,
    pub target_path: PathBuf,
    pub chmod: u32,
    is_built: bool,
}

impl Asset {
    pub fn new(source: AssetSource, mut target_path: PathBuf, chmod: u32, is_built: bool) -> Self {
        if target_path.is_absolute() {
            target_path = target_path.strip_prefix("/").expect("no root dir").to_owned();
        }
        // is_dir() is only for paths that exist
        if target_path.to_string_lossy().ends_with('/') {
            let file_name = source.path().and_then(|p| p.file_name()).expect("source must be a file");
            target_path = target_path.join(file_name);
        }
        Self {
            source,
            target_path,
            chmod,
            is_built,
        }
    }

    fn is_executable(&self) -> bool {
        0 != (self.chmod & 0o111)
    }

    fn is_dynamic_library(&self) -> bool {
        self.target_path.file_name()
            .and_then(|f| f.to_str())
            .map_or(false, |f| f.ends_with(DLL_SUFFIX))
    }
}

#[derive(Debug)]
/// Cargo deb configuration read from the manifest and cargo metadata
pub struct Config {
    /// Root directory where `Cargo.toml` is located. It's a subdirectory in workspaces.
    pub manifest_dir: PathBuf,
    /// Triple. `None` means current machine architecture.
    pub target: Option<String>,
    /// `CARGO_TARGET_DIR`
    pub target_dir: PathBuf,
    /// The name of the project to build
    pub name: String,
    /// The software license of the project (SPDX format).
    pub license: Option<String>,
    /// The location of the license file
    pub license_file: Option<PathBuf>,
    /// number of lines to skip when reading `license_file`
    pub license_file_skip_lines: usize,
    /// The copyright of the project
    /// (Debian's `copyright` file contents).
    pub copyright: String,
    pub changelog: Option<String>,
    /// The version number of the project.
    pub version: String,
    /// The homepage URL of the project.
    pub homepage: Option<String>,
    /// Documentation URL from `Cargo.toml`. Fallback if `homepage` is missing.
    pub documentation: Option<String>,
    /// The URL of the software repository.
    pub repository: Option<String>,
    /// A short description of the project.
    pub description: String,
    /// An extended description of the project.
    pub extended_description: Option<String>,
    /// The maintainer of the Debian package.
    /// In Debian `control` file `Maintainer` field format.
    pub maintainer: String,
    /// The Debian dependencies required to run the project.
    pub depends: String,
    /// The Debian software category to which the package belongs.
    pub section: Option<String>,
    /// The Debian priority of the project. Typically 'optional'.
    pub priority: String,

    /// `Conflicts` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub conflicts: Option<String>,
    /// `Breaks` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub breaks: Option<String>,
    /// `Replaces` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub replaces: Option<String>,
    /// `Provides` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub provides: Option<String>,

    /// The Debian architecture of the target system.
    pub architecture: String,
    /// A list of configuration files installed by the package.
    pub conf_files: Option<String>,
    /// All of the files that are to be packaged.
    pub assets: Vec<Asset>,
    /// The path were possible maintainer scripts live
    pub maintainer_scripts: Option<PathBuf>,
    /// List of Cargo features to use during build
    pub features: Vec<String>,
    pub default_features: bool,
    /// Should the binary be stripped from debug symbols?
    pub strip: bool,
    _use_constructor_to_make_this_struct_: (),
}

impl Config {
    /// Makes a new config from `Cargo.toml` in the current working directory.
    ///
    /// `None` target means the host machine's architecture.
    pub fn from_manifest(manifest_path: &Path, target: Option<&str>, variant: Option<&str>, listener: &mut Listener) -> CDResult<Config> {
        let metadata = cargo_metadata(manifest_path)?;
        let root_id = metadata.resolve.root;
        let root_package = metadata.packages.iter()
            .find(|p| p.id == root_id)
            .ok_or("Unable to find root package in cargo metadata")?;
        let target_dir = Path::new(&metadata.target_directory);
        let manifest_path = Path::new(&root_package.manifest_path);
        let manifest_dir = manifest_path.parent().unwrap();
        let content = file::get_text(&manifest_path)
            .map_err(|e| CargoDebError::IoFile("unable to read Cargo.toml", e, manifest_path.to_owned()))?;
        toml::from_str::<Cargo>(&content)?.into_config(root_package, manifest_dir, target_dir, target, variant, listener)
    }

    pub(crate) fn get_dependencies(&self, listener: &mut Listener) -> CDResult<String> {
        let mut deps = HashSet::new();
        for word in self.depends.split(',') {
            let word = word.trim();
            if word == "$auto" {
                for bname in self.all_binaries().into_iter().filter_map(|p| p.path()) {
                    match resolve(bname, &self.architecture, listener) {
                        Ok(bindeps) => for dep in bindeps {
                            deps.insert(dep);
                        },
                        Err(err) => {
                            listener.warning(format!("{} (no auto deps for {})", err, bname.display()));
                        },
                    };
                }
            } else {
                deps.insert(word.to_owned());
            }
        }
        Ok(deps.into_iter().collect::<Vec<_>>().join(", "))
    }

    pub(crate) fn add_copyright_asset(&mut self) -> CDResult<()> {
        let copyright_file = ::data::generate_copyright_asset(self)?;
        self.assets.push(Asset::new(
            AssetSource::Data(copyright_file),
            Path::new("usr/share/doc")
                .join(&self.name)
                .join("copyright"),
            0o644,
            false,
        ));
        Ok(())
    }

    fn add_changelog_asset(&mut self) -> CDResult<()> {
        // The file is autogenerated later
        if self.changelog.is_some() {
            if let Some(changelog_file) = ::data::generate_changelog_asset(self)? {
                self.assets.push(Asset::new(
                    AssetSource::Data(changelog_file),
                    Path::new("usr/share/doc")
                        .join(&self.name)
                        .join("changelog.gz"),
                    0o644,
                    false,
                ));
            }
        }
        Ok(())
    }

    /// Executables AND dynamic libraries
    fn all_binaries(&self) -> Vec<&AssetSource> {
        self.binaries(false)
    }

    /// Executables AND dynamic libraries, but only in `target/release`
    pub(crate) fn built_binaries(&self) -> Vec<&AssetSource> {
        self.binaries(true)
    }

    fn binaries(&self, built_only: bool) -> Vec<&AssetSource> {
        self.assets.iter().filter_map(|asset| {
            // Assumes files in build dir which have executable flag set are binaries
            if (!built_only || asset.is_built) &&
                (asset.is_dynamic_library() || asset.is_executable()) {
                    Some(&asset.source)
            } else {
                None
            }
        }).collect()
    }

    /// Tries to guess type of source control used for the repo URL.
    /// It's a guess, and it won't be 100% accurate, because Cargo suggests using
    /// user-friendly URLs or webpages instead of tool-specific URL schemes.
    pub(crate) fn repository_type(&self) -> Option<&str> {
        if let Some(ref repo) = self.repository {
            if repo.starts_with("git+") || repo.ends_with(".git") || repo.contains("git@") || repo.contains("github.com") || repo.contains("gitlab.com") {
                return Some("Git");
            }
            if repo.starts_with("cvs+") || repo.contains("pserver:") || repo.contains("@cvs.") {
                return Some("Cvs");
            }
            if repo.starts_with("hg+") || repo.contains("hg@") || repo.contains("/hg.") {
                return Some("Hg");
            }
            if repo.starts_with("svn+") || repo.contains("/svn.") {
                return Some("Svn");
            }
            return None;
        }
        None
    }

    pub(crate) fn path_in_build<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.target_dir.join("release").join(rel_path)
    }

    pub(crate) fn path_in_workspace<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.manifest_dir.join(rel_path)
    }

    pub(crate) fn deb_dir(&self) -> PathBuf {
        self.target_dir.join("debian")
    }

    pub fn path_in_deb<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.deb_dir().join(rel_path)
    }

    pub(crate) fn cargo_config(&self) -> CDResult<Option<CargoConfig>> {
        CargoConfig::new(&self.target_dir)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Cargo {
    pub package: CargoPackage,
    pub profile: Option<CargoProfiles>,
}

impl Cargo {
    /// Convert Cargo.toml/metadata information into internal configu structure
    ///
    /// **IMPORTANT**: This function must not create or expect to see any files on disk!
    /// It's run before destination directory is cleaned up, and before the build start!
    ///
    fn into_config(
        mut self,
        root_package: &CargoMetadataPackage,
        manifest_dir: &Path,
        target_dir: &Path,
        target: Option<&str>,
        variant: Option<&str>,
        listener: &mut Listener,
    ) -> CDResult<Config> {
        // Cargo cross-compiles to a dir
        let target_dir = if let Some(target) = target {
            target_dir.join(target)
        } else {
            target_dir.to_owned()
        };

        // If we build against a variant use that config and change the package name
        let mut deb = if let Some(variant) = variant {
            // Use dash as underscore is not allowed in package names
            self.package.name = format!("{}-{}", self.package.name, variant);
            let mut deb = self.package
                .metadata
                .take()
                .and_then(|m| m.deb)
                .unwrap_or_else(CargoDeb::default);
            let variant = deb.variants
                .as_mut()
                .and_then(|v| v.remove(variant))
                .ok_or(CargoDebError::VariantNotFound(variant.to_string()))?;
            variant.inherit_from(deb)
        } else {
            self.package
                .metadata
                .take()
                .and_then(|m| m.deb)
                .unwrap_or_else(CargoDeb::default)
        };

        let (license_file, license_file_skip_lines) = self.license_file(deb.license_file.as_ref())?;
        let readme = self.package.readme.as_ref();
        self.check_config(manifest_dir, readme, &deb, listener);
        let mut config = Config {
            manifest_dir: manifest_dir.to_owned(),
            target: target.map(|t| t.to_string()),
            target_dir,
            name: self.package.name.clone(),
            license: self.package.license.take(),
            license_file,
            license_file_skip_lines,
            copyright: deb.copyright.take().ok_or_then(|| {
                Ok(self.package.authors.as_ref().ok_or("Package must have a copyright or authors")?.join(", "))
            })?,
            version: self.version_string(deb.revision),
            homepage: self.package.homepage.clone(),
            documentation: self.package.documentation.clone(),
            repository: self.package.repository.take(),
            description: self.package.description.take().unwrap_or_else(||format!("[generated from Rust crate {}]", self.package.name)),
            extended_description: self.extended_description(deb.extended_description.take(), readme)?,
            maintainer: deb.maintainer.take().ok_or_then(|| {
                Ok(self.package.authors.as_ref().and_then(|a|a.get(0))
                    .ok_or("Package must have a maintainer or authors")?.to_owned())
            })?,
            depends: deb.depends.take().unwrap_or("$auto".to_owned()),
            conflicts: deb.conflicts.take(),
            breaks: deb.breaks.take(),
            replaces: deb.replaces.take(),
            provides: deb.provides.take(),
            section: deb.section.take(),
            priority: deb.priority.take().unwrap_or("optional".to_owned()),
            architecture: get_arch(target.unwrap_or(::DEFAULT_TARGET)).to_owned(),
            conf_files: deb.conf_files.map(|x| x.iter().fold(String::new(), |a, b| a + b + "\n")),
            assets: vec![],
            changelog: deb.changelog.take(),
            maintainer_scripts: deb.maintainer_scripts.map(PathBuf::from),
            features: deb.features.take().unwrap_or(vec![]),
            default_features: deb.default_features.unwrap_or(true),
            strip: self.profile.as_ref().and_then(|p|p.release.as_ref())
                .and_then(|r|r.debug).map(|debug|!debug).unwrap_or(true),
            _use_constructor_to_make_this_struct_: (),
        };

        let assets = self.take_assets(&config, deb.assets.take(), &root_package.targets, readme)?;
        if assets.is_empty() {
            Err("No binaries or cdylibs found. The package is empty. Please specify some assets to package in Cargo.toml")?;
        }
        config.assets.extend(assets);
        config.add_copyright_asset()?;
        config.add_changelog_asset()?;

        Ok(config)
    }

    fn check_config(&self, manifest_dir: &Path, readme: Option<&String>, deb: &CargoDeb, listener: &mut Listener) {
        if self.package.description.is_none() {
            listener.warning("description field is missing in Cargo.toml".to_owned());
        }
        if self.package.license.is_none() {
            listener.warning("license field is missing in Cargo.toml".to_owned());
        }
        if let Some(readme) = readme {
            if deb.extended_description.is_none() && (readme.ends_with(".md") || readme.ends_with(".markdown")) {
                listener.warning(format!("extended-description field missing. Using {}, but markdown may not render well.",readme));
            }
        } else {
            for p in &["README.md", "README.markdown", "README.txt", "README"] {
                if manifest_dir.join(p).exists() {
                    listener.warning(format!("{} file exists, but is not specified in `readme` Cargo.toml field", p));
                    break;
                }
            }
        }
    }

    fn extended_description(&self, desc: Option<String>, readme: Option<&String>) -> CDResult<Option<String>> {
        Ok(if desc.is_some() {
            desc
        } else if let Some(readme) = readme {
            Some(file::get_text(readme)
                .map_err(|err| CargoDebError::IoFile("unable to read README", err, PathBuf::from(readme)))?)
        } else {
            None
        })
    }

    fn license_file(&mut self, license_file: Option<&Vec<String>>) -> CDResult<(Option<PathBuf>, usize)> {
        if let Some(args) = license_file {
            let mut args = args.iter();
            let file = args.next();
            let lines = if let Some(lines) = args.next() {
                lines.parse().map_err(|e| CargoDebError::NumParse("invalid number of lines", e))?
            } else {0};
            Ok((file.map(|s|s.into()), lines))
        } else {
            Ok((self.package.license_file.as_ref().map(|s| s.into()), 0))
        }
    }

    fn take_assets(&self, options: &Config, assets: Option<Vec<Vec<String>>>, targets: &[CargoMetadataTarget], readme: Option<&String>) -> CDResult<Vec<Asset>> {
        Ok(if let Some(assets) = assets {
            let mut all_assets = Vec::with_capacity(assets.len());
            for mut v in assets {
                let mut v = v.drain(..);
                let source_path = PathBuf::from(v.next()
                    .ok_or("missing path (first array entry) for asset in Cargo.toml")?);
                let (is_built, source_path) = if let Ok(rel_path) = source_path.strip_prefix("target/release") {
                    (true, options.path_in_build(rel_path))
                } else {
                    (false, options.path_in_workspace(&source_path))
                };
                let target_path = PathBuf::from(v.next().ok_or("missing target (second array entry) for asset in Cargo.toml")?);
                let mode = u32::from_str_radix(&v.next().ok_or("missing chmod (third array entry) for asset in Cargo.toml")?, 8)
                    .map_err(|e| CargoDebError::NumParse("unable to parse chmod argument", e))?;
                let source_prefix: PathBuf = source_path.iter()
                    .take_while(|part| !is_glob_pattern(part.to_str().unwrap()))
                    .collect();
                let source_is_glob = is_glob_pattern(source_path.to_str().unwrap());
                let mut file_matches = glob::glob(source_path.to_str().unwrap())?
                    // Remove dirs from globs without throwing away errors
                    .map(|entry| {
                        let source_file = entry?;
                        Ok(if source_file.is_dir() {
                            None
                        } else {
                            Some(source_file)
                        })
                    })
                    .filter_map(|res| match res {
                        Ok(None) => None,
                        Ok(Some(x)) => Some(Ok(x)),
                        Err(x) => Some(Err(x)),
                    })
                    .collect::<CDResult<Vec<_>>>()?;
                // If glob didn't match anything, it's probably a regular path
                // to a file that hasn't been built yet
                if file_matches.is_empty() {
                    file_matches.push(source_path);
                }
                for source_file in file_matches {
                    // XXX: how do we handle duplicated assets?
                    let target_file = if source_is_glob {
                        target_path.join(source_file.strip_prefix(&source_prefix).unwrap())
                    } else {
                        target_path.clone()
                    };
                    all_assets.push(Asset::new(
                        AssetSource::Path(source_file),
                        target_file,
                        mode,
                        is_built,
                    ));
                }
            }
            all_assets
        } else {
            let mut implied_assets: Vec<_> = targets
                .iter()
                .filter_map(|t| {
                    if t.crate_types.iter().any(|ty| ty == "bin") && t.kind.iter().any(|k| k == "bin") {
                        Some(Asset::new(
                            AssetSource::Path(options.path_in_build(&t.name)),
                            Path::new("usr/bin").join(&t.name),
                            0o755,
                            true,
                        ))
                    } else if t.crate_types.iter().any(|ty| ty == "cdylib") && t.kind.iter().any(|k| k == "cdylib") {
                        // FIXME: std has constants for the host arch, but not for cross-compilation
                        let lib_name = format!("{}{}{}", DLL_PREFIX, t.name, DLL_SUFFIX);
                        Some(Asset::new(
                            AssetSource::Path(options.path_in_build(&lib_name)),
                            Path::new("usr/lib").join(lib_name),
                            0o644,
                            true,
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(readme) = readme {
                let target_path = Path::new("usr/share/doc").join(&self.package.name).join(readme);
                implied_assets.push(Asset::new(
                    AssetSource::Path(PathBuf::from(readme)),
                    target_path,
                    0o644,
                    false,
                ));
            }
            implied_assets
        })
    }

    fn version_string(&self, revision: Option<String>) -> String {
        if let Some(revision) = revision {
            format!("{}-{}", self.package.version, revision)
        } else {
            self.package.version.clone()
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoPackage {
    pub name: String,
    pub authors: Option<Vec<String>>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub homepage: Option<String>,
    pub documentation: Option<String>,
    pub repository: Option<String>,
    pub version: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub metadata: Option<CargoPackageMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
struct CargoPackageMetadata {
    pub deb: Option<CargoDeb>,
}

#[derive(Clone, Debug, Deserialize)]
struct CargoProfiles {
    pub release: Option<CargoProfile>,
}

#[derive(Clone, Debug, Deserialize)]
struct CargoProfile {
    pub debug: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoBin {
    pub name: String,
    pub plugin: Option<bool>,
    pub proc_macro: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct CargoDeb {
    pub maintainer: Option<String>,
    pub copyright: Option<String>,
    pub license_file: Option<Vec<String>>,
    pub changelog: Option<String>,
    pub depends: Option<String>,
    pub conflicts: Option<String>,
    pub breaks: Option<String>,
    pub replaces: Option<String>,
    pub provides: Option<String>,
    pub extended_description: Option<String>,
    pub section: Option<String>,
    pub priority: Option<String>,
    pub revision: Option<String>,
    pub conf_files: Option<Vec<String>>,
    pub assets: Option<Vec<Vec<String>>>,
    pub maintainer_scripts: Option<String>,
    pub features: Option<Vec<String>>,
    pub default_features: Option<bool>,
    pub variants: Option<HashMap<String, CargoDeb>>,
}

impl CargoDeb {
    fn inherit_from(self, parent: CargoDeb) -> CargoDeb {
        CargoDeb {
            maintainer: self.maintainer.or(parent.maintainer),
            copyright: self.copyright.or(parent.copyright),
            license_file: self.license_file.or(parent.license_file),
            changelog: self.changelog.or(parent.changelog),
            depends: self.depends.or(parent.depends),
            conflicts: self.conflicts.or(parent.conflicts),
            breaks: self.breaks.or(parent.breaks),
            replaces: self.replaces.or(parent.replaces),
            provides: self.provides.or(parent.provides),
            extended_description: self.extended_description.or(parent.extended_description),
            section: self.section.or(parent.section),
            priority: self.priority.or(parent.priority),
            revision: self.revision.or(parent.revision),
            conf_files: self.conf_files.or(parent.conf_files),
            assets: self.assets.or(parent.assets),
            maintainer_scripts: self.maintainer_scripts.or(parent.maintainer_scripts),
            features: self.features.or(parent.features),
            default_features: self.default_features.or(parent.default_features),
            variants: self.variants.or(parent.variants),
        }
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoMetadataPackage>,
    resolve: CargoMetadataResolve,
    target_directory: String,
}

#[derive(Deserialize)]
struct CargoMetadataResolve {
    root: String,
}

#[derive(Deserialize)]
struct CargoMetadataPackage {
    pub id: String,
    pub targets: Vec<CargoMetadataTarget>,
    pub manifest_path: String,
}

#[derive(Deserialize)]
struct CargoMetadataTarget {
    pub name: String,
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
}

/// Returns the path of the `Cargo.toml` that we want to build.
fn cargo_metadata(manifest_path: &Path) -> CDResult<CargoMetadata> {
    let mut cmd = Command::new("cargo");
    cmd.arg("metadata");
    cmd.arg("--format-version=1");
    cmd.arg(format!("--manifest-path={}", manifest_path.display()));

    let output = cmd.output()
        .map_err(|e| CargoDebError::CommandFailed(e, "cargo (is it in your PATH?)"))?;
    if !output.status.success() {
        return Err(CargoDebError::CommandError("cargo", "metadata".to_owned(), output.stderr));
    }

    let stdout = String::from_utf8(output.stdout).unwrap();
    let metadata = serde_json::from_str(&stdout)?;
    Ok(metadata)
}

/// Debianizes the architecture name
fn get_arch(target: &str) -> &str {
    let mut parts = target.split('-');
    let arch = parts.next().unwrap();
    let abi = parts.last().unwrap_or("");
    match (arch, abi) {
        // https://wiki.debian.org/Multiarch/Tuples
        // rustc --print target-list
        // https://doc.rust-lang.org/std/env/consts/constant.ARCH.html
        ("aarch64", _)          => "arm64",
        ("mips64", "gnuabin32") => "mipsn32",
        ("mips64el", "gnuabin32") => "mipsn32el",
        ("mipsisa32r6", _) => "mipsr6",
        ("mipsisa32r6el", _) => "mipsr6el",
        ("mipsisa64r6", "gnuabi64") => "mips64r6",
        ("mipsisa64r6", "gnuabin32") => "mipsn32r6",
        ("mipsisa64r6el", "gnuabi64") => "mips64r6el",
        ("mipsisa64r6el", "gnuabin32") => "mipsn32r6el",
        ("powerpc", "gnuspe") => "powerpcspe",
        ("powerpc64", _)   => "ppc64",
        ("powerpc64le", _) => "ppc64el",
        ("i586", _) |
        ("i686", _) |
        ("x86", _)   => "i386",
        ("x86_64", "gnux32") => "x32",
        ("x86_64", _) => "amd64",
        (arm, gnueabi) if arm.starts_with("arm") && gnueabi.ends_with("hf") => "armhf",
        (arm, _) if arm.starts_with("arm") => "armel",
        (other_arch, _) => other_arch,
    }
}

#[test]
fn match_arm_arch() {
    assert_eq!("armhf", get_arch("arm-unknown-linux-gnueabihf"));
}

#[test]
fn assets() {
    let a = Asset::new(
        AssetSource::Path(PathBuf::from("target/release/bar")),
        PathBuf::from("baz/"),
        0o644,
        true,
    );
    assert_eq!("baz/bar", a.target_path.to_str().unwrap());
    assert!(a.is_built);

    let a = Asset::new(
        AssetSource::Path(PathBuf::from("foo/bar")),
        PathBuf::from("/baz/quz"),
        0o644,
        false,
    );
    assert_eq!("baz/quz", a.target_path.to_str().unwrap());
    assert!(!a.is_built);
}
