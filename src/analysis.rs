use guppy::graph::{DependencyDirection, DependencyLink, PackageGraph};
use guppy::{MetadataCommand, PackageId};
use serde::{Deserialize, Serialize};
use std::collections::{
  hash_map::{Entry, HashMap},
  HashSet,
};
use std::iter::FromIterator;
use std::path::PathBuf;
use tempdir::TempDir;

use crate::metrics;

//
// Essential Structs
// =================
//

/// PackageRisk contains information about a package after analysis.
#[rustfmt::skip]
#[derive(Default, Serialize, Deserialize)]
pub struct PackageRisk {

  // metadata
  // --------

  /// name of the dependency
  pub name: String,
  /// potentially different versions are pulled (bad)
  pub versions: HashSet<String>,
  /// link to its repository
  pub repo: Option<String>,
  /// description from Cargo.toml
  pub description: Option<String>,

  // TODO: get description of each crate from Cargo.toml

  // useful for analysis
  // -------------------

  /// path to the actual source code on disk
  #[serde(skip)]
  pub manifest_path: PathBuf,

  // analysis result
  // ---------------

  /// is this dependency used for the host target and features?
  pub used: bool,
  // TODO: skip any PackageId with serde, and have <String> for HTML
  /// transitive dependencies (not including this dependency)
  pub transitive_dependencies: HashSet<PackageId>,
  /// number of root crates that import this package
  pub root_importers: Vec<PackageId>,
  /// total number of transitive third party dependencies imported
  /// by this dependency, and only by this dependency
  pub exclusive_deps_introduced: Vec<PackageId>,
  /// number of non-rust lines-of-code
  pub loc: u64,
  /// number of rust lines-of-code
  pub rust_loc: u64,
  /// number of lines of unsafe code
  pub unsafe_loc: u64,
  /// number of github stars, if any
  pub stargazers_count: Option<u64>,
  /// number of dependent crates on crates.io
  pub crates_io_dependent: Option<u64>,
}

//
// Helper
// ------
//

fn create_or_update_dependency(
  analysis_result: &mut HashMap<PackageId, PackageRisk>,
  dep_link: &DependencyLink,
) {
  match analysis_result.entry(dep_link.to.id().to_owned()) {
    Entry::Occupied(mut entry) => {
      let package_risk = entry.get_mut();
      package_risk
        .versions
        .insert(dep_link.to.version().to_string());
    }
    Entry::Vacant(entry) => {
      let mut package_risk = PackageRisk::default();
      package_risk.name = dep_link.to.name().to_owned();
      package_risk
        .versions
        .insert(dep_link.to.version().to_string());
      package_risk.repo = dep_link.to.repository().map(|x| x.to_owned());
      package_risk.description = dep_link.to.description().map(|x| x.to_owned());
      package_risk.manifest_path = dep_link.to.manifest_path().to_path_buf();
      entry.insert(package_risk);
    }
  };
}

/// Takes a `manifest_path` and produce an analysis stored in `analysis_result`.
///
/// Optionally, you can pass:
/// - `proxy`, a proxy (used to query github to fetch number of stars)
/// - `github_token`, a github personnal access token (PAT) used to query the github API
///   this is useful due to github limiting queries that are not authenticated.
/// - `to_ignore`, a list of direct dependencies to ignore.
///
/// Let's define some useful terms as well:
/// - **workspace packages** or **root crates**: crates that live in the workspace
///   (and not on crates.io for example)
/// - **direct dependency**: third-party dependencies (from crates.io for example)
///   that are imported from the root crates.
/// - **transitive dependencies**: third-party dependencies that end up getting imported
///   at some point. For example if A imports B and B imports C,
///   then C is a transitive dependency of A.
///
pub fn analyze_repo(
  manifest_path: &str,
  http_client: reqwest::blocking::Client,
  github_token: Option<(&str, &str)>,
  packages: Option<Vec<&str>>,
  to_ignore: Option<Vec<&str>>,
) -> Result<
  (
    HashSet<String>,                 // root_crates
    HashSet<PackageId>,              // main_dependencies
    HashMap<PackageId, PackageRisk>, // analysis_result
  ),
  String,
> {
  //
  // Obtain package graph via guppy
  // ------------------------------
  //

  // obtain metadata from manifest_path
  let mut cmd = MetadataCommand::new();
  cmd.manifest_path(manifest_path);

  // construct graph with guppy
  let package_graph = PackageGraph::from_command(&mut cmd).map_err(|err| err.to_string())?;

  // Obtain internal dependencies
  // ----------------------------
  // Either the sole main crate,
  // or every crate members of the workspace (if there is a workspace)
  //

  let root_crates = package_graph.workspace().member_ids().map(|x| x.clone());
  let root_crates: HashSet<PackageId> = HashSet::from_iter(root_crates);
  let mut root_crates_to_analyze: HashSet<PackageId> = root_crates.clone();
  // either select specific packages or remove ignored packages
  if let Some(packages) = packages {
    root_crates_to_analyze = root_crates_to_analyze
      .into_iter()
      .filter(|pkg_id| {
        let package_metadata = package_graph.metadata(pkg_id).unwrap();
        let package_name = package_metadata.name();
        packages.contains(&package_name)
      })
      .collect();
  } else if let Some(to_ignore) = to_ignore {
    root_crates_to_analyze = root_crates_to_analyze
      .into_iter()
      .filter(|pkg_id| {
        let package_metadata = package_graph.metadata(pkg_id).unwrap();
        let package_name = package_metadata.name();
        !to_ignore.contains(&package_name)
      })
      .collect();
  }

  if root_crates_to_analyze.len() == 0 {
    return Err("dephell: no package to analyze was found".to_string());
  }

  // What dependencies do we want to analyze?
  // ----------------------------------------
  //

  let mut analysis_result: HashMap<PackageId, PackageRisk> = HashMap::new();

  // TODO: combine the two loops and inline `create_or_update...`
  // find all direct dependencies
  let mut main_dependencies: HashSet<PackageId> = HashSet::new();
  for root_crate in &root_crates_to_analyze {
    // (non-ignored) root crate > direct dependency
    for dep_link in package_graph.dep_links(root_crate).unwrap() {
      // ignore dev dependencies
      if dep_link.edge.dev_only() {
        continue;
      }
      // ignore root crates (when used as dependency)
      if root_crates.contains(dep_link.to.id()) {
        continue;
      }
      main_dependencies.insert(dep_link.to.id().to_owned());
      create_or_update_dependency(&mut analysis_result, &dep_link);
    }
  }

  // find all transitive dependencies
  let transitive_dependencies = package_graph
    .select_forward(&main_dependencies)
    .unwrap()
    .into_iter_links(Some(DependencyDirection::Forward));
  // (non-ignored) root crate > direct dependency > transitive dependencies
  for dep_link in transitive_dependencies {
    // ignore dev dependencies
    if dep_link.edge.dev_only() {
      continue;
    }
    // ignore root crates (when used as dependency)
    if root_crates.contains(dep_link.to.id()) {
      continue;
    }
    create_or_update_dependency(&mut analysis_result, &dep_link);
  }

  //
  // Build the workspace/crate to obtain dep files
  // ---------------------------------------------
  //

  // TODO: `cargo build --message-format=json` probably has the hashes of the dep-info files
  // TODO: maybe we don't need to re-build in a different folder (optimization)
  let target_dir = TempDir::new("target_dir").expect("could not create temporary folder");
  let target_dir = target_dir.path();
  std::process::Command::new("cargo")
    .args(&[
      "build",
      "--manifest-path",
      manifest_path,
      "--target-dir",
      target_dir.to_str().unwrap(),
      "-q",
    ])
    .output()
    .expect("failed to build crate");

  // Analyze!
  // --------
  //

  for (package_id, mut package_risk) in analysis_result.iter_mut() {
    // .transitive_dependencies
    package_risk.transitive_dependencies = package_graph
      .select_forward(std::iter::once(package_id))
      .unwrap()
      .into_iter_links(Some(DependencyDirection::Reverse))
      .map(|package_id| package_id.to.id().clone())
      .collect();

    // .root_importers
    let root_importers =
      metrics::get_root_importers(&package_graph, &root_crates_to_analyze, package_id);
    package_risk.root_importers = root_importers;

    // .exclusive_deps_introduced
    let exclusive_deps_introduced =
      metrics::get_exclusive_deps(&package_graph, &root_crates_to_analyze, package_id);
    package_risk.exclusive_deps_introduced = exclusive_deps_introduced;

    // .in_host_target
    let (used, dependency_files) = metrics::get_dependency_files(
      &package_risk.name,
      package_risk.manifest_path.as_path(),
      &target_dir,
    );
    package_risk.used = used;

    /*
      println!(
        "files for dependency {}: {:#?}",
        package_risk.name, dependency_files
      );
    */

    // .loc + .rust_loc
    metrics::get_loc(&mut package_risk, &dependency_files);

    // .unsafe_loc
    metrics::get_unsafe(&mut package_risk, &dependency_files);

    // .stargazers_count
    // TODO: also retrieve latest SHA commit (of release)
    // TODO: also compare it to the hash to the repo we have (this signals a big problem)
    if let Some(repo) = &package_risk.repo {
      let stars = metrics::get_github_stars(http_client.clone(), github_token, &repo);
      package_risk.stargazers_count = stars;
    }

    // .cratesio_dependent
    let crates_io_dependent =
      metrics::get_dependent_published_crates(http_client.clone(), &package_risk.name);
    package_risk.crates_io_dependent = crates_io_dependent;
  }

  // PackageId -> name
  let root_crates_to_analyze: HashSet<String> = root_crates_to_analyze
    .iter()
    .map(|pkg_id| {
      let package_metadata = package_graph.metadata(pkg_id).unwrap();
      package_metadata.name().to_owned()
    })
    .collect();
  // TODO: do the same for main_dependencies and analysis_result

  //
  Ok((root_crates_to_analyze, main_dependencies, analysis_result))
}
