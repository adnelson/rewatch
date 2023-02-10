use crate::bsconfig;
use crate::bsconfig::*;
use crate::helpers;
use crate::structure_hashmap;
use ahash::{AHashMap, AHashSet};
use convert_case::{Case, Casing};
use rayon::prelude::*;
use std::fs;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub parent: Option<String>,
    pub bsconfig: bsconfig::T,
    pub source_folders: AHashSet<(String, bsconfig::PackageSource)>,
    pub source_files: Option<AHashMap<String, fs::Metadata>>,
    pub namespace: Option<String>,
    pub modules: Option<AHashSet<String>>,
}

impl PartialEq for Package {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Eq for Package {}
impl Hash for Package {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

/// Given a projects' root folder and a `bsconfig::Source`, this recursively creates all the
/// sources in a flat list. In the process, it removes the children, as they are being resolved
/// because of the recursiveness. So you get a flat list of files back, retaining the type_ and
/// wether it needs to recurse into all structures
fn get_source_dirs(
    project_root: &str,
    source: Source,
) -> AHashSet<(String, bsconfig::PackageSource)> {
    let mut source_folders: AHashSet<(String, bsconfig::PackageSource)> = AHashSet::new();

    let (package_root, subdirs, full_recursive) = match source.to_owned() {
        Source::Shorthand(dir)
        | Source::Qualified(PackageSource {
            dir, subdirs: None, ..
        }) => (dir, None, false),
        Source::Qualified(PackageSource {
            dir,
            subdirs: Some(Subdirs::Recurse(recurse)),
            ..
        }) => (dir, None, recurse),
        Source::Qualified(PackageSource {
            dir,
            subdirs: Some(Subdirs::Qualified(subdirs)),
            ..
        }) => (dir, Some(subdirs), false),
    };

    let full_path = project_root.to_string() + "/" + &package_root;
    source_folders.insert((
        full_path.to_owned(),
        bsconfig::to_qualified_without_children(&source),
    ));

    if !full_recursive {
        subdirs
            .unwrap_or(vec![])
            .par_iter()
            .map(|subdir| get_source_dirs(&full_path, subdir.to_owned()))
            .collect::<Vec<AHashSet<(String, bsconfig::PackageSource)>>>()
            .into_iter()
            .for_each(|subdir| source_folders.extend(subdir))
    }

    source_folders
}

/// # Make Package
/// Given a directory that includes a bsconfig file, read it, and recursively find all other
/// bsconfig files, and turn those into Packages as well.
fn build_package(
    is_root: bool,
    project_root: &str,
    package_name: &str,
    parent: Option<String>,
) -> AHashMap<String, Package> {
    let mut children: AHashMap<String, Package> = AHashMap::new();

    let package_dir = if is_root {
        project_root.to_owned()
    } else {
        project_root.to_owned() + "/node_modules/" + package_name
    };

    let bsconfig = bsconfig::read(package_dir.to_string() + "/bsconfig.json");

    let source_folders = match bsconfig.sources.to_owned() {
        bsconfig::OneOrMore::Single(source) => get_source_dirs(&package_dir, source),
        bsconfig::OneOrMore::Multiple(sources) => {
            let mut source_folders: AHashSet<(String, bsconfig::PackageSource)> = AHashSet::new();
            sources
                .par_iter()
                .map(|source| get_source_dirs(&package_dir, source.to_owned()))
                .collect::<Vec<AHashSet<(String, bsconfig::PackageSource)>>>()
                .into_iter()
                .for_each(|source| source_folders.extend(source));
            source_folders
        }
    };

    /* At this point in time we may have started encountering elements multiple times as there is
     * no deduplication on the package level so far. Once we return this flat list of packages, do
     * have this deduplication. From that point on, we can add the source files for every single
     * one as that is an expensive operation IO wise and we don't want to duplicate that.*/
    // dbg!("PACKAGE____");
    // dbg!(&bsconfig.name.to_owned());
    // dbg!(&bsconfig.namespace);
    children.insert(
        package_dir.to_owned(),
        Package {
            name: bsconfig.name.to_owned(),
            parent,
            bsconfig: bsconfig.to_owned(),
            source_folders,
            source_files: None,
            namespace: match bsconfig.namespace {
                Some(bsconfig::Namespace::Bool(true)) => {
                    Some(namespace_from_package_name(&bsconfig.name))
                }
                Some(bsconfig::Namespace::Bool(false)) => None,
                None => None,
                Some(bsconfig::Namespace::String(str)) => match str.as_str() {
                    "true" => Some(namespace_from_package_name(&bsconfig.name)),
                    namespace => Some(namespace.to_string()),
                },
            },
            modules: None,
        },
    );

    bsconfig
        .bs_dependencies
        .to_owned()
        .unwrap_or(vec![])
        .par_iter()
        .map(|dep| build_package(false, &project_root, &dep, Some(package_dir.to_string())))
        .collect::<Vec<AHashMap<String, Package>>>()
        .into_iter()
        .for_each(|child| children.extend(child));

    children
}

/// `get_source_files` is essentially a wrapper around `structure_hashmap::read_structure`, which read a
/// list of files in a folder to a hashmap of `string` / `fs::Metadata` (file metadata). Reason for
/// this wrapper is the recursiveness of the `bsconfig.json` subfolders. Some sources in bsconfig
/// can be specified as being fully recursive (`{ subdirs: true }`). This wrapper pulls out that
/// data from the config and pushes it forwards. Another thing is the 'type_', some files / folders
/// can be marked with the type 'dev'. Which means that they may not be around in the distributed
/// NPM package. The file reader allows for this, just warns when this happens.
/// TODO -> Check wether we actually need the `fs::Metadata`
pub fn get_source_files(dir: &String, source: &PackageSource) -> AHashMap<String, fs::Metadata> {
    let mut map: AHashMap<String, fs::Metadata> = AHashMap::new();

    let (recurse, type_) = match source {
        PackageSource {
            subdirs: Some(Subdirs::Recurse(subdirs)),
            type_,
            ..
        } => (subdirs.to_owned(), type_),
        PackageSource { type_, .. } => (false, type_),
    };

    // don't include dev sources for now
    if type_ != &Some("dev".to_string()) {
        match structure_hashmap::read_folders(dir, recurse) {
            Ok(files) => map.extend(files),
            Err(_e) if type_ == &Some("dev".to_string()) => {
                println!("Could not read folder: {dir}... Probably ok as type is dev")
            }
            Err(_e) => println!("Could not read folder: {dir}..."),
        }
    }

    map
}

pub fn namespace_from_package_name(package_name: &str) -> String {
    package_name
        .to_owned()
        .replace("@", "")
        .replace("/", "_")
        .to_case(Case::Pascal)
}

/// This takes the tree of packages, and finds all the source files for each, adding them to the
/// respective packages.
fn extend_with_children(mut build: AHashMap<String, Package>) -> AHashMap<String, Package> {
    for (_key, value) in build.iter_mut() {
        let mut map: AHashMap<String, fs::Metadata> = AHashMap::new();
        value
            .source_folders
            .par_iter()
            .map(|(dir, source)| get_source_files(dir, source))
            .collect::<Vec<AHashMap<String, fs::Metadata>>>()
            .into_iter()
            .for_each(|source| map.extend(source));

        let mut modules = AHashSet::from_iter(
            map.keys()
                .map(|key| helpers::file_path_to_module_name(key, value.namespace.to_owned())),
        );
        match value.namespace.to_owned() {
            Some(namespace) => {
                let _ = modules.insert(namespace);
            }
            None => (),
        }
        value.modules = Some(modules);
        value.source_files = Some(map);
    }
    build
}

/// Make turns a folder, that should contain a bsconfig, into a tree of Packages.
/// It does so in two steps:
/// 1. Get all the packages parsed, and take all the source folders from the bsconfig
/// 2. Take the (by then deduplicated) packages, and find all the '.re', '.res', '.ml' and
///    interface files.
/// The two step process is there to reduce IO overhead
pub fn make(folder: &str) -> AHashMap<String, Package> {
    /* The build_package get's called recursively. By using extend, we deduplicate all the packages
     * */
    let mut map: AHashMap<String, Package> = AHashMap::new();
    map.extend(build_package(true, folder, "", None));
    /* Once we have the deduplicated packages, we can add the source files for each - to minimize
     * the IO */
    extend_with_children(map)
}